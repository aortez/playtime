"""Offline contract tests; candidate programs are copied, never executed here."""
import copy
import hashlib
import importlib.util
import json
from pathlib import Path
import tempfile
import tomllib
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location("coding_repair", ROOT / "scripts/check-coding-repair.py")
checker = importlib.util.module_from_spec(spec)
spec.loader.exec_module(checker)
CATALOG = json.loads(checker.catalog_path(checker.CURRENT_REVISION).read_text())["tasks"]


class CodingRepairContract(unittest.TestCase):
    def test_revision_selection_freezes_the_selected_checker_and_catalog(self):
        for task_id in CATALOG:
            with self.subTest(task=task_id), tempfile.TemporaryDirectory() as temporary:
                for revision in checker.ASSESSMENT_REVISIONS:
                    available = json.loads(checker.catalog_path(revision).read_text())["tasks"]
                    if task_id not in available:
                        with self.assertRaises(ValueError):
                            checker.load_task(task_id, revision)
                        continue
                    task = checker.load_task(task_id, revision)
                    self.assertEqual(task["assessment_revision"], revision)
                    expected = CATALOG[task_id]["cases"]
                    if task_id == "linearizable-kv-v1" and revision < 7:
                        expected = [c for c in expected if c["id"] != "deferred_noop_write"]
                    self.assertEqual(task["cases"], expected)
                    identity = checker.checker_identity(task_id, revision)
                    destination = Path(temporary) / str(revision)
                    checker.freeze_checker(task_id, destination, identity, revision)
                    source = checker.checker_sources(task_id, revision)[-1]
                    self.assertEqual((destination / "check.py").read_bytes(), source.read_bytes())
                    self.assertIn(checker.catalog_path(revision).relative_to(ROOT).as_posix(),
                                  identity["files"])
                if task_id in json.loads(checker.catalog_path(2).read_text())["tasks"]:
                    self.assertNotEqual(checker.checker_identity(task_id, 2)["sha256"],
                                        checker.checker_identity(task_id, 3)["sha256"])
        with self.assertRaises(ValueError):
            checker.load_task("ledger-repair-v1", 9)

    def test_frozen_inputs_and_original_checkers_match_provenance(self):
        for task_id, task in CATALOG.items():
            with self.subTest(task=task_id):
                folder = ROOT / "assessments" / task_id
                provenance = json.loads((folder / "provenance.json").read_text())
                if "historical_checker_sha256" in provenance:
                    original = (folder / "historical-check.py").read_bytes()
                    self.assertEqual(hashlib.sha256(original).hexdigest(), provenance["historical_checker_sha256"])
                else:
                    self.assertEqual(checker.digest((folder / "check.py").read_bytes()),
                                     provenance["initial_checker_sha256"])
                for name, expected in provenance["original_seed_files"].items():
                    data = (ROOT / "seeds" / task_id / name).read_bytes()
                    self.assertEqual(hashlib.sha256(data).hexdigest(), expected)
                for name, expected in provenance["reference_files"].items():
                    self.assertEqual(checker.digest((folder / "reference" / name).read_bytes()), expected)
                original_cases = [c for c in task["cases"] if c["id"] != "deferred_noop_write"]
                self.assertEqual(len(original_cases), provenance["controls"]["total"])
                self.assertEqual(len({case["id"] for case in task["cases"]}), len(task["cases"]))
                self.assertEqual(sum(case["kind"] == "deliverable" for case in task["cases"]),
                                 provenance["controls"].get("deliverable_total", 2))

    def test_screen_recipes_keep_four_minute_budget_and_private_graders(self):
        for task_id in ("async-singleflight-v1", "streaming-jsonlines-v1", "shortest-walk-v1",
                        "sql-tombstones-v1", "delta-debugging-v1", "stable-quadratic-v1",
                        "crash-journal-v1", "linearizable-kv-v1"):
            with self.subTest(task=task_id):
                recipe = tomllib.loads((ROOT / "recipes" / (task_id.removesuffix("-v1") + ".toml")).read_text())
                self.assertEqual(recipe["seeds"], "seeds/" + task_id)
                self.assertEqual(recipe["parameters"]["limits"]["wall_seconds"], 240)
                provenance = json.loads((ROOT / "assessments" / task_id / "provenance.json").read_text())
                self.assertEqual({p.name for p in (ROOT / recipe["seeds"]).iterdir()},
                                 set(provenance["original_seed_files"]))

    def valid_report(self):
        task = CATALOG["ledger-repair-v1"]
        rows = [{"case": case["id"], "passed": True} for case in task["cases"]]
        return task, {"passed": 10, "total": 10, "results": rows}

    def test_functional_and_deliverable_counts_are_separate(self):
        task, raw = self.valid_report()
        raw["results"][-1]["passed"] = False
        raw["passed"] = 9
        result = checker.validate_result(task, raw, 1)
        self.assertEqual(result["functional"], {"passed": 8, "total": 8})
        self.assertEqual(result["deliverable"], {"passed": 1, "total": 2})
        self.assertFalse(result["all_checks_passed"])

    def test_malformed_incomplete_or_inconsistent_reports_are_not_scores(self):
        task, valid = self.valid_report()
        mutations = [
            lambda r: r["results"].pop(),
            lambda r: r["results"][0].update(case="invented"),
            lambda r: r["results"][0].update(case=r["results"][1]["case"]),
            lambda r: r["results"][0].update(passed=1),
            lambda r: r.update(passed=9),
            lambda r: r.update(total=True),
            lambda r: r.update(results=None),
        ]
        for mutate in mutations:
            raw = copy.deepcopy(valid)
            mutate(raw)
            with self.assertRaises(ValueError):
                checker.validate_result(task, raw, 0)
        with self.assertRaises(ValueError):
            checker.validate_result(task, valid, 137)
        for raw in (None, [], "not an object"):
            with self.assertRaises(ValueError):
                checker.validate_result(task, raw, 0)

    def test_skip_preserves_outcome_without_running_docker_or_grading(self):
        with patch.object(checker, "command") as command:
            report, code = checker.assess("ledger-repair-v1", None, outcome="skipped")
        command.assert_not_called()
        self.assertEqual(code, 0)
        self.assertEqual(report["assessment"], {"status": "not_run"})
        self.assertEqual(report["execution"]["outcome"], "skipped")
        self.assertIsNone(report["candidate"])

    def test_snapshot_is_independent_and_content_addressed(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "source"
            source.mkdir()
            (source / "helper.py").write_text("raise RuntimeError('must not execute on host')\n")
            snapshot = root / "snapshot"
            inventory = checker.freeze_candidate(source, snapshot)
            (source / "helper.py").write_text("changed")
            self.assertIn("must not execute", (snapshot / "helper.py").read_text())
            self.assertEqual(inventory["files"][0]["sha256"], checker.digest((snapshot / "helper.py").read_bytes()))
            self.assertEqual(snapshot.stat().st_mode & 0o777, 0o755)

    def test_snapshot_rejects_links_and_size_overflow(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "source"
            source.mkdir()
            (source / "link").symlink_to("/etc/passwd")
            with self.assertRaises(ValueError):
                checker.freeze_candidate(source, root / "linked")
            (source / "link").unlink()
            (source / "large").write_bytes(b"12345")
            with patch.object(checker, "MAX_WORKSPACE", 4), self.assertRaises(ValueError):
                checker.freeze_candidate(source, root / "oversized")

    def test_empty_directories_change_snapshot_identity(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "source"
            source.mkdir()
            (source / "helper.py").write_text("# identical file contents\n")
            before = checker.freeze_candidate(source, root / "before")
            (source / "empty" / "nested").mkdir(parents=True)
            after = checker.freeze_candidate(source, root / "after")
            self.assertNotEqual(before["sha256"], after["sha256"])
            self.assertEqual(before["size_bytes"], after["size_bytes"])
            self.assertTrue((root / "after" / "empty" / "nested").is_dir())
            self.assertEqual(before["files"], after["files"])
            self.assertEqual(after["directories"], ["empty", "empty/nested"])

    def test_executable_state_is_preserved_and_hashed(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "source"
            source.mkdir()
            helper = source / "helper.py"
            helper.write_text("# identical file contents\n")
            helper.chmod(0o600)
            before = checker.freeze_candidate(source, root / "before")
            helper.chmod(0o700)
            after = checker.freeze_candidate(source, root / "after")
            self.assertNotEqual(before["sha256"], after["sha256"])
            self.assertEqual(before["files"][0]["sha256"], after["files"][0]["sha256"])
            self.assertEqual(before["files"][0]["mode"], "0644")
            self.assertEqual(after["files"][0]["mode"], "0755")
            self.assertEqual((root / "before" / "helper.py").stat().st_mode & 0o7777, 0o644)
            self.assertEqual((root / "after" / "helper.py").stat().st_mode & 0o7777, 0o755)

    def test_unavailable_docker_is_incomplete_not_zero_passed(self):
        with patch.object(checker, "command", side_effect=FileNotFoundError("docker unavailable")):
            report, code = checker.assess("ledger-repair-v1", ROOT / "seeds/ledger-repair-v1")
        self.assertEqual(code, 2)
        self.assertEqual(report["assessment"]["status"], "incomplete")
        self.assertNotIn("passed", report["assessment"])


if __name__ == "__main__":
    unittest.main()
