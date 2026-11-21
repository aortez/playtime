"""Correlated one-turn requests and cleanup receipts for the owner wrapper.

Tool cleanup requires the client's receipt; inference cleanup requires normal
owner release. An actor's missing evidence remains unresolved even after exit.
"""

import hashlib
import json
import os
import re
import stat
import sys
import tempfile
import time
from pathlib import Path

SCHEMA_VERSION = 1
PROTOCOL = "launcher_v1"
REQUEST_FLAG = "--launch-request"
DESCRIBE_FLAG = "--describe-launch"

MAX_REQUEST_BYTES = 64 * 1024
MAX_RECEIPT_BYTES = 64 * 1024
# One turn's prompt, matching the console's own one-shot limit.
MAX_PROMPT_BYTES = 256 * 1024
MIN_TIMEOUT_SECONDS = 1
MAX_TIMEOUT_SECONDS = 86_400

OUTCOMES = ("completed", "failed", "cancelled", "timed_out")
CLEANUP_STATES = ("confirmed", "not_acquired", "unresolved")
# The chat client can only speak for tools it actually held.
CLIENT_CLEANUP_STATES = ("confirmed", "unresolved")

REQUEST_KEYS = {"schema_version", "launch_id", "workspace", "prompt_file",
                "prompt_sha256", "receipt_file", "timeout_seconds"}
RECEIPT_KEYS = {"schema_version", "launch_id", "prompt_sha256", "outcome",
                "local_tools", "inference_owner", "session_id"}
CLIENT_KEYS = {"schema_version", "launch_id", "prompt_sha256", "outcome",
               "local_tools", "session_id"}

IDENTIFIER = re.compile(r"[A-Za-z0-9_-]{1,80}")
DIGEST = re.compile(r"[0-9a-f]{64}")

PRIVATE_DIR = "private"
PROMPT_NAME = "prompt.txt"
CLIENT_COMPLETION = "client-completion.json"

# The caller must keep waiting this long past the task deadline for the wrapper's
# own shutdown, or it stops watching before cleanup can be proven. Each term is a
# grace the wrapper really spends: the verifier and SSH child each fall back from
# terminate to kill, the client gets its full tool-cleanup grace, the heartbeat
# thread is joined, and the warm-session helper gets the preset's own budget.
VERIFIER_STOP_GRACE = 5
CLIENT_CLEANUP_GRACE = 45
HEARTBEAT_JOIN = 2
OWNER_STOP_GRACE = 5
KILL_GRACE = 5
CLEANUP_SLACK = 8
MAX_CLEANUP_BUDGET = 300


class LaunchError(Exception):
    """This launch cannot proceed, or its evidence cannot be trusted."""


def log(message):
    print(f"Playtime: {message}", file=sys.stderr, flush=True)


def cleanup_budget(preset):
    """Conservative seconds the wrapper needs after the task deadline to clean up."""
    return (VERIFIER_STOP_GRACE + KILL_GRACE + CLIENT_CLEANUP_GRACE + KILL_GRACE
            + HEARTBEAT_JOIN + int(preset["cleanup_timeout"])
            + OWNER_STOP_GRACE + KILL_GRACE + CLEANUP_SLACK)


def require_cleanup_budget(preset, profile):
    budget = cleanup_budget(preset)
    if budget > MAX_CLEANUP_BUDGET:
        raise LaunchError(f"profile {profile} needs {budget}s of cleanup, above the "
                          f"{MAX_CLEANUP_BUDGET}s this protocol allows")
    return budget


def describe(preset, profile):
    """Answer `--describe-launch` read-only: no SSH, no model, nothing acquired."""
    budget = require_cleanup_budget(preset, profile)
    print(json.dumps({"schema_version": SCHEMA_VERSION, "protocol": PROTOCOL,
                      "max_prompt_bytes": MAX_PROMPT_BYTES,
                      "cleanup_timeout_seconds": budget}))
    return 0


def read_bounded(path, limit, what):
    """Read a bounded regular file, refusing a symlink rather than following it."""
    try:
        before = os.lstat(path)
    except OSError as error:
        raise LaunchError(f"{what} {path}: {error.strerror or error}")
    if stat.S_ISLNK(before.st_mode):
        raise LaunchError(f"{what} {path} is a symlink; launcher evidence must be the file itself")
    if not stat.S_ISREG(before.st_mode):
        raise LaunchError(f"{what} {path} is not a regular file")
    if before.st_size > limit:
        raise LaunchError(f"{what} {path} exceeds {limit} bytes")
    try:
        # O_NOFOLLOW covers the final component and O_NONBLOCK keeps a replaced
        # fifo from hanging the launch during preflight.
        descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    except OSError as error:
        raise LaunchError(f"{what} {path}: {error.strerror or error}")
    with os.fdopen(descriptor, "rb") as source:
        if not stat.S_ISREG(os.fstat(source.fileno()).st_mode):
            raise LaunchError(f"{what} {path} is not a regular file")
        raw = source.read(limit + 1)
    if len(raw) > limit:
        raise LaunchError(f"{what} {path} exceeds {limit} bytes")
    return raw


def unique_keys(pairs):
    value = {}
    for key, item in pairs:
        if key in value:
            raise ValueError(f"duplicate JSON key: {key}")
        value[key] = item
    return value


def sync_directory(path):
    descriptor = os.open(path, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def read_object(path, limit, what):
    raw = read_bounded(path, limit, what)
    try:
        value = json.loads(raw, object_pairs_hook=unique_keys)
    except (UnicodeDecodeError, ValueError) as error:
        raise LaunchError(f"{what} {path} is not well-formed JSON: {error}")
    if not isinstance(value, dict):
        raise LaunchError(f"{what} {path} must hold one JSON object")
    return value


def exact_keys(value, expected, what):
    missing, unknown = expected - set(value), set(value) - expected
    if missing:
        raise LaunchError(f"{what} is missing {', '.join(sorted(missing))}")
    if unknown:
        raise LaunchError(f"{what} has unsupported keys: {', '.join(sorted(unknown))}")


def absolute(value, field):
    if not isinstance(value, str) or not value.startswith("/"):
        raise LaunchError(f"launch request {field} must be an absolute path")
    # Refuse what the filesystem calls would reject only after the claim exists.
    if "\0" in value:
        raise LaunchError(f"launch request {field} contains a NUL character")
    try:
        value.encode("utf-8")
    except UnicodeEncodeError:
        raise LaunchError(f"launch request {field} is not valid Unicode text") from None
    return Path(value)


class Request:
    """One validated `launcher_v1` request plus the exact prompt bytes it names."""

    def __init__(self, path, value):
        self.path = path
        if type(value["schema_version"]) is not int or value["schema_version"] != SCHEMA_VERSION:
            raise LaunchError(f"launch request {path} uses schema version "
                              f"{value['schema_version']!r}; this launcher reads version {SCHEMA_VERSION}")
        self.launch_id = value["launch_id"]
        if not isinstance(self.launch_id, str) or not IDENTIFIER.fullmatch(self.launch_id):
            raise LaunchError(f"launch id {self.launch_id!r} must use letters, digits, '-' or '_' "
                              "(1 to 80 characters) and is never a path or profile name")
        self.workspace = absolute(value["workspace"], "workspace")
        self.prompt_file = absolute(value["prompt_file"], "prompt_file")
        self.receipt_file = absolute(value["receipt_file"], "receipt_file")
        self.prompt_sha256 = value["prompt_sha256"]
        if not isinstance(self.prompt_sha256, str) or not DIGEST.fullmatch(self.prompt_sha256):
            raise LaunchError(f"prompt hash {self.prompt_sha256!r} is not a lowercase 64-digit "
                              f"sha256 hex digest")
        self.timeout_seconds = value["timeout_seconds"]
        if isinstance(self.timeout_seconds, bool) or not isinstance(self.timeout_seconds, int) \
                or not MIN_TIMEOUT_SECONDS <= self.timeout_seconds <= MAX_TIMEOUT_SECONDS:
            raise LaunchError(f"launch timeout {self.timeout_seconds!r} must be a whole number of "
                              f"seconds in {MIN_TIMEOUT_SECONDS}..={MAX_TIMEOUT_SECONDS}")
        self.prompt_bytes = b""

    def check_inputs(self):
        """Read once so later caller edits cannot change the prompt delivered."""
        if not self.workspace.is_dir():
            raise LaunchError(f"launch workspace {self.workspace} is not an existing directory")
        raw = read_bounded(self.prompt_file, MAX_PROMPT_BYTES, "launch prompt")
        try:
            text = raw.decode("utf-8")
        except UnicodeDecodeError as error:
            raise LaunchError(f"launch prompt {self.prompt_file} is not valid UTF-8: {error}")
        if not text.strip():
            raise LaunchError(f"launch prompt {self.prompt_file} is blank")
        actual = hashlib.sha256(raw).hexdigest()
        if actual != self.prompt_sha256:
            raise LaunchError(f"launch prompt {self.prompt_file} hashes to {actual}, not the "
                              f"requested {self.prompt_sha256}; the frozen prompt changed")
        self.prompt_bytes = raw

    def check_receipt_slot(self):
        """Require an absent receipt in a private caller-owned directory."""
        parent = self.receipt_file.parent
        try:
            info = parent.lstat()
        except OSError as error:
            raise LaunchError(f"launch receipt parent {parent} is unavailable: {error.strerror or error}")
        if not stat.S_ISDIR(info.st_mode):
            raise LaunchError(f"launch receipt parent {parent} is not a directory")
        if info.st_uid != os.getuid():
            raise LaunchError(f"launch receipt parent {parent} must be owned by the calling user")
        if info.st_mode & 0o077:
            raise LaunchError(f"launch receipt parent {parent} must be private, with no group or other access")
        if self.receipt_file.is_symlink() or self.receipt_file.exists():
            raise LaunchError(f"launch receipt {self.receipt_file} already exists; a launch is never rerun over it")

    def claim(self):
        # Retain this claim even on failure: absence of a receipt cannot prove
        # that a previous process stopped before dispatch.
        path = self.receipt_file.with_name(self.receipt_file.name + ".started")
        try:
            descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
        except FileExistsError:
            raise LaunchError(f"launch claim {path} already exists; inspect that attempt instead of replaying it")
        with os.fdopen(descriptor, "wb") as sink:
            sink.write((json.dumps({"schema_version": SCHEMA_VERSION, "launch_id": self.launch_id,
                                   "prompt_sha256": self.prompt_sha256}) + "\n").encode())
            sink.flush()
            os.fsync(sink.fileno())
        sync_directory(path.parent)



class Launch:
    """Mutable state for one accepted request, from preflight to published receipt."""

    def __init__(self, request, preset, profile):
        self.request = request
        self.preset = preset
        self.profile = profile
        # One monotonic deadline covers startup, verification and the client turn;
        # cleanup uses the separate budget advertised by describe().
        self.deadline = time.monotonic() + request.timeout_seconds
        self.timed_out = False
        self.inference_acquired = self.tools_acquired = False
        self.evidence = self.prompt_copy = self.completion_file = None
        self.published = None

    def check(self):
        """Health hook the wrapper polls; stops everything through one deadline."""
        if time.monotonic() >= self.deadline:
            self.timed_out = True
            raise TimeoutError(f"launch timeout of {self.request.timeout_seconds}s exceeded")

    def chat_arguments(self):
        """Select host tools without adding model settings or session recovery."""
        return ["--preset", "coding-host", "--json", "--workspace", str(self.request.workspace)]

    def freeze(self, evidence):
        """Copy the frozen prompt into this run's private evidence before any start."""
        self.evidence = Path(evidence)
        private = self.evidence / PRIVATE_DIR
        private.mkdir(mode=0o700)
        private.chmod(0o700)
        self.completion_file = private / CLIENT_COMPLETION
        self.prompt_copy = self.evidence / PROMPT_NAME
        with self.prompt_copy.open("xb") as sink:
            sink.write(self.request.prompt_bytes)
            sink.flush()
            os.fsync(sink.fileno())
        return self.prompt_copy

    def attach(self, args):
        """Extend the chat arguments with the frozen prompt and the receipt request."""
        if self.prompt_copy is None:
            raise LaunchError("launch prompt was frozen after the client was started")
        remaining = max(1, int(self.deadline - time.monotonic()))
        return [*args, "--prompt-file", str(self.prompt_copy),
                "--completion-receipt", str(self.completion_file),
                "--launch-id", self.request.launch_id,
                "--wall-seconds", str(remaining)]

    def note(self, kind):
        """Record that an actor really started, so `not_acquired` stays honest."""
        if kind == "inference":
            self.inference_acquired = True
        elif kind == "tools":
            self.tools_acquired = True
        else:
            raise LaunchError(f"unknown launch actor {kind}")

    def read_completion(self):
        """The chat client's own statement, or None plus the reason it is unusable.

        A missing, unreadable, unknown-version, foreign or truncated completion is
        never a default of "clean": the client's exit code is not evidence.
        """
        if self.completion_file is None:
            return None, "the client was never started"
        try:
            value = read_object(self.completion_file, MAX_RECEIPT_BYTES, "client completion receipt")
            exact_keys(value, CLIENT_KEYS, f"client completion receipt {self.completion_file}")
            if type(value["schema_version"]) is not int or value["schema_version"] != SCHEMA_VERSION:
                raise LaunchError(f"client completion receipt uses schema version {value['schema_version']!r}")
            if value["launch_id"] != self.request.launch_id:
                raise LaunchError(f"client completion receipt reports launch id {value['launch_id']!r}, "
                                  f"not this attempt's {self.request.launch_id!r}")
            if value["prompt_sha256"] != self.request.prompt_sha256:
                raise LaunchError(f"client completion receipt reports prompt hash "
                                  f"{value['prompt_sha256']!r}, not the {self.request.prompt_sha256} "
                                  f"this attempt sent")
            if value["outcome"] not in OUTCOMES:
                raise LaunchError(f"client completion receipt reports unknown outcome {value['outcome']!r}")
            if value["local_tools"] not in CLIENT_CLEANUP_STATES:
                raise LaunchError(f"client completion receipt reports unknown tool state "
                                  f"{value['local_tools']!r}")
            if not (value["session_id"] is None or isinstance(value["session_id"], str)):
                raise LaunchError("client completion receipt session id must be a string or null")
        except LaunchError as error:
            return None, str(error)
        return value, None

    def outcome(self, child, completion, signals):
        if signals:
            return "cancelled"
        if self.timed_out:
            return "timed_out"
        if completion is None:
            return "failed"
        if completion["outcome"] in ("cancelled", "timed_out"):
            return completion["outcome"]
        # A turn only counts as completed when the client itself said so and the
        # process was reaped normally; anything else stays failed.
        if completion["outcome"] == "completed" and child is not None and child.returncode == 0:
            return "completed"
        return "failed"

    def local_tools(self, child, completion):
        if not self.tools_acquired:
            return "not_acquired"
        if completion is None or completion["local_tools"] != "confirmed":
            return "unresolved"
        # Confirmed needs the client's own claim and a process this wrapper reaped
        # normally. A child killed by a signal never confirms, however it claimed.
        if child is None or child.returncode is None or child.returncode < 0:
            return "unresolved"
        return "confirmed"

    def inference_owner(self, owner_code, transport_failed):
        if not self.inference_acquired:
            return "not_acquired"
        # Only the helper's own normal exit proves the model was released or parked.
        # A forced kill, an expired budget or a lost transport stays unknown.
        if transport_failed or owner_code != 0:
            return "unresolved"
        return "confirmed"

    def publish(self, outcome, local_tools, inference_owner, session_id):
        """Create the caller's receipt once, durably, and never overwrite it."""
        if self.published is not None:
            return self.published
        receipt = {"schema_version": SCHEMA_VERSION, "launch_id": self.request.launch_id,
                   "prompt_sha256": self.request.prompt_sha256, "outcome": outcome,
                   "local_tools": local_tools, "inference_owner": inference_owner,
                   "session_id": session_id}
        if set(receipt) != RECEIPT_KEYS or receipt["outcome"] not in OUTCOMES \
                or receipt["local_tools"] not in CLEANUP_STATES \
                or receipt["inference_owner"] not in CLEANUP_STATES:
            raise LaunchError("internal launch receipt shape or vocabulary changed")
        path = self.request.receipt_file
        descriptor, temporary = tempfile.mkstemp(prefix=f".{path.name}.", dir=path.parent)
        try:
            with os.fdopen(descriptor, "wb") as sink:
                sink.write((json.dumps(receipt) + "\n").encode())
                sink.flush()
                os.fsync(sink.fileno())
            # Publish complete bytes without replacing another writer's receipt.
            os.link(temporary, path)
            os.unlink(temporary)
            sync_directory(path.parent)
        finally:
            if os.path.exists(temporary):
                os.unlink(temporary)
        self.published = receipt
        return receipt

    def finish(self, child, owner_code, signals, transport_failed):
        """Publish after cleanup attempts, retaining uncertainty for each actor."""
        try:
            completion, reason = self.read_completion()
            if reason:
                log(f"client completion was not usable: {reason}")
            outcome = self.outcome(child, completion, signals)
            tools = self.local_tools(child, completion)
            owner = self.inference_owner(owner_code, transport_failed)
            session = None if completion is None else completion["session_id"]
            self.publish(outcome, tools, owner, session)
            # Only acknowledged after the file and its directory are both fsynced.
            log(f"launch receipt {self.request.receipt_file} for {self.profile}: {outcome} "
                f"local_tools={tools} inference_owner={owner}")
            return self.published
        except Exception as error:
            log(f"launch receipt could not be published: {error}")
            return None

    def abort(self, error, signals=()):
        """Report an exception or signal that ended the launch before its own cleanup ran."""
        if self.published is not None:
            return self.published
        outcome = "cancelled" if signals else "timed_out" if self.timed_out else "failed"
        tools = "not_acquired" if not self.tools_acquired else "unresolved"
        owner = "not_acquired" if not self.inference_acquired else "unresolved"
        try:
            receipt = self.publish(outcome, tools, owner, None)
            log(f"launch receipt {self.request.receipt_file}: {outcome} "
                f"local_tools={tools} inference_owner={owner} ({error})")
            return receipt
        except Exception as failure:
            log(f"launch receipt could not be published: {failure}")
            return None

    def succeeded(self):
        return (self.published is not None and self.published["outcome"] == "completed"
                and self.published["local_tools"] in ("confirmed", "not_acquired")
                and self.published["inference_owner"] in ("confirmed", "not_acquired"))


def begin(preset, profile, request_path):
    """Validate and claim a request before acquiring any execution resource."""
    require_cleanup_budget(preset, profile)
    path = Path(request_path)
    if not path.is_absolute():
        # A relative name would resolve against whatever directory the launcher
        # happens to change into, so the caller's request stops being identifiable.
        raise LaunchError(f"launch request {path} must be an absolute path")
    value = read_object(path, MAX_REQUEST_BYTES, "launch request")
    exact_keys(value, REQUEST_KEYS, f"launch request {path}")
    request = Request(path, value)
    launch = Launch(request, preset, profile)
    request.check_receipt_slot()
    request.claim()
    try:
        request.check_inputs()
    except Exception:
        # The receipt slot is usable, so say why this attempt is not.
        # Any failure counts: once the claim exists, every exit needs a receipt.
        try:
            launch.publish("failed", "not_acquired", "not_acquired", None)
        except Exception as failure:
            log(f"launch receipt could not be published: {failure}")
        raise
    return launch
