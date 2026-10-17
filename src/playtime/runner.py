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
from .context import CONTROL_RESERVE, TEMPLATE_RESERVE, ContextWindow, fit_tool_result
from .environment import check_image
from .ollama import Ollama, ToolCallParseError
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


SMOKE_PROMPT = (
    "This is a two-session infrastructure test. In session 1, write /work/probe.txt with exactly "
    "playtime-persistent, verify the file, and call end_session with a useful memento and finish_run=false. "
    "In session 2, read that file, verify its exact contents, and call end_session with finish_run=true. "
    "Do not create other projects.\n"
)


def prepare_run(
    root, recipe, *, scripted=False, smoke=False, previous=None, runs_dir=None, live=True
):
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
            root,
            recipe,
            live=live and not scripted,
            image=old["image"],
            model=old["model"],
            sources=False,
        )
    else:
        image, model = check(root, recipe, live=live and not scripted)
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
        (inputs / "prompt.txt").write_text(SMOKE_PROMPT)
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
    return directory, manifest


def execute_prepared(directory, manifest):
    recipe, image = manifest["recipe"], manifest["image"]
    scripted = manifest["kind"] == "scripted-smoke"
    supervisor = Supervisor(directory, recipe, image, scripted=scripted)
    result = supervisor.execute()
    from .report import publish

    publish(directory)
    return directory, result


def run(root, recipe, **options):
    # CLI and dashboard launches share the same exclusion lock.
    from .jobs import RunLock, ensure_idle, finish_job, mark_active

    with RunLock(root):
        ensure_idle(root)
        directory, manifest = prepare_run(root, recipe, **options)
        mark_active(root, directory, mode="cli")
        try:
            return execute_prepared(directory, manifest)
        finally:
            finish_job(directory)


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
        self.unaccounted_calls = 0
        self.unaccounted_token_reserve = 0
        self.tool_parse_errors = 0
        self.parse_error_streak = 0
        self.context = ContextWindow()
        self.handoff_saved = False

    @property
    def usage_complete(self):
        return self.unaccounted_calls == 0

    @property
    def tokens_charged(self):
        return self.generated + self.unaccounted_token_reserve

    def accounting(self):
        return {
            "generated_tokens": self.generated,
            "token_usage_complete": self.usage_complete,
            "unaccounted_model_calls": self.unaccounted_calls,
            "unaccounted_token_reserve": self.unaccounted_token_reserve,
            "token_budget_charged": self.tokens_charged,
            "tool_parse_errors": self.tool_parse_errors,
        }

    def remaining(self):
        remaining = self.deadline - time.monotonic()
        if remaining <= 0:
            raise StopRun("wall_limit")
        return remaining

    def budget(self):
        return {
            "generated_tokens_remaining": self.recipe["limits"]["generated_tokens"]
            - self.tokens_charged,
            "wall_seconds_limit": self.recipe["limits"]["wall_seconds"],
            "session": self.session,
            "sessions_total": self.recipe["sessions"],
        }

    def request(self, messages, *, handoff=False):
        limits = self.recipe["limits"]
        remaining_tokens = limits["generated_tokens"] - self.tokens_charged
        reserve = 0 if handoff else limits["handoff_tokens"]
        maximum = min(
            limits["handoff_tokens"] if handoff else limits["tokens_per_call"],
            remaining_tokens - reserve,
        )
        if maximum <= 0:
            raise StopRun("token_limit")
        model = self.recipe["model"]
        tools = [TOOLS[-1]] if handoff else TOOLS
        context_input, context_basis = self.context.estimate(messages, tools)
        context_reserve = TEMPLATE_RESERVE
        if not handoff:
            context_reserve += (
                limits["handoff_tokens"]
                + CONTROL_RESERVE
                + min(limits["output_bytes"], model["context_tokens"] // 4)
            )
        if context_input + maximum + context_reserve >= model["context_tokens"]:
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
        seconds_remaining = self.remaining()
        self.log.emit(
            "model_request",
            session=self.session,
            call=self.calls,
            handoff=handoff,
            payload=payload,
            seconds_remaining=round(seconds_remaining, 3),
            context_input_bound=context_input,
            context_basis=context_basis,
            context_reserve=context_reserve,
        )
        # Reserve the full generation allowance before dispatch. Only a complete
        # response with server-reported usage can replace it with an exact count.
        # A later successful call never erases an earlier unknown count.
        timeout = min(limits["model_seconds"], self.remaining())
        self.unaccounted_calls += 1
        self.unaccounted_token_reserve += maximum
        try:
            response = self.model.chat(
                payload,
                timeout,
                lambda chunk: self.log.emit(
                    "model_chunk", session=self.session, call=self.calls, chunk=chunk
                ),
            )
        except ToolCallParseError as error:
            self.tool_parse_errors += 1
            self.parse_error_streak += 1
            retries = limits.get("tool_parse_retries", 0)
            error_limit = limits.get("tool_parse_error_limit", retries + 1)
            recovery_limit = (
                "consecutive"
                if self.parse_error_streak > retries
                else "run"
                if self.tool_parse_errors >= error_limit
                else None
            )
            recovery_allowed = recovery_limit is None
            self.log.emit(
                "model_error",
                session=self.session,
                call=self.calls,
                reason="tool_call_parse_error",
                error=str(error),
                recovery_allowed=recovery_allowed,
                recovery_limit=recovery_limit,
                consecutive_errors=self.parse_error_streak,
                token_budget_charge=maximum,
                **self.accounting(),
            )
            if not recovery_allowed:
                raise StopRun(
                    "tool_parse_error_limit"
                    if recovery_limit == "run" and "tool_parse_error_limit" in limits
                    else "tool_parse_retry_limit"
                ) from error
            messages.append(
                {
                    "role": "user",
                    "content": "The server rejected your last response because a tool call could "
                    "not be parsed. None of that response's tools ran. Try again with valid JSON "
                    "object arguments. The failed request's full generation allowance was "
                    f"charged to the budget; its actual usage is unknown. Budget: {canonical(self.budget())}",
                }
            )
            # The caller consumes this work/handoff turn. There is no inner
            # retry loop that can bypass turn, context, wall, or log limits.
            return None
        self.generated += response["eval_count"]
        self.parse_error_streak = 0
        self.context.observe(messages, tools, response)
        self.unaccounted_calls -= 1
        self.unaccounted_token_reserve -= maximum
        self.log.emit(
            "model_response",
            session=self.session,
            call=self.calls,
            response=response,
            generated_tokens_total=self.generated,
            token_budget_charged_total=self.tokens_charged,
        )
        if self.tokens_charged > limits["generated_tokens"]:
            raise StopRun("token_limit_exceeded_by_server")
        return response["message"]

    def apply_calls(self, message, messages, *, handoff=False):
        calls = message.get("tool_calls", [])
        # A model cannot turn one work turn into arbitrarily many tool executions.
        if len(calls) > 8:
            raise StopRun("too_many_tools_in_one_response")
        ended = False
        finished = False
        for index, call in enumerate(calls):
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
                        self.handoff_saved = True
                        finished = arguments.get("finish_run", False)
            else:
                result = self.sandbox.tool(name, arguments)
            original = result
            if not ended and not handoff:
                context_input, _ = self.context.estimate(messages, [TOOLS[-1]])
                available = (
                    self.recipe["model"]["context_tokens"]
                    - context_input
                    - self.recipe["limits"]["handoff_tokens"]
                    - TEMPLATE_RESERVE
                    - CONTROL_RESERVE
                    - (len(calls) - index - 1) * 512
                )
                result = fit_tool_result(name, result, max(512, available))
            self.log.emit(
                "tool_result",
                session=self.session,
                name=name,
                result=result,
                **({"unabridged_result": original} if result is not original else {}),
            )
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
            "token_budget_charged_total": self.tokens_charged,
            "unaccounted_model_calls_total": self.unaccounted_calls,
            "changes": changes,
            "memento_status": memory["status"],
            "handoff_saved": self.handoff_saved,
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
                self.context = ContextWindow()
                self.handoff_saved = False
                folder = self.directory / "sessions" / f"{self.session:03d}"
                folder.mkdir(parents=True, exist_ok=True)
                write_json(folder / "memory-received.json", memory)
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
                    if limits["generated_tokens"] - self.tokens_charged <= limits["handoff_tokens"]:
                        break
                    try:
                        message = self.request(messages)
                    except SessionFull:
                        self.log.emit("early_handoff", session=self.session, reason="context_guard")
                        break
                    if message is not None:
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
                    for attempt in range(2):
                        if self.tokens_charged >= limits["generated_tokens"]:
                            break
                        try:
                            message = self.request(messages, handoff=True)
                        except SessionFull:
                            self.log.emit(
                                "handoff_skipped", session=self.session, reason="context_guard"
                            )
                            break
                        if message is not None:
                            messages.append(message)
                            ended, finished = self.apply_calls(message, messages, handoff=True)
                            if ended:
                                break
                            if attempt == 0 and not message.get("tool_calls"):
                                feedback = (
                                    "Your last response did not call a tool, so it did not save "
                                    "a memento. Call the end_session tool with your memento now. "
                                    "Returning JSON as ordinary text does not invoke the tool."
                                )
                                messages.append({"role": "user", "content": feedback})
                                self.log.emit(
                                    "handoff_feedback",
                                    session=self.session,
                                    call=self.calls,
                                    reason="missing_tool_call",
                                    content=feedback,
                                )
                    if not ended:
                        self.log.emit("handoff_missing", session=self.session)
                self.checkpoint()
                if finished:
                    reason = "agent_finished"
                    break
                if self.tokens_charged >= limits["generated_tokens"]:
                    reason = "token_limit"
                    break
        except StopRun as error:
            reason = str(error)
            if reason in {"model_connection_error", "model_timeout"} and error.__cause__:
                cause = error.__cause__
                error_text = f"{type(cause).__name__}: {cause}"
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
                **self.accounting(),
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
                    **self.accounting(),
                    error=error_text,
                    cleanup_errors=cleanup_errors,
                )
            finally:
                self.log.close()
        usage = str(self.generated) + (" + unknown" if not self.usage_complete else "")
        print(
            f"Stopped: {reason}; generated tokens: {usage}; "
            f"budget charged: {self.tokens_charged}; sessions: {len(self.sessions)}",
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
