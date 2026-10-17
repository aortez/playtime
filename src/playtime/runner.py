import copy
import json
import platform
import shutil
import sys
import time
import uuid
from pathlib import Path

from .common import (
    EventLog,
    PlaytimeError,
    StopRun,
    canonical,
    command,
    controller_manifest,
    digest_json,
    timestamp,
    tree_manifest,
    write_json,
)
from .config import repo_path
from .environment import check_image
from .ollama import Ollama
from .sandbox import Sandbox, extract_regular_files, initial_archive
from .tools import TOOLS, validate_call


class SessionFull(PlaytimeError):
    pass


def read_locks(root, recipe):
    try:
        image = json.loads(repo_path(root, recipe["workshop"]["lock"]).read_text())
        model = json.loads(repo_path(root, recipe["model"]["lock"]).read_text())
    except FileNotFoundError as error:
        raise PlaytimeError("Missing lock file; run prepare first") from error
    return image, model


def check(root, recipe, *, live=True, image=None, model=None, sources=True):
    if sources:
        for name in ["prompt", "protocol"]:
            if not repo_path(root, recipe[name]).is_file():
                raise PlaytimeError(f"Missing {name} file")
        if recipe["seeds"] and not repo_path(root, recipe["seeds"]).is_dir():
            raise PlaytimeError("Seed directory does not exist")
    if image is None or model is None:
        image, model = read_locks(root, recipe)
    check_image(root, recipe, image)
    if (
        sources
        and digest_json(tree_manifest(Path(root) / "workshop")) != image["build_context_sha256"]
    ):
        raise PlaytimeError(
            "Workshop source differs from its lock; run prepare --refresh deliberately"
        )
    if model["name"] != recipe["model"]["name"]:
        raise PlaytimeError("Model name differs from its lock")
    if live:
        Ollama(recipe["model"]["endpoint"]).check(model)
    return image, model


def archive_controller(root, destination):
    destination.mkdir()
    for name in ["src", "workshop"]:
        shutil.copytree(
            Path(root) / name, destination / name, ignore=shutil.ignore_patterns("__pycache__")
        )
    for name in ["playtime", "pyproject.toml"]:
        shutil.copy2(Path(root) / name, destination / name)


def run(root, recipe, *, scripted=False, smoke=False, previous=None, runs_dir=None):
    root = Path(root).resolve()
    source_root = Path(__file__).resolve().parents[2]
    recipe = copy.deepcopy(recipe)
    if previous:
        previous = Path(previous).resolve()
        old = json.loads((previous / "manifest.json").read_text())
        if old["inputs"] != tree_manifest(previous / "inputs"):
            raise PlaytimeError(
                "Archived inputs have changed; refusing to repeat altered starting conditions"
            )
        if old["controller_sha256"] != digest_json(controller_manifest(source_root)):
            raise PlaytimeError(
                "Controller has changed. Use the archived controller with --root pointing to this project to repeat this run"
            )
        image, model = check(
            root, recipe, live=not scripted, image=old["image"], model=old["model"], sources=False
        )
    else:
        image, model = check(root, recipe, live=not scripted)
    run_id = time.strftime("%Y%m%dT%H%M%SZ", time.gmtime()) + "-" + uuid.uuid4().hex[:8]
    directory = Path(runs_dir or root / "runs") / run_id
    directory.mkdir(parents=True, mode=0o700)
    inputs = directory / "inputs"
    inputs.mkdir()
    archive_controller(source_root, inputs / "controller")
    if previous:
        for name in ["prompt.txt", "protocol.txt", "inventory.txt", "initial-workspace.tar"]:
            shutil.copyfile(previous / "inputs" / name, inputs / name)
    else:
        shutil.copyfile(repo_path(root, recipe["prompt"]), inputs / "prompt.txt")
        shutil.copyfile(repo_path(root, recipe["protocol"]), inputs / "protocol.txt")
        inventory = (root / "workshop/docs/INVENTORY.md").read_text()
        (inputs / "inventory.txt").write_text(
            "Installed collections: " + ", ".join(image["toolsets"]) + "\n\n" + inventory
        )
        initial_archive(
            repo_path(root, recipe["seeds"]) if recipe["seeds"] else None,
            inputs / "initial-workspace.tar",
        )
    if smoke and not previous:
        (inputs / "prompt.txt").write_text(
            "This is a two-session infrastructure test. In session 1, write /work/probe.txt with exactly "
            "playtime-persistent, verify the file, and call end_session with a useful memento and finish_run=false. "
            "In session 2, read that file, verify its exact contents, and call end_session with finish_run=true. "
            "Do not create other projects.\n"
        )
    write_json(inputs / "recipe.json", recipe)
    write_json(inputs / "tools.json", TOOLS)
    controller = controller_manifest(source_root)
    manifest = {
        "schema": 1,
        "id": run_id,
        "created": timestamp(),
        "kind": "scripted-smoke" if scripted else ("live-smoke" if smoke else "experiment"),
        "repeat_of": previous.name if previous else None,
        "recipe": recipe,
        "image": image,
        "model": model,
        "controller_sha256": digest_json(controller),
        "controller_sources": controller,
        "inputs": tree_manifest(inputs),
        "runtime": {
            "python": sys.version,
            "platform": platform.platform(),
            "docker": json.loads(command(["docker", "version", "--format", "{{json .Server}}"])),
            "inference_endpoint": recipe["model"]["endpoint"],
        },
    }
    write_json(directory / "manifest.json", manifest)
    print(f"Run archive: {directory}", flush=True)
    supervisor = Supervisor(directory, recipe, image, scripted=scripted)
    result = supervisor.execute()
    from .report import publish

    publish(directory)
    return directory, result


class Supervisor:
    def __init__(self, directory, recipe, image, *, scripted=False):
        self.directory = Path(directory)
        self.recipe = recipe
        self.started = time.monotonic()
        self.deadline = self.started + recipe["limits"]["wall_seconds"]
        self.sandbox = Sandbox(image["image_id"], recipe, self.directory.name, self.deadline)
        self.log = EventLog(self.directory / "events.jsonl", recipe["limits"]["log_mb"] * 1024**2)
        self.model = ScriptedModel() if scripted else Ollama(recipe["model"]["endpoint"])
        self.generated = 0
        self.calls = 0
        self.session = 0
        self.last_snapshot = None
        self.index = {}
        self.sessions = []
        self.usage_complete = True

    def remaining(self):
        remaining = self.deadline - time.monotonic()
        if remaining <= 0:
            raise StopRun("wall_limit")
        return remaining

    def budget(self):
        return {
            "generated_tokens_remaining": self.recipe["limits"]["generated_tokens"]
            - self.generated,
            "wall_seconds_limit": self.recipe["limits"]["wall_seconds"],
            "session": self.session,
            "sessions_total": self.recipe["sessions"],
        }

    def request(self, messages, *, handoff=False):
        limits = self.recipe["limits"]
        remaining_tokens = limits["generated_tokens"] - self.generated
        reserve = 0 if handoff else limits["handoff_tokens"]
        maximum = min(
            limits["handoff_tokens"] if handoff else limits["tokens_per_call"],
            remaining_tokens - reserve,
        )
        if maximum <= 0:
            raise StopRun("token_limit")
        model = self.recipe["model"]
        tools = [TOOLS[-1]] if handoff else TOOLS
        input_bytes = len(canonical({"messages": messages, "tools": tools}).encode())
        if input_bytes + maximum + 4096 >= model["context_tokens"]:
            raise SessionFull("context_guard")
        self.calls += 1
        payload = {
            "model": model["name"],
            "messages": messages,
            "tools": tools,
            "stream": True,
            "think": model["think"],
            "keep_alive": "5m",
            "options": {
                "num_ctx": model["context_tokens"],
                "num_predict": maximum,
                "temperature": model["temperature"],
                "top_p": model["top_p"],
                "top_k": model["top_k"],
                "repeat_penalty": model["repeat_penalty"],
                "seed": (self.recipe["sampling_seed"] + self.calls - 1) % (2**31),
            },
        }
        self.log.emit(
            "model_request",
            session=self.session,
            call=self.calls,
            handoff=handoff,
            payload=payload,
            seconds_remaining=round(self.remaining(), 3),
        )
        self.usage_complete = False
        response = self.model.chat(
            payload,
            min(limits["model_seconds"], self.remaining()),
            lambda chunk: self.log.emit(
                "model_chunk", session=self.session, call=self.calls, chunk=chunk
            ),
        )
        self.generated += response["eval_count"]
        self.usage_complete = True
        self.log.emit(
            "model_response",
            session=self.session,
            call=self.calls,
            response=response,
            generated_tokens_total=self.generated,
        )
        if self.generated > limits["generated_tokens"]:
            raise StopRun("token_limit_exceeded_by_server")
        return response["message"]

    def apply_calls(self, message, messages, *, handoff=False):
        calls = message.get("tool_calls", [])
        # A model cannot turn one work turn into arbitrarily many tool executions.
        if len(calls) > 8:
            raise StopRun("too_many_tools_in_one_response")
        ended = False
        finished = False
        for call in calls:
            self.remaining()
            function = call.get("function", {})
            name, arguments = function.get("name"), function.get("arguments")
            self.log.emit("tool_request", session=self.session, name=name, arguments=arguments)
            error = validate_call(name, arguments)
            if ended:
                error = "The session has already ended"
            if handoff and name != "end_session":
                error = "Only end_session is available during handoff"
            if error:
                result = {"error": error}
            elif name == "end_session":
                if len(arguments["memento"].encode()) > self.recipe["limits"]["memento_bytes"]:
                    result = {"error": "Memento exceeds its byte limit; shorten it and retry"}
                else:
                    result = self.sandbox.tool(
                        "write_file", {"path": "/work/MEMENTO.md", "content": arguments["memento"]}
                    )
                    if "error" not in result:
                        ended = True
                        finished = arguments.get("finish_run", False)
            else:
                result = self.sandbox.tool(name, arguments)
            self.log.emit("tool_result", session=self.session, name=name, result=result)
            messages.append(
                {"role": "tool", "tool_name": name or "unknown", "content": canonical(result)}
            )
        return ended, finished

    def checkpoint(self, *, interrupted=False):
        self.sandbox.stop_session()
        folder = self.directory / "sessions" / f"{self.session:03d}"
        folder.mkdir(parents=True, exist_ok=True)
        snapshot = folder / "workspace.tar"
        self.index, changes = self.sandbox.snapshot(snapshot, self.index)
        self.last_snapshot = snapshot
        memory = self.sandbox.memory()
        (folder / "memento.txt").write_text(memory["content"])
        summary = {
            "session": self.session,
            "generated_tokens_total": self.generated,
            "changes": changes,
            "memento_status": memory["status"],
            "interrupted": interrupted,
        }
        write_json(folder / "summary.json", summary)
        self.sessions.append(summary)
        self.log.emit("checkpoint", final=interrupted, **summary)

    def execute(self):
        reason = "session_limit"
        error_text = None
        prompt = (self.directory / "inputs/prompt.txt").read_text()
        protocol = (self.directory / "inputs/protocol.txt").read_text()
        inventory = (self.directory / "inputs/inventory.txt").read_text()
        limits = self.recipe["limits"]
        try:
            self.log.emit("run_start", recipe=self.recipe)
            self.sandbox.create(self.directory / "inputs/initial-workspace.tar")
            for session_number in range(1, self.recipe["sessions"] + 1):
                self.session = session_number
                self.remaining()
                memory = self.sandbox.memory()
                self.log.emit(
                    "session_start", session=self.session, memory=memory, budget=self.budget()
                )
                self.sandbox.start_session(self.session)
                messages = [
                    {"role": "system", "content": protocol},
                    {
                        "role": "user",
                        "content": f"{prompt}\nSession {self.session}/{self.recipe['sessions']}.\n"
                        f"Budget: {canonical(self.budget())}\nMemento limit: {limits['memento_bytes']} UTF-8 bytes.\n"
                        f"\n{inventory}\n\nMemento status: {memory['status']}\n{memory['content']}",
                    },
                ]
                ended = finished = False
                for turn in range(self.recipe["work_turns_per_session"]):
                    # UTF-8/JSON bytes conservatively upper-bound ordinary text tokens.
                    # Reserve room for model templates, output, and the final handoff.
                    input_bytes = len(canonical({"messages": messages, "tools": TOOLS}).encode())
                    if (
                        input_bytes + limits["tokens_per_call"] + 4096
                        >= self.recipe["model"]["context_tokens"]
                    ):
                        self.log.emit("early_handoff", session=self.session, reason="context_guard")
                        break
                    if limits["generated_tokens"] - self.generated <= limits["handoff_tokens"]:
                        break
                    try:
                        message = self.request(messages)
                    except SessionFull:
                        self.log.emit("early_handoff", session=self.session, reason="context_guard")
                        break
                    messages.append(message)
                    ended, finished = self.apply_calls(message, messages)
                    if ended:
                        break
                    messages.append(
                        {
                            "role": "user",
                            "content": f"Continue independently. Budget: {canonical(self.budget())}. "
                            f"Work turns left this session: {self.recipe['work_turns_per_session'] - turn - 1}.",
                        }
                    )
                if not ended:
                    messages.append(
                        {
                            "role": "user",
                            "content": "This session is ending. Call end_session now with the memento "
                            "your next session needs. Only end_session is available. "
                            f"Maximum {limits['memento_bytes']} UTF-8 bytes. Budget: {canonical(self.budget())}",
                        }
                    )
                    for _ in range(2):
                        if self.generated >= limits["generated_tokens"]:
                            break
                        try:
                            message = self.request(messages, handoff=True)
                        except SessionFull:
                            self.log.emit(
                                "handoff_skipped", session=self.session, reason="context_guard"
                            )
                            break
                        messages.append(message)
                        ended, finished = self.apply_calls(message, messages, handoff=True)
                        if ended:
                            break
                    if not ended:
                        self.log.emit("handoff_missing", session=self.session)
                self.checkpoint()
                if finished:
                    reason = "agent_finished"
                    break
                if self.generated >= limits["generated_tokens"]:
                    reason = "token_limit"
                    break
        except StopRun as error:
            reason = str(error)
        except KeyboardInterrupt:
            reason = "interrupted"
        except Exception as error:
            reason = "error"
            error_text = f"{type(error).__name__}: {error}"
        finally:
            if self.sandbox.active:
                try:
                    self.checkpoint(interrupted=True)
                except Exception as error:
                    error_text = (error_text or "") + f"; final checkpoint: {error}"
            cleanup_errors = self.sandbox.close()
            if self.last_snapshot:
                try:
                    extract_regular_files(self.last_snapshot, self.directory / "final-workspace")
                except Exception as error:
                    error_text = (error_text or "") + f"; artifact extraction: {error}"
            result = {
                "stop_reason": reason,
                "error": error_text,
                "cleanup_errors": cleanup_errors,
                "generated_tokens": self.generated,
                "token_usage_complete": self.usage_complete,
                "model_calls": self.calls,
                "sessions": self.sessions,
                "elapsed_seconds": round(time.monotonic() - self.started, 3),
                "finished": timestamp(),
            }
            write_json(self.directory / "result.json", result)
            try:
                self.log.emit(
                    "run_end",
                    final=True,
                    stop_reason=reason,
                    generated_tokens=self.generated,
                    token_usage_complete=self.usage_complete,
                    error=error_text,
                    cleanup_errors=cleanup_errors,
                )
            finally:
                self.log.close()
        print(
            f"Stopped: {reason}; generated tokens: {self.generated}; sessions: {len(self.sessions)}",
            flush=True,
        )
        return result


class ScriptedModel:
    """A deterministic tool-using fixture that exercises the real Docker runner."""

    def __init__(self):
        self.calls = 0

    def chat(self, payload, timeout, on_chunk):
        self.calls += 1
        script = [
            ("write_file", {"path": "/work/probe.txt", "content": "playtime-persistent"}),
            (
                "exec",
                {
                    "command": "touch /tmp/transient; python -c 'import time; time.sleep(10)'",
                    "timeout_seconds": 0.1,
                },
            ),
            (
                "end_session",
                {
                    "memento": "Read /work/probe.txt and verify the saved experiment marker. /tmp should be fresh.",
                    "finish_run": False,
                },
            ),
            (
                "exec",
                {
                    "command": "python -c \"from pathlib import Path; assert Path('/work/probe.txt').read_text() == 'playtime-persistent'; assert not Path('/tmp/transient').exists(); print('persistence-ok')\""
                },
            ),
            (
                "end_session",
                {
                    "memento": "The file persisted across the reset and temporary state was cleared.",
                    "finish_run": True,
                },
            ),
        ]
        if self.calls == 4:
            if (
                len(payload["messages"]) != 2
                or "playtime-persistent"
                in payload["messages"][1]["content"].split("Memento status:")[-1]
            ):
                raise PlaytimeError("The new session unexpectedly inherited conversation history")
        if self.calls > len(script):
            raise PlaytimeError("Scripted smoke did not follow the expected lifecycle")
        name, arguments = script[self.calls - 1]
        response = {
            "model": "scripted-fixture",
            "message": {
                "role": "assistant",
                "content": "",
                "tool_calls": [{"function": {"name": name, "arguments": arguments}}],
            },
            "done": True,
            "done_reason": "stop",
            "eval_count": 32,
        }
        on_chunk(response)
        return response
