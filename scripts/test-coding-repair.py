"""Offline contract tests; candidate programs are copied, never executed here."""
import copy
import hashlib
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location("coding_repair", ROOT / "scripts/check-coding-repair.py")
checker = importlib.util.module_from_spec(spec)
spec.loader.exec_module(checker)
CATALOG = json.loads((checker.SUPPORT / "catalog.json").read_text())["tasks"]


class CodingRepairContract(unittest.TestCase):
    def test_frozen_inputs_and_original_checkers_match_provenance(self):
        for task_id, task in CATALOG.items():
            with self.subTest(task=task_id):
                folder = ROOT / "assessments" / task_id
                provenance = json.loads((folder / "provenance.json").read_text())
                original = (folder / "historical-check.py").read_bytes()
                self.assertEqual(hashlib.sha256(original).hexdigest(), provenance["historical_checker_sha256"])
                for name, expected in provenance["original_seed_files"].items():
                    data = (ROOT / "seeds" / task_id / name).read_bytes()
                    self.assertEqual(hashlib.sha256(data).hexdigest(), expected)
                for name, expected in provenance["reference_files"].items():
                    self.assertEqual(checker.digest((folder / "reference" / name).read_bytes()), expected)
                self.assertEqual(len(task["cases"]), provenance["controls"]["total"])
                self.assertEqual(len({case["id"] for case in task["cases"]}), len(task["cases"]))
                self.assertEqual(sum(case["kind"] == "deliverable" for case in task["cases"]), 2)

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

    def test_unavailable_docker_is_incomplete_not_zero_passed(self):
        with patch.object(checker, "command", side_effect=FileNotFoundError("docker unavailable")):
            report, code = checker.assess("ledger-repair-v1", ROOT / "seeds/ledger-repair-v1")
        self.assertEqual(code, 2)
        self.assertEqual(report["assessment"]["status"], "incomplete")
        self.assertNotIn("passed", report["assessment"])


if __name__ == "__main__":
    unittest.main()
