"""Run checker controls in the locked workshop. Never execute candidates on host.

Requires the existing locked Docker image. Missing prerequisites fail explicitly.
"""
import json
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parent.parent


class CheckerControls(unittest.TestCase):
    def assess(self, directory):
        result = subprocess.run(
            ["bash", str(ROOT / "scripts/check-simulation-repair.sh"), str(directory)],
            capture_output=True, text=True,
        )
        self.assertIn(result.returncode, (0, 1), result.stderr)
        try:
            report = json.loads(result.stdout)
        except ValueError:
            self.fail(f"Checker did not complete: {result.stderr} {result.stdout}")
        verdicts = {row["id"]: row["verdict"] for row in report["requirements"]}
        self.assertEqual(set(verdicts), {f"R{i}" for i in range(1, 6)})
        self.assertEqual(result.returncode, int("failed" in verdicts.values()))
        return verdicts

    def test_starter_is_rejected(self):
        self.assertEqual(self.assess(ROOT / "seeds/simulation-repair-v1"), {
            "R1": "passed", "R2": "failed", "R3": "failed", "R4": "failed", "R5": "failed",
        })

    def test_reference_is_accepted(self):
        verdicts = self.assess(ROOT / "assessments/simulation-repair-v1/reference")
        self.assertTrue(all(verdict == "passed" for verdict in verdicts.values()))

    def test_local_helper_module_is_supported(self):
        with tempfile.TemporaryDirectory() as temp:
            directory = Path(temp)
            reference = (ROOT / "assessments/simulation-repair-v1/reference/diffusion.py").read_bytes()
            (directory / "helper.py").write_bytes(reference)
            (directory / "diffusion.py").write_text("from helper import Simulation\n")
            verdicts = self.assess(directory)
            self.assertTrue(all(verdict == "passed" for verdict in verdicts.values()))

    def test_zero_step_live_alias_is_rejected(self):
        with tempfile.TemporaryDirectory() as temp:
            directory = Path(temp)
            reference = (ROOT / "assessments/simulation-repair-v1/reference/diffusion.py").read_text()
            needle = "    def advance(self, steps):\n"
            self.assertEqual(reference.count(needle), 1)
            broken = reference.replace(needle, needle + "        if steps == 0:\n            return [self.cells]\n")
            (directory / "diffusion.py").write_text(broken)
            verdicts = self.assess(directory)
            self.assertEqual(verdicts, {
                "R1": "passed", "R2": "passed", "R3": "passed", "R4": "failed", "R5": "passed",
            })


if __name__ == "__main__":
    unittest.main()
