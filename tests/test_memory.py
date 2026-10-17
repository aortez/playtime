import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from playtime.memory import checkpoint_memory, compare_memory
from playtime.report import report


class MemoryTests(unittest.TestCase):
    def test_successful_handoff_is_distinct_from_unchanged_or_missing_memory(self):
        before = {"status": "loaded", "content": "Next: inspect GIF\nProblems: None"}
        after = {"status": "loaded", "content": "Next: inspect GIF\nProblems: ffmpeg unavailable"}
        memory = compare_memory(before, after, True)
        self.assertEqual(memory["state"], "saved")
        self.assertIn("-Problems: None\n+Problems: ffmpeg unavailable", memory["diff"])
        self.assertEqual(compare_memory(before, before, False)["state"], "unchanged")
        self.assertEqual(compare_memory(before, before, True)["label"], "Saved again · unchanged")
        self.assertEqual(compare_memory(before, after, False)["state"], "updated")
        self.assertEqual(
            compare_memory(before, {"status": "FileNotFoundError", "content": ""}, False)["state"],
            "missing",
        )
        self.assertEqual(
            compare_memory(before, {"status": "loaded", "content": ""}, True)["state"], "empty"
        )
        self.assertEqual(
            compare_memory(before, {"status": "Memento exceeds limit", "content": ""}, False)[
                "state"
            ],
            "unavailable",
        )

    def test_old_archives_show_exact_received_memory_and_escape_diff_markup(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            folder = directory / "sessions/001"
            folder.mkdir(parents=True)
            (directory / "inputs").mkdir()
            (directory / "inputs/prompt.txt").write_text("Explore")
            (directory / "manifest.json").write_text(
                json.dumps(
                    {"id": "memory-test", "kind": "fixture", "recipe": {"name": "memory-test"}}
                )
            )
            received = {"status": "loaded", "content": "Known failure: missing ffmpeg"}
            summary = {"session": 1, "memento_status": "loaded"}
            records = [
                {"kind": "session_start", "session": 1, "memory": received},
                {
                    "kind": "tool_request",
                    "session": 1,
                    "name": "end_session",
                    "arguments": {"memento": "<script>bad()</script>"},
                },
                {
                    "kind": "tool_result",
                    "session": 1,
                    "name": "end_session",
                    "result": {"bytes_written": 22},
                },
                {"kind": "checkpoint", **summary},
            ]
            (directory / "events.jsonl").write_text(
                "\n".join(json.dumps(e) for e in records) + '\n{"partial":'
            )
            (folder / "memento.txt").write_text("<script>bad()</script>")
            memory = checkpoint_memory(directory, 1, summary)
            self.assertEqual(memory["received"], received)
            self.assertTrue(memory["handoff_saved"])
            self.assertIn("-Known failure: missing ffmpeg", memory["diff"])
            document = report(directory).read_text()
            self.assertIn('id="memory-continuity"', document)
            self.assertIn("What changed in memory", document)
            self.assertIn("&lt;script&gt;bad()&lt;/script&gt;", document)
            self.assertNotIn("<script>bad()", document)
