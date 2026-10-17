import copy
import io
import json
import sys
import tarfile
import tempfile
import time
import unittest
from contextlib import redirect_stdout
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from playtime.common import PlaytimeError, StopRun
from playtime.config import load_recipe, validate
from playtime.ollama import ToolCallParseError
from playtime.report import load_timeline, report
from playtime.runner import SessionFull, Supervisor

ROOT = Path(__file__).resolve().parents[1]


def parse_error():
    return ToolCallParseError(
        "error parsing tool call: invalid character ']' after object key:value pair"
    )


def completed(*, finish=True, handoff=True, tokens=5):
    message = {"role": "assistant", "content": "continuing"}
    if handoff:
        message["tool_calls"] = [
            {
                "function": {
                    "name": "end_session",
                    "arguments": {
                        "memento": "The project is in progress.",
                        "finish_run": finish,
                    },
                }
            }
        ]
    return {"done": True, "eval_count": tokens, "message": message}


class SequenceModel:
    def __init__(self, responses):
        self.responses = iter(responses)
        self.requests = []

    def chat(self, payload, timeout, on_chunk):
        self.requests.append(copy.deepcopy(payload))
        response = next(self.responses)
        if isinstance(response, Exception):
            # Even a complete tool call in an unfinished response must not run.
            on_chunk(
                {
                    "message": {
                        "thinking": "Before the failure",
                        "tool_calls": [
                            {
                                "function": {
                                    "name": "exec",
                                    "arguments": {"command": "must-not-execute"},
                                }
                            }
                        ],
                    }
                }
            )
            if isinstance(response, ToolCallParseError):
                on_chunk({"error": str(response)})
            raise response
        on_chunk(response)
        return response


class RecordingSandbox:
    """Lifecycle fixture; records calls and never executes model commands."""

    def __init__(self):
        self.active = False
        self.calls = []
        self.memento = ""

    def create(self, initial):
        pass

    def start_session(self, number):
        self.active = True

    def stop_session(self):
        self.active = False

    def memory(self):
        return {"content": self.memento, "status": "loaded"}

    def tool(self, name, arguments):
        self.calls.append((name, arguments))
        if name != "write_file" or arguments["path"] != "/work/MEMENTO.md":
            raise AssertionError("Unexpected model action")
        self.memento = arguments["content"]
        return {"bytes_written": len(self.memento.encode()), "path": arguments["path"]}

    def snapshot(self, path, previous):
        with tarfile.open(path, "w"):
            pass
        return {}, {"added": [], "modified": [], "removed": []}

    def close(self):
        self.active = False
        return []


class RecoveryTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.directory = Path(temporary.name)
        self.recipe = load_recipe(ROOT / "experiments/curiosity.toml")
        self.recipe["sessions"] = 3
        self.recipe["work_turns_per_session"] = 1
        self.recipe["limits"].update(generated_tokens=1000, tokens_per_call=100, handoff_tokens=50)
        self.recipe["limits"].pop("tool_parse_error_limit", None)
        self.runner = Supervisor(self.directory, self.recipe, {"image_id": "unused"})
        self.runner.sandbox = RecordingSandbox()
        self.addCleanup(self.runner.log.close)
        inputs = self.directory / "inputs"
        inputs.mkdir()
        for name in ["prompt", "protocol", "inventory"]:
            (inputs / (name + ".txt")).write_text("Explore the workshop.")
        (self.directory / "manifest.json").write_text(
            json.dumps(
                {
                    "id": "recovery-fixture",
                    "kind": "fixture",
                    "recipe": self.recipe,
                }
            )
        )

    def execute(self, responses):
        self.runner.model = SequenceModel(responses)
        with redirect_stdout(io.StringIO()):
            return self.runner.execute()

    def test_failed_work_turn_uses_handoff_and_memory_resets_but_accounting_persists(self):
        result = self.execute([parse_error(), completed(finish=False), completed()])
        requests = self.runner.model.requests
        self.assertEqual(result["stop_reason"], "agent_finished")
        self.assertEqual(result["model_calls"], 3)
        self.assertEqual(result["generated_tokens"], 10)
        self.assertEqual(result["token_budget_charged"], 110)
        self.assertEqual(result["unaccounted_token_reserve"], 100)
        self.assertEqual(result["unaccounted_model_calls"], 1)
        self.assertFalse(result["token_usage_complete"])
        self.assertEqual(
            [tool["function"]["name"] for tool in requests[1]["tools"]], ["end_session"]
        )
        self.assertIn("None of that response's tools ran", json.dumps(requests[1]["messages"]))
        self.assertEqual(len(requests[2]["messages"]), 2)
        self.assertNotIn("server rejected", json.dumps(requests[2]["messages"]))
        self.assertIn("The project is in progress", requests[2]["messages"][1]["content"])
        self.assertEqual(
            [name for name, _ in self.runner.sandbox.calls], ["write_file", "write_file"]
        )
        sessions, _ = load_timeline(self.directory)
        self.assertEqual(len(sessions[1]["turns"]), 2)
        self.assertIsNone(sessions[1]["turns"][0]["response"])
        self.assertTrue(sessions[1]["turns"][0]["error"]["recovery_allowed"])
        document = report(self.directory).read_text()
        for text in [
            "10 + unknown",
            "Budget charged: 110",
            "Correction requested",
            "Before the failure",
            "not measured usage",
            "No tools from this response ran",
        ]:
            self.assertIn(text, document)

    def test_recovery_allowance_does_not_reset_between_sessions(self):
        result = self.execute(
            [
                parse_error(),
                completed(finish=False),
                parse_error(),
                completed(finish=False),
                parse_error(),
            ]
        )
        self.assertEqual(result["stop_reason"], "tool_parse_retry_limit")
        self.assertEqual(result["model_calls"], 5)
        self.assertEqual(result["tool_parse_errors"], 3)
        self.assertEqual(result["generated_tokens"], 10)
        self.assertEqual(result["token_budget_charged"], 310)
        self.assertEqual(result["unaccounted_model_calls"], 3)
        self.assertEqual(len(self.runner.sandbox.calls), 2)
        self.assertIn("Retry limit reached", report(self.directory).read_text())

    def test_handoff_failures_do_not_add_extra_turns(self):
        self.recipe["sessions"] = 1
        result = self.execute([completed(handoff=False), parse_error(), parse_error()])
        self.assertEqual(result["stop_reason"], "session_limit")
        self.assertEqual(result["model_calls"], 3)
        self.assertEqual(result["token_budget_charged"], 105)
        self.assertFalse(result["token_usage_complete"])
        self.assertEqual(self.runner.sandbox.calls, [])
        for request in self.runner.model.requests[1:]:
            self.assertEqual(
                [tool["function"]["name"] for tool in request["tools"]], ["end_session"]
            )

    def test_separate_retry_limits_allow_recovered_errors_but_bound_the_run(self):
        self.recipe["sessions"] = 4
        self.recipe["limits"]["tool_parse_error_limit"] = 4
        result = self.execute(
            [
                parse_error(),
                completed(finish=False),
                parse_error(),
                completed(finish=False),
                parse_error(),
                completed(finish=False),
                parse_error(),
            ]
        )
        self.assertEqual(result["stop_reason"], "tool_parse_error_limit")
        self.assertEqual(result["model_calls"], 7)
        self.assertEqual(result["tool_parse_errors"], 4)
        self.assertEqual(result["token_budget_charged"], 415)
        self.assertEqual(len(self.runner.sandbox.calls), 3)

    def test_consecutive_failures_stop_before_the_larger_run_limit(self):
        self.recipe["limits"]["tool_parse_error_limit"] = 8
        self.recipe["work_turns_per_session"] = 5
        result = self.execute([parse_error(), parse_error(), parse_error()])
        self.assertEqual(result["stop_reason"], "tool_parse_retry_limit")
        self.assertEqual(result["model_calls"], 3)
        self.assertEqual(result["token_budget_charged"], 300)
        self.assertEqual(self.runner.sandbox.calls, [])

    def test_large_tool_results_leave_room_for_handoff_and_next_session(self):
        self.recipe["model"]["context_tokens"] = 32768
        self.recipe["limits"].update(
            generated_tokens=20000, tokens_per_call=4096, handoff_tokens=2048
        )
        self.recipe["work_turns_per_session"] = 2
        original_tool = self.runner.sandbox.tool

        def tool(name, arguments):
            if name == "read_file":
                return {"content": '"\n\\\x00' * 4096, "offset_bytes": 0}
            return original_tool(name, arguments)

        self.runner.sandbox.tool = tool
        work = {
            "done": True,
            "eval_count": 3000,
            "prompt_eval_count": 1000,
            "message": {
                "role": "assistant",
                "content": "",
                "thinking": "thinking " * 1500,
                "tool_calls": [
                    {"function": {"name": "read_file", "arguments": {"path": "/work/large.txt"}}}
                ]
                * 8,
            },
        }
        result = self.execute([work, completed(finish=False), completed()])
        self.assertEqual(result["stop_reason"], "agent_finished")
        self.assertEqual(result["model_calls"], 3)
        self.assertEqual(len(result["sessions"]), 2)
        self.assertTrue(all(session["handoff_saved"] for session in result["sessions"]))
        requests = self.runner.model.requests
        self.assertEqual([t["function"]["name"] for t in requests[1]["tools"]], ["end_session"])
        results = [json.loads(m["content"]) for m in requests[1]["messages"] if m["role"] == "tool"]
        self.assertEqual(len(results), 8)
        self.assertTrue(any(r.get("context_truncated") for r in results))
        log = [
            json.loads(line) for line in (self.directory / "events.jsonl").read_text().splitlines()
        ]
        self.assertFalse(any(e["kind"] == "handoff_skipped" for e in log))
        self.assertTrue(any(e.get("unabridged_result") for e in log))
        self.assertEqual(len(requests[2]["messages"]), 2)
        self.assertNotIn("thinking thinking", json.dumps(requests[2]["messages"]))
        self.assertIn("The project is in progress", requests[2]["messages"][1]["content"])

    def test_text_handoff_gets_feedback_then_saves_memory_for_next_session(self):
        text_handoff = completed(handoff=False)
        text_handoff["message"]["content"] = json.dumps(
            {"memento": "This text must not be saved automatically.", "finish_run": True}
        )
        result = self.execute(
            [completed(handoff=False), text_handoff, completed(finish=False), completed()]
        )
        requests = self.runner.model.requests
        self.assertEqual(result["stop_reason"], "agent_finished")
        self.assertEqual(result["model_calls"], 4)
        self.assertEqual(result["generated_tokens"], 20)
        self.assertTrue(result["token_usage_complete"])
        self.assertEqual(result["tool_parse_errors"], 0)
        feedback = requests[2]["messages"][-1]
        self.assertEqual(feedback["role"], "user")
        self.assertIn("did not save a memento", feedback["content"])
        self.assertIn("Call the end_session tool", feedback["content"])
        self.assertEqual(requests[2]["messages"][-2], text_handoff["message"])
        for request in requests[1:3]:
            self.assertEqual(
                [tool["function"]["name"] for tool in request["tools"]], ["end_session"]
            )
        self.assertEqual(len(requests[3]["messages"]), 2)
        self.assertIn("The project is in progress.", requests[3]["messages"][1]["content"])
        self.assertNotIn("did not call a tool", json.dumps(requests[3]["messages"]))
        self.assertEqual(
            self.runner.sandbox.calls,
            [("write_file", {"path": "/work/MEMENTO.md", "content": "The project is in progress."})]
            * 2,
        )
        sessions, _ = load_timeline(self.directory)
        notices = sessions[1]["notices"]
        self.assertEqual(len(notices), 1)
        self.assertEqual(notices[0]["kind"], "handoff_feedback")
        self.assertEqual(notices[0]["call"], 2)
        self.assertEqual(notices[0]["content"], feedback["content"])
        self.assertIn("did not save a memento", report(self.directory).read_text())

    def test_empty_handoffs_get_one_reminder_and_preserve_existing_memory(self):
        self.recipe["sessions"] = 1
        self.runner.sandbox.memento = "Existing checkpoint."
        empty = completed(handoff=False)
        empty["message"].update(content="", tool_calls=[])
        result = self.execute([completed(handoff=False), empty, empty])
        self.assertEqual(result["stop_reason"], "session_limit")
        self.assertEqual(result["model_calls"], 3)
        self.assertEqual(self.runner.sandbox.calls, [])
        self.assertEqual(self.runner.sandbox.memento, "Existing checkpoint.")
        self.assertIn(
            "did not save a memento", self.runner.model.requests[2]["messages"][-1]["content"]
        )
        sessions, _ = load_timeline(self.directory)
        self.assertEqual(
            [notice["kind"] for notice in sessions[1]["notices"]],
            ["handoff_feedback", "handoff_missing"],
        )

    def test_handoff_feedback_does_not_bypass_generation_budget(self):
        self.recipe["sessions"] = 1
        self.recipe["limits"]["generated_tokens"] = 55
        result = self.execute([completed(handoff=False), completed(handoff=False, tokens=50)])
        self.assertEqual(result["stop_reason"], "token_limit")
        self.assertEqual(result["model_calls"], 2)
        self.assertEqual(result["generated_tokens"], 55)
        self.assertEqual(self.runner.sandbox.calls, [])

    def test_unknown_usage_consumes_work_budget_and_preserves_only_handoff_reserve(self):
        self.recipe["limits"].update(generated_tokens=150)
        self.runner.model = SequenceModel([parse_error(), completed(tokens=50)])
        messages = [{"role": "user", "content": "Explore"}]
        self.assertIsNone(self.runner.request(messages))
        with self.assertRaisesRegex(StopRun, "token_limit"):
            self.runner.request(messages)
        self.runner.request(messages, handoff=True)
        self.assertEqual(
            [request["options"]["num_predict"] for request in self.runner.model.requests], [100, 50]
        )
        self.assertEqual(self.runner.generated, 50)
        self.assertEqual(self.runner.tokens_charged, 150)
        self.assertFalse(self.runner.usage_complete)
        with self.assertRaisesRegex(StopRun, "token_limit"):
            self.runner.request(messages, handoff=True)

    def test_retries_obey_context_wall_and_log_limits_before_dispatch(self):
        self.runner.model = SequenceModel([parse_error()])
        messages = [{"role": "user", "content": "Explore"}]
        self.runner.request(messages)
        with self.assertRaises(SessionFull):
            self.runner.request(
                messages
                + [{"role": "user", "content": "x" * self.recipe["model"]["context_tokens"]}]
            )
        deadline = self.runner.deadline
        self.runner.deadline = time.monotonic() - 1
        with self.assertRaisesRegex(StopRun, "wall_limit"):
            self.runner.request(messages)
        self.runner.deadline = deadline
        self.runner.log.max_bytes = self.runner.log.used + 65536
        with self.assertRaisesRegex(StopRun, "log_limit"):
            self.runner.request(messages)
        self.assertEqual(len(self.runner.model.requests), 1)
        self.assertEqual(self.runner.tokens_charged, 100)

    def test_timeouts_remain_fatal_after_a_recoverable_error(self):
        result = self.execute([parse_error(), StopRun("model_timeout")])
        self.assertEqual(result["stop_reason"], "model_timeout")
        self.assertEqual(result["model_calls"], 2)
        self.assertEqual(result["tool_parse_errors"], 1)
        self.assertEqual(result["unaccounted_model_calls"], 2)
        self.assertEqual(result["token_budget_charged"], 150)
        self.assertFalse(result["token_usage_complete"])

    def test_transport_failure_preserves_cause_and_unknown_usage(self):
        failure = StopRun("model_connection_error")
        failure.__cause__ = ConnectionResetError("Connection reset by peer")
        result = self.execute([failure])
        self.assertEqual(result["stop_reason"], "model_connection_error")
        self.assertEqual(result["error"], "ConnectionResetError: Connection reset by peer")
        self.assertEqual(result["generated_tokens"], 0)
        self.assertEqual(result["token_budget_charged"], 100)
        self.assertFalse(result["token_usage_complete"])
        self.assertEqual(self.runner.sandbox.calls, [])
        self.assertIn("Connection reset by peer", report(self.directory).read_text())

    def test_old_recipes_default_to_no_recovery_and_retry_settings_are_validated(self):
        del self.recipe["limits"]["tool_parse_retries"]
        validate(self.recipe)
        result = self.execute([parse_error()])
        self.assertEqual(result["stop_reason"], "tool_parse_retry_limit")
        self.assertEqual(result["model_calls"], 1)
        for value in [-1, True, 11, 1.5]:
            self.recipe["limits"]["tool_parse_retries"] = value
            with self.subTest(value=value), self.assertRaises(PlaytimeError):
                validate(self.recipe)
        self.recipe["limits"]["tool_parse_retries"] = 2
        for value in [0, True, 101, 1.5]:
            self.recipe["limits"]["tool_parse_error_limit"] = value
            with self.subTest(value=value), self.assertRaises(PlaytimeError):
                validate(self.recipe)


if __name__ == "__main__":
    unittest.main()
