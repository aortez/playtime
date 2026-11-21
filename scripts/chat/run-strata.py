#!/usr/bin/env python3
"""Own a reviewed remote Strata session, verify it, and run installed Playtime chat.

Configuration and locks sit next to this program, outside the selected project.
Status checks only invoke the remote helper's read-only status command.
"""

import json
import os
from pathlib import Path
import queue
import re
import shlex
import signal
import socket
import stat
import subprocess
import sys
import tempfile
import threading
import time

PACKAGE = Path(__file__).resolve().parent
STATUS_TIMEOUT = 8
VERIFY_TIMEOUT = 600
CLIENT_CLEANUP_TIMEOUT = 45


def log(message):
    print(f"Playtime: {message}", file=sys.stderr, flush=True)


def ssh(preset):
    return ["ssh", *[os.path.expanduser(arg) for arg in preset["ssh_options"]], "-T",
            "-o", "BatchMode=yes", "-o", "StrictHostKeyChecking=yes", "-o", "ConnectTimeout=5",
            "-o", "ServerAliveInterval=15", "-o", "ServerAliveCountMax=3"]


def remote(preset, *args):
    return [preset["host"], shlex.join(args)]


def status(preset, profile):
    try:
        result = subprocess.run(ssh(preset) + remote(preset, "python3", preset["helper"], "--status", profile),
                                stdin=subprocess.DEVNULL, capture_output=True, timeout=STATUS_TIMEOUT)
        if result.returncode:
            report = {"state": "down" if result.returncode == 255 else "unknown",
                      "message": "remote helper status failed"}
        else:
            report = json.loads(result.stdout)
            if not isinstance(report, dict) or "state" not in report:
                raise ValueError("invalid helper status")
    except (OSError, ValueError, subprocess.TimeoutExpired) as error:
        report = {"state": "unknown", "message": str(error)}
    print(json.dumps(report))
    return 0


def private_run_directory():
    root = Path.home() / ".local/state/playtime-launcher"
    root.mkdir(mode=0o700, parents=True, exist_ok=True)
    meta = root.lstat()
    if not stat.S_ISDIR(meta.st_mode) or meta.st_uid != os.getuid() or meta.st_mode & 0o077:
        raise RuntimeError(f"launcher state must be a private, caller-owned directory: {root}")
    return Path(tempfile.mkdtemp(prefix=time.strftime("%Y%m%dT%H%M%S-") , dir=root))


def stop_process(process, grace):
    if process is not None and process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=grace)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=5)


def check_ready(ready, lock):
    if (ready.get("model") != lock["identity"]["model"]
            or ready.get("context") != lock["identity"]["context_tokens"]
            or ready.get("drafting") != "dflash2"):
        raise RuntimeError("helper returned a different model, context or drafting preset")


def check_host(report, lock):
    if report.get("host") != lock["host"]:
        raise RuntimeError("running host artifacts/settings differ from the reviewed model lock; client not started")


def worker_arguments(args):
    """Resolve worker session metadata without treating an opaque ID as a path."""
    allowed = {"--workspace", "--run-dir", "--request", "--state-dir", "--worker-session",
               "--protocol-version", "--wall-seconds"}
    values = {}
    index = 0
    while index < len(args):
        name, separator, value = args[index].partition("=")
        if name not in allowed or name in values:
            raise RuntimeError("unsupported or duplicate worker argument")
        if not separator:
            index += 1
            if index == len(args):
                raise RuntimeError("missing worker argument value")
            value = args[index]
        values[name] = value
        index += 1
    if any(name not in values for name in ("--workspace", "--run-dir", "--request")):
        raise RuntimeError("worker needs --workspace, --run-dir and --request")
    if values.get("--protocol-version", "1") != "1":
        raise RuntimeError("unsupported worker protocol")
    root = Path(values["--workspace"]).resolve(strict=True)
    for name in ("--run-dir", "--request"):
        if Path(values[name]).resolve(strict=True).is_relative_to(root):
            raise RuntimeError("worker records and request must stay outside the project")
    selected = ["--workspace", str(root)]
    if "--worker-session" in values:
        session = values["--worker-session"]
        if not re.fullmatch(r"ptw-[0-9a-f]{64}", session):
            raise RuntimeError("invalid Playtime worker session ID")
        state = Path(values.get("--state-dir", str(Path.home() / ".local/state/playtime-chat")))
        source = state / session
        if not (source / "config.json").is_file():
            raise RuntimeError("saved worker session unavailable; no automatic fresh fallback")
        selected += ["--session", str(source)]
    # Devbot sends the assignment only after its process-start receipt is durable.
    # No SSH ownership or model activity is allowed before that release gate.
    request = Path(values["--request"])
    descriptor = os.open(request, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(descriptor, "rb") as source:
        metadata = os.fstat(source.fileno())
        if not stat.S_ISREG(metadata.st_mode) or metadata.st_uid != os.getuid() or metadata.st_mode & 0o077:
            raise RuntimeError("worker request must be a private owned regular file")
        frozen = source.read(256 * 1024 + 1)
    delivered = sys.stdin.buffer.read(256 * 1024 + 1)
    if not frozen or len(frozen) > 256 * 1024 or delivered != frozen:
        raise RuntimeError("worker startup gate differs from the frozen assignment")
    return selected


def session(preset, profile, args):
    # Wrapper flags precede chat arguments, so a prompt whose text is literally
    # "--full-verify" remains an ordinary prompt value.
    import project_environment

    worker = args[:1] == ["--worker"]
    client_environment = dict(os.environ)
    if worker:
        # Authentication belongs to the SSH owner, not the local harness child.
        client_environment.pop("SSH_AUTH_SOCK", None)
        args = args[1:]
    full_verify = force_picker = False
    environment = None
    while args:
        if args[0] in ("--full-verify", "--sessions"):
            full_verify |= args[0] == "--full-verify"
            force_picker |= args[0] == "--sessions"
            args = args[1:]
        elif args[0] == "--environment":
            if len(args) < 2 or environment is not None:
                raise RuntimeError("--environment needs one selection")
            environment, args = args[1], args[2:]
        elif args[0] == "--prepare-environment":
            if full_verify or force_picker or environment is not None:
                raise RuntimeError("environment preparation accepts only --workspace")
            return subprocess.call([sys.executable, str(PACKAGE / "project_environment.py"), *args[1:]])
        else:
            break
    for arg in args:
        if arg.split("=", 1)[0] in ("--endpoint", "--model-lock", "--image-lock"):
            raise RuntimeError(f"{arg.split('=', 1)[0]} is owned by this preset")
    executable = PACKAGE / ("playtime-task" if worker else "playtime-chat")
    lock_path = PACKAGE / preset["model_lock"]
    image_path = PACKAGE / "general.lock.json"
    # Read local resources before touching the remote host.
    lock = json.loads(lock_path.read_text())
    json.loads(image_path.read_text())
    if not executable.is_file() or not os.access(executable, os.X_OK):
        raise RuntimeError("playtime-chat is not installed beside this wrapper")
    if args in (["--help"], ["-h"], ["--version"], ["-V"]) or (worker and args == ["--describe-protocol"]):
        if args in (["--help"], ["-h"]):
            print("Launcher options (before chat arguments): --full-verify rehashes model files; "
                  "--sessions opens the local session picker. --stop-server releases an idle warm model. "
                  "--prepare-environment builds the project devcontainer; --environment general|LOCK selects an environment. "
                  "Interactive launches pick by default.", flush=True)
        return subprocess.call([str(executable), *args], env=client_environment)
    if args == ["--stop-server"]:
        return subprocess.call(ssh(preset) + remote(preset, "python3", preset["helper"], "--stop"), stdin=subprocess.DEVNULL)
    selection_args = worker_arguments(args) if worker else args
    if worker:
        if force_picker:
            raise RuntimeError("workers cannot use the session picker")
        described = subprocess.run([str(executable), "--describe-protocol"], check=True,
                                   capture_output=True, text=True, timeout=5, env=client_environment)
        protocol = json.loads(described.stdout)
        if protocol.get("protocol") != "playtime-worker" or protocol.get("version") != 1:
            raise RuntimeError("unsupported Playtime worker protocol")
    image_path = project_environment.resolve(selection_args, PACKAGE, environment)
    json.loads(image_path.read_text())
    client_args = [str(executable), "--model-lock", str(lock_path), "--image-lock", str(image_path)]
    if worker:
        checked = subprocess.run([*client_args, "--check-inputs", *args], stdin=subprocess.DEVNULL,
                                 capture_output=True, text=True, timeout=10, env=client_environment)
        if checked.returncode:
            raise RuntimeError(checked.stderr.strip() or "worker input validation failed")
    options = {arg.split("=", 1)[0] for arg in args}
    if options & {"--select-session", "--list-sessions", "--inspect-interrupted"} and not options & {"--prompt", "--session", "--recover-compaction", "--recover-interrupted"}:
        return subprocess.call([*client_args, *args])
    explicit = worker or bool(options & {"--session", "--recover-compaction", "--recover-interrupted", "--prompt"})
    if force_picker and explicit:
        raise RuntimeError("--sessions cannot be combined with --session, recovery options or --prompt")
    if force_picker or (sys.stdin.isatty() and not explicit):
        try:
            picked = subprocess.run([*client_args, "--select-session", *args],
                                    stdout=subprocess.PIPE, text=True, check=True)
        except KeyboardInterrupt:
            return 130
        choice = json.loads(picked.stdout)
        if choice["cancelled"]:
            return 0
        if choice["session"] is not None:
            args = [*args, "--session", choice["session"]]
        if choice.get("recover_interrupted") is not None:
            args = [*args, "--recover-interrupted", choice["recover_interrupted"]]
            if choice.get("acknowledge_uncertain_tools"):
                args.append("--acknowledge-uncertain-tools")
    # Startup timings exclude time spent choosing a conversation.
    started_at = time.monotonic()
    verifier = (PACKAGE / "verify-strata-host.py").read_bytes()
    evidence = private_run_directory()
    log(f"session evidence: {evidence}")
    with socket.socket() as reservation:
        reservation.bind(("127.0.0.1", 0))
        port = reservation.getsockname()[1]
    command = ssh(preset) + ["-o", "ExitOnForwardFailure=yes", "-L", f"127.0.0.1:{port}:127.0.0.1:8080"]
    command += remote(preset, "python3", preset["helper"], profile)
    owner = child = verification = heartbeats = None
    events = queue.Queue(maxsize=256)
    stopped = threading.Event()
    failed = threading.Event()
    signals = []
    previous = {}
    result = 1
    owner_code = None

    def receive(stream, channel):
        try:
            while line := stream.readline(16385):
                if len(line) > 16384:
                    raise RuntimeError("oversized helper output")
                events.put_nowait((channel, line))
        except (OSError, ValueError, RuntimeError, queue.Full):
            failed.set()

    def heartbeat():
        try:
            while not stopped.is_set():
                owner.stdin.write(".\n")
                owner.stdin.flush()
                stopped.wait(10)
        except (OSError, ValueError):
            failed.set()

    def interrupted(signum, _frame):
        signals.append(signum)

    def healthy():
        if signals:
            raise InterruptedError(f"received signal {signals[0]}")
        if failed.is_set() or owner.poll() is not None:
            raise RuntimeError("lost the inference host owner")

    def drain():
        nonlocal ready, state_dir
        while True:
            try:
                channel, line = events.get_nowait()
            except queue.Empty:
                return
            transcript.write(f"{channel}: {line}")
            transcript.flush()
            if channel == "stderr":
                print(line, end="", file=sys.stderr, flush=True)
                prefixes = [f"Gyro-M: {verb} {profile}; logs: " for verb in ("starting", "reusing")]
                prefix = next((p for p in prefixes if line.startswith(p)), None)
                if prefix:
                    state_dir = line[len(prefix):].strip()
                    path = Path(state_dir)
                    if not path.is_absolute() or not re.fullmatch(r"\d{8}T\d{6}Z-\d+", path.name):
                        raise RuntimeError("unrecognized helper session path")
                    if not str(path.parent).endswith("/" + preset["state_parent"]):
                        raise RuntimeError("unexpected helper state directory")
            elif line.startswith("GYRO_READY "):
                if ready is not None:
                    raise RuntimeError("duplicate helper readiness report")
                ready = json.loads(line[len("GYRO_READY "):])

    ready = state_dir = None
    transcript = (evidence / "owner.log").open("x")
    try:
        for sig in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
            previous[sig] = signal.signal(sig, interrupted)
        owner = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                 stderr=subprocess.PIPE, text=True, start_new_session=True)
        for channel in ("stdout", "stderr"):
            threading.Thread(target=receive, args=(getattr(owner, channel), channel), daemon=True).start()
        heartbeats = threading.Thread(target=heartbeat, daemon=True)
        heartbeats.start()
        deadline = time.monotonic() + preset["startup_timeout"]
        while ready is None or state_dir is None:
            drain()
            healthy()
            if time.monotonic() >= deadline:
                raise TimeoutError("owned model startup deadline exceeded")
            time.sleep(.1)
        check_ready(ready, lock)
        model_ready_seconds = time.monotonic() - started_at
        mode = "full hashing" if full_verify else "reuse hashes of unchanged files"
        log(f"model ready in {model_ready_seconds:.1f}s; verifying artifacts ({mode})…")
        start = time.monotonic()
        with (evidence / "verify-input.py").open("xb") as source:
            source.write(verifier)
        with (evidence / "verify-input.py").open("rb") as source, (evidence / "host.json").open("xb") as report, (evidence / "verify.log").open("xb") as errors:
            verify_args = ["python3", "-", state_dir, "--expected-host", json.dumps(lock["host"])]
            if full_verify:
                verify_args.append("--full-verify")
            verification = subprocess.Popen(ssh(preset) + remote(preset, *verify_args),
                                            stdin=source, stdout=report, stderr=errors, start_new_session=True)
            while verification.poll() is None:
                drain()
                healthy()
                if time.monotonic() - start > VERIFY_TIMEOUT:
                    raise TimeoutError("host verification deadline exceeded")
                time.sleep(.1)
            if verification.returncode:
                raise RuntimeError(f"host verification failed; see {evidence / 'verify.log'}")
        report = json.loads((evidence / "host.json").read_text())
        check_host(report, lock)
        healthy()
        verification_seconds = time.monotonic() - start
        stats = report["verification"]
        timings = {"model_ready_seconds": model_ready_seconds,
                   "verification_seconds": verification_seconds,
                   "launcher_ready_seconds": time.monotonic() - started_at,
                   "verification": stats}
        (evidence / "timings.json").write_text(json.dumps(timings, indent=2) + "\n")
        log(f"verified in {verification_seconds:.1f}s; {stats['reused_files']} cached, "
            f"{stats['hashed_files']} hashed ({stats['hashed_bytes'] / 1e9:.2f} GB read); "
            f"starting {profile} {'worker' if worker else 'chat'} in {project_environment.workspace_arguments(selection_args)[0]}")
        child = subprocess.Popen([str(executable), "--endpoint", f"http://127.0.0.1:{port}/v1",
                                  "--model-lock", str(lock_path), "--image-lock", str(image_path), *args],
                                 stdin=subprocess.DEVNULL if worker else None, env=client_environment)
        while child.poll() is None:
            drain()
            healthy()
            time.sleep(.1)
        result = child.returncode if child.returncode >= 0 else 128 - child.returncode
    except InterruptedError:
        result = 128 + signals[0]
        log("stopping chat and restoring the inference host…")
    except Exception as error:
        log(str(error))
    finally:
        # Keep the owner heartbeats alive until local tool cleanup has had its
        # full grace period. Only successful, confirmed cleanup permits parking;
        # every other EOF asks the owner to shut the model down.
        for sig in previous:
            signal.signal(sig, signal.SIG_IGN)
        for process, grace in ((verification, 5), (child, CLIENT_CLEANUP_TIMEOUT)):
            try:
                stop_process(process, grace)
            except (OSError, subprocess.TimeoutExpired) as error:
                log(f"local child cleanup was not confirmed: {error}")
                result = 1
        stopped.set()
        if heartbeats is not None:
            heartbeats.join(timeout=2)
            if heartbeats.is_alive():
                result = 1
        if owner is not None:
            try:
                if result == 0 and child is not None and child.returncode == 0 and not failed.is_set() and not signals:
                    owner.stdin.write("GYRO_RELEASE\n")
                    owner.stdin.flush()
                owner.stdin.close()
            except (OSError, ValueError):
                pass
            deadline = time.monotonic() + preset["cleanup_timeout"]
            while owner.poll() is None and time.monotonic() < deadline:
                try:
                    drain()
                except Exception:
                    pass
                time.sleep(.1)
            if owner.poll() is None:
                log("host restoration was not confirmed; inspect the helper logs before another launch")
                try:
                    stop_process(owner, 5)
                except (OSError, subprocess.TimeoutExpired) as error:
                    log(f"SSH cleanup was not confirmed: {error}")
                result = 1
            owner_code = owner.returncode
            if owner_code != 0:
                result = result or 1
            try:
                drain()
            except Exception:
                pass
        transcript.close()
        (evidence / "exit.json").write_text(json.dumps({"client_exit": None if child is None else child.returncode,
            "owner_exit": owner_code, "exit": result}) + "\n")
        for sig, handler in previous.items():
            signal.signal(sig, handler)
    return result


def main(args):
    report = args[:1] == ["--status"]
    if report:
        args = args[1:]
    config = json.loads((PACKAGE / "presets.json").read_text())
    if config.get("schema_version") != 1:
        raise RuntimeError("unsupported preset configuration")
    if not args or args[0] not in config["profiles"]:
        raise RuntimeError("use run-strata.py [--status] " + "|".join(config["profiles"]) + " [CHAT ARGS]")
    profile, extra = args[0], args[1:]
    if report and extra:
        raise RuntimeError("status accepts only a profile")
    preset = config["profiles"][profile]
    return status(preset, profile) if report else session(preset, profile, extra)


if __name__ == "__main__":
    os.umask(0o077)
    try:
        sys.exit(main(sys.argv[1:]))
    except Exception as error:
        log(str(error))
        sys.exit(1)
