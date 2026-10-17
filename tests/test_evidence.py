import copy
import json
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from playtime.common import PlaytimeError, write_json
from playtime.config import validate
from playtime.context import ContextWindow
from playtime.evidence import build_evidence, condition, validate_evidence
from playtime.live import checkpoint, project
from playtime.report import report
from playtime.runner import SessionFull
from playtime.sandbox import archive_index, initial_archive
from playtime.tools import TOOLS
import test_recovery as fixtures
from test_recovery import SequenceModel, completed


def command_message(command="printf '<script>quoted</script>'"):
    result = completed(handoff=False)
    result["message"]["tool_calls"] = [{"function": {"name": "exec", "arguments": {"command": command}}}]
    return result


class EvidenceTests(unittest.TestCase):
    def setUp(self):
        fixtures.RecoveryTests.setUp(self)

    def execute(self, responses, enabled=True):
        if enabled:
            self.recipe["memory_condition"] = "self_plus_evidence"
        write_json(self.directory / "manifest.json", {"id": "evidence", "kind": "fixture", "recipe": self.recipe})
        self.runner.model = SequenceModel(responses)
        return self.runner.execute()

    def test_fresh_delivery_baseline_isolation_exact_archive_and_context_cost(self):
        self.recipe["sessions"] = 3
        result = self.execute([completed(finish=False), completed(finish=False), completed()])
        self.assertEqual(result["stop_reason"], "agent_finished")
        requests = self.runner.model.requests
        self.assertEqual([len(r["messages"]) for r in requests], [2, 3, 3])
        for number, request in enumerate(requests[1:], 2):
            folder = self.directory / "sessions" / f"{number:03d}"
            exact = (folder / "evidence-received.txt").read_text()
            self.assertEqual(request["messages"][-1], {"role": "user", "content": exact})
            self.assertIn(f"from session {number - 1}", exact)
            self.assertNotIn("Controller observations", (folder / "memento.txt").read_text())
            window = ContextWindow()
            self.assertGreater(window.estimate(request["messages"], TOOLS)[0], window.estimate(request["messages"][:2], TOOLS)[0] + len(exact.encode()))
        validate_evidence(self.directory)
        (self.directory / "sessions/002/evidence-received.txt").write_text("altered")
        with self.assertRaisesRegex(PlaytimeError, "received evidence"):
            validate_evidence(self.directory)

    def test_default_condition_has_original_two_message_start_and_no_appendix(self):
        self.execute([completed(finish=False), completed()], enabled=False)
        self.assertEqual(condition(self.recipe), "self")
        self.assertEqual([len(r["messages"]) for r in self.runner.model.requests], [2, 2])
        self.assertEqual(list(self.directory.glob("sessions/*/evidence*")), [])
        self.assertIn("A · Self-written memory", report(self.directory).read_text())
        validate_evidence(self.directory)

    def test_missing_last_session_or_all_sessions_is_rejected(self):
        self.execute([completed(finish=False), completed()])
        validate_evidence(self.directory)
        for relative in ["sessions/002", "sessions"]:
            with self.subTest(missing=relative):
                path = self.directory / relative
                moved = self.directory / "removed"
                path.rename(moved)
                try:
                    with self.assertRaisesRegex(PlaytimeError, "Archived session .* is missing"):
                        validate_evidence(self.directory)
                finally:
                    moved.rename(path)

    def test_started_session_without_checkpoint_remains_valid(self):
        original = self.runner.sandbox.stop_session
        def stop():
            original()
            if self.runner.session == 2:
                raise PlaytimeError("checkpoint unavailable")
        self.runner.sandbox.stop_session = stop
        result = self.execute([completed(finish=False), completed()])
        self.assertEqual(result["stop_reason"], "error")
        self.assertTrue((self.directory / "sessions/002/evidence-received.txt").is_file())
        self.assertFalse((self.directory / "sessions/002/summary.json").exists())
        validate_evidence(self.directory)

    def test_checkpoint_event_requires_evidence_even_if_summary_is_missing(self):
        self.execute([completed()])
        folder = self.directory / "sessions/001"
        for name in ["summary.json", "evidence.json", "evidence.txt"]:
            (folder / name).unlink()
        with self.assertRaisesRegex(PlaytimeError, "Archived evidence is missing"):
            validate_evidence(self.directory)

    def test_dispatched_missing_failed_and_rejected_results_are_distinct(self):
        self.recipe["memory_condition"] = "self_plus_evidence"
        folder = self.directory / "sessions/001"
        folder.mkdir(parents=True)
        write_json(folder / "summary.json", {"changes": {"added": [], "modified": [], "removed": []}})
        self.runner.session = 1
        self.runner.sandbox.tool = lambda name, args: {"exit_code": 1, "timed_out": True, "stdout": "", "stderr": "failed"}
        self.runner.apply_calls(command_message()["message"], [])
        invalid = command_message("never-dispatched")["message"]
        invalid["tool_calls"][0]["function"]["arguments"]["extra"] = True
        self.runner.apply_calls(invalid, [])
        def missing(name, args):
            raise PlaytimeError("transport lost")
        self.runner.sandbox.tool = missing
        with self.assertRaises(PlaytimeError):
            self.runner.apply_calls(command_message("unknown-outcome")["message"], [])
        evidence = build_evidence(self.directory, 1)
        self.assertEqual(evidence["counts"]["commands_total"], 2)
        self.assertIn('"exit_code":1', evidence["content"])
        self.assertIn('"timed_out":true', evidence["content"])
        self.assertIn('"result":"unknown"', evidence["content"])
        self.assertNotIn("never-dispatched", evidence["content"])
        self.assertNotIn("transport lost", evidence["content"])

    def test_byte_bound_selection_omissions_unicode_and_upstream_truncation(self):
        folder = self.directory / "sessions/001"
        folder.mkdir(parents=True)
        write_json(folder / "summary.json", {"changes": {"added": [f"{i:03d}😀" * 100 for i in range(100)], "modified": [], "removed": ["gone"]}})
        self.runner.session = 1
        self.runner.sandbox.tool = lambda name, args: {"exit_code": 0, "timed_out": False, "stdout": "😀\n\"" * 5000, "output_truncated": True}
        for _ in range(20):
            self.runner.apply_calls(command_message("😀" * 500)["message"], [])
        evidence = build_evidence(self.directory, 1)
        self.assertEqual(evidence, build_evidence(self.directory, 1))
        self.assertLessEqual(evidence["bytes"], 4096)
        self.assertEqual(evidence["bytes"], len(evidence["content"].encode()))
        for key in ["files_omitted", "commands_omitted", "fields_truncated", "excerpt_bytes_omitted", "upstream_truncated_results"]:
            self.assertGreater(evidence["counts"][key], 0)
        self.assertIn("not artifact or test correctness", evidence["content"])
        self.assertEqual(len(evidence["sources"]["commands"]), 20 - evidence["counts"]["commands_omitted"])

    def test_saved_and_live_reports_escape_and_show_exact_received_appendix(self):
        self.recipe["work_turns_per_session"] = 2
        original = self.runner.sandbox.tool
        self.runner.sandbox.tool = lambda n, a: {"stdout": "<script>result</script>", "exit_code": 0} if n == "exec" else original(n, a)
        self.execute([command_message(), completed(finish=False), completed()])
        document = report(self.directory).read_text()
        self.assertIn("B · Self-written memory", document)
        self.assertIn("Controller appendix received", document)
        self.assertNotIn("<script>result", document)
        self.assertIn("&lt;script&gt;result", document)
        data = checkpoint(self.directory, 2)
        self.assertEqual(data["evidence"]["content"], self.runner.model.requests[-1]["messages"][-1]["content"])
        starts = [json.loads(l) for l in (self.directory / "events.jsonl").read_text().splitlines() if json.loads(l)["kind"] == "session_start"]
        self.assertEqual(project(starts[-1])["evidence"], data["evidence"])

    def test_appendix_can_trigger_existing_context_guard_without_extra_allowance(self):
        self.recipe["model"]["context_tokens"] = 24000
        self.runner.model = SequenceModel([completed()])
        messages = [{"role": "user", "content": "x" * 5000}]
        self.runner.request(messages)
        with self.assertRaises(SessionFull):
            self.runner.request(messages + [{"role": "user", "content": "x" * 4096}])
        self.assertEqual(self.runner.calls, 1)

    def test_condition_is_optional_but_invalid_values_are_rejected(self):
        validate(self.recipe)
        for value in [True, [], "evidence", None]:
            recipe = copy.deepcopy(self.recipe)
            recipe["memory_condition"] = value
            with self.subTest(value=value), self.assertRaises(PlaytimeError):
                validate(recipe)

    def test_first_checkpoint_compares_against_seeds_and_archives_source_hashes(self):
        seed = self.directory / "seeds"
        seed.mkdir()
        (seed / "same.txt").write_text("same")
        (seed / "changed.txt").write_text("before")
        (seed / "removed.txt").write_text("before")
        initial = self.directory / "inputs/initial-workspace.tar"
        initial_archive(seed, initial)
        index = archive_index(initial)
        index["seeds/changed.txt"]["sha256"] = "changed"
        index.pop("seeds/removed.txt")
        index["new.txt"] = {"sha256": "new"}
        folder = self.directory / "sessions/001"
        folder.mkdir(parents=True)
        write_json(folder / "workspace.json", {"files": index, "changes": {"added": sorted(index), "modified": [], "removed": []}})
        evidence = build_evidence(self.directory, 1)
        self.assertEqual(evidence["counts"]["files_total"], 3)
        self.assertNotIn("same.txt", evidence["content"])
        self.assertIn('"change":"modified","file":"seeds/changed.txt"', evidence["content"])
        self.assertIn('"change":"removed","file":"seeds/removed.txt"', evidence["content"])
        self.assertIn("inputs/initial-workspace.tar", evidence["sources"]["checkpoints"])

    def test_changed_source_record_is_rejected_by_repeat_validation(self):
        self.recipe["work_turns_per_session"] = 2
        original = self.runner.sandbox.tool
        self.runner.sandbox.tool = lambda n, a: {"stdout": "observed", "exit_code": 0} if n == "exec" else original(n, a)
        self.execute([command_message(), completed(finish=False), completed()])
        validate_evidence(self.directory)
        path = self.directory / "events.jsonl"
        path.write_text(path.read_text().replace('"stdout":"observed"', '"stdout":"tampered"'))
        with self.assertRaisesRegex(PlaytimeError, "source records"):
            validate_evidence(self.directory)
