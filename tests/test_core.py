import io
import json
import sys
import tarfile
import tempfile
import time
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from playtime.common import EventLog, PlaytimeError, StopRun, command
from playtime.config import load_recipe, validate
from playtime.report import report
from playtime.sandbox import archive_index, extract_regular_files, initial_archive
from playtime.tools import validate_call

ROOT = Path(__file__).resolve().parents[1]


class CoreTests(unittest.TestCase):
    def test_config_rejects_silent_typos_and_unbounded_work(self):
        recipe = load_recipe(ROOT / "experiments/pilot.toml")
        recipe["limits"]["wall_second"] = 1
        with self.assertRaises(PlaytimeError):
            validate(recipe)
        recipe = load_recipe(ROOT / "experiments/pilot.toml")
        recipe["limits"]["generated_tokens"] = recipe["limits"]["handoff_tokens"]
        with self.assertRaises(PlaytimeError):
            validate(recipe)

    def test_process_output_and_deadline_are_bounded(self):
        started = time.monotonic()
        with self.assertRaises(PlaytimeError):
            command([sys.executable, "-c", "import time; time.sleep(10)"], timeout=0.1)
        self.assertLess(time.monotonic() - started, 2)
        with self.assertRaises(PlaytimeError):
            command([sys.executable, "-c", "print('x'*100000)"], max_bytes=100)

    def test_log_keeps_a_stop_record_after_budget_exhaustion(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "events.jsonl"
            log = EventLog(path, 70000)
            with self.assertRaises(StopRun):
                log.emit("huge", content="x" * 10000)
            log.emit("run_end", final=True, stop_reason="log_limit")
            log.close()
            self.assertEqual(json.loads(path.read_text())["kind"], "run_end")
            self.assertLess(path.stat().st_size, 70000)

    def test_tool_validation_blocks_unknown_and_malformed_calls(self):
        self.assertIsNotNone(validate_call("host_exec", {"command": "pwd"}))
        self.assertIsNotNone(validate_call("exec", {"command": "pwd", "privileged": True}))
        self.assertIsNotNone(validate_call("exec", {"command": 3}))
        self.assertIsNone(validate_call("exec", {"command": "pwd"}))

    def test_seed_archives_are_stable_and_owned_by_workshop_user(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            seed = root / "seed"
            (seed / "nested").mkdir(parents=True)
            (seed / "nested/a.txt").write_text("seed")
            initial_archive(seed, root / "one.tar")
            initial_archive(seed, root / "two.tar")
            self.assertEqual((root / "one.tar").read_bytes(), (root / "two.tar").read_bytes())
            with tarfile.open(root / "one.tar") as archive:
                for item in archive:
                    self.assertEqual(item.uid, 1000)
                    self.assertEqual(item.gid, 1000)
            (seed / "outside").symlink_to("/etc/passwd")
            with self.assertRaises(PlaytimeError):
                initial_archive(seed, root / "bad.tar")

    def test_artifact_extraction_does_not_create_links_or_escape(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            path = root / "safe.tar"
            with tarfile.open(path, "w") as archive:
                member = tarfile.TarInfo("hello.txt")
                member.size = 5
                archive.addfile(member, io.BytesIO(b"hello"))
                link = tarfile.TarInfo("outside")
                link.type = tarfile.SYMTYPE
                link.linkname = "/etc/passwd"
                archive.addfile(link)
            extract_regular_files(path, root / "output")
            self.assertEqual((root / "output/hello.txt").read_text(), "hello")
            self.assertFalse((root / "output/outside").exists())
            self.assertEqual(archive_index(path)["outside"]["link"], "/etc/passwd")
            with tarfile.open(root / "unsafe.tar", "w") as archive:
                member = tarfile.TarInfo("../escaped")
                member.size = 1
                archive.addfile(member, io.BytesIO(b"x"))
            with self.assertRaises(PlaytimeError):
                extract_regular_files(root / "unsafe.tar", root / "bad-output")
            self.assertFalse((root / "escaped").exists())

    def test_report_escapes_model_content_and_handles_partial_logs(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "inputs").mkdir()
            (root / "inputs/prompt.txt").write_text("<script>alert('prompt')</script>")
            (root / "manifest.json").write_text(
                json.dumps({"id": "test", "kind": "fixture", "recipe": {"name": "test"}})
            )
            event = {
                "sequence": 0,
                "time": "now",
                "kind": "model_response",
                "content": "<script>bad()</script>",
            }
            (root / "events.jsonl").write_text(json.dumps(event) + '\n{"partial":')
            text = report(root).read_text()
            self.assertNotIn("<script>", text)
            self.assertIn("&lt;script&gt;", text)
            self.assertIn("incomplete", text)


if __name__ == "__main__":
    unittest.main()
