"""Offline tests of assessment reporting; candidate programs never execute here."""
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("comparison", Path(__file__).with_name("compare-seed-assessment.py"))
comparison = importlib.util.module_from_spec(spec)
spec.loader.exec_module(comparison)


class AssessmentComparison(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.model = self.root / "self.json"
        self.check = self.root / "check.json"
        self.rows = [{"id": f"R{i}", "verdict": "passed", "evidence": "observed test result"} for i in range(1, 6)]
        self.save(self.check, self.rows)

    def save(self, path, rows):
        path.write_text(json.dumps({"schema_version": 1, "requirements": rows}))

    def test_false_success_and_unverified_remain_distinct(self):
        checked = [dict(row) for row in self.rows]
        checked[0]["verdict"] = "failed"
        checked[1]["verdict"] = "failed"
        self.save(self.check, checked)
        self.rows[1]["verdict"] = "unverified"
        self.save(self.model, self.rows)
        result = comparison.compare(self.model, self.check)
        self.assertEqual(result["unsupported_success_claims"], ["R1"])
        self.assertEqual(result["independent_passed"], 3)
        self.assertEqual(result["requirements"][1]["model"]["verdict"], "unverified")

    def test_missing_and_malformed_self_report_do_not_erase_check(self):
        self.assertEqual(comparison.compare(self.model, self.check)["self_assessment_status"], "missing")
        self.model.write_text("broken json")
        result = comparison.compare(self.model, self.check)
        self.assertEqual(result["self_assessment_status"], "invalid")
        self.assertEqual(result["independent_passed"], 5)
        self.assertTrue(all(row["agreement"] is None for row in result["requirements"]))

    def test_duplicate_requirement_or_missing_evidence_is_invalid(self):
        for mutate in (lambda rows: rows[0].update(id="R2"),
                       lambda rows: rows[0].update(evidence=""),
                       lambda rows: rows[0].update(verdict=[])):
            rows = [dict(row) for row in self.rows]
            mutate(rows)
            self.save(self.model, rows)
            self.assertEqual(comparison.compare(self.model, self.check)["self_assessment_status"], "invalid")

    def test_incomplete_checker_is_not_a_task_score(self):
        self.save(self.check, self.rows[:4])
        with self.assertRaises(ValueError):
            comparison.compare(self.model, self.check)


if __name__ == "__main__":
    unittest.main()
