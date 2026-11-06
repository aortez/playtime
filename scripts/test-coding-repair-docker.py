"""Real assessment controls. All candidate execution stays inside Docker."""
import importlib.util
import json
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import unittest

ROOT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location("coding_repair", ROOT / "scripts/check-coding-repair.py")
checker = importlib.util.module_from_spec(spec)
spec.loader.exec_module(checker)
TASKS = {"ledger-repair-v1": "report.py", "dependency-planner-repair-v1": "planner.py",
         "byte-range-repair-v1": "ranges.py"}


class CodingRepairDocker(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        image = json.loads((ROOT / "environments/general.lock.json").read_text())["image_id"]
        subprocess.run(["docker", "image", "inspect", image], stdout=subprocess.DEVNULL,
                       check=True, timeout=15)

    def assess(self, task, directory, **kwargs):
        report, code = checker.assess(task, directory, **kwargs)
        self.assertIn(code, (0, 1), report)
        self.assertEqual(report["assessment"]["status"], "complete", report)
        self.assertEqual(report["cleanup"]["status"], "completed")
        return report

    def test_positive_and_negative_controls(self):
        for task in TASKS:
            folder = ROOT / "assessments" / task
            controls = json.loads((folder / "provenance.json").read_text())["controls"]
            for kind, path, expected in [
                ("reference", folder / "reference", controls["reference_passed"]),
                ("starter", ROOT / "seeds" / task, controls["starter_passed"]),
            ]:
                with self.subTest(task=task, kind=kind):
                    report = self.assess(task, path)
                    self.assertEqual(report["assessment"]["passed"], expected)
                    self.assertEqual(report["assessment"]["total"], controls["total"])
                    self.assertEqual(report["execution"]["outcome"], "unknown")

    def test_candidate_local_modules_work_for_all_tasks(self):
        for task, program in TASKS.items():
            with self.subTest(task=task), tempfile.TemporaryDirectory() as temporary:
                path = Path(temporary) / "candidate"
                shutil.copytree(ROOT / "assessments" / task / "reference", path)
                (path / program).rename(path / "implementation.py")
                (path / program).write_text(
                    "from implementation import *\n"
                    "if __name__ == '__main__':\n"
                    "    import runpy\n"
                    "    runpy.run_module('implementation', run_name='__main__')\n")
                report = self.assess(task, path)
                self.assertTrue(report["assessment"]["all_checks_passed"], report)

    def test_import_noise_cannot_corrupt_the_report(self):
        task = "dependency-planner-repair-v1"
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "candidate"
            shutil.copytree(ROOT / "assessments" / task / "reference", path)
            program = path / "planner.py"
            # Appending preserves any future-import placement in the reference.
            program.write_text(program.read_text() + "\nif __name__ != '__main__':\n"
                               "    import os, sys\n"
                               "    print('candidate chatter')\n"
                               "    print('candidate stderr', file=sys.stderr)\n"
                               "    os.write(1, b'direct stdout noise')\n")
            self.assertTrue(self.assess(task, path)["assessment"]["all_checks_passed"])

    def test_missing_program_is_candidate_failure_with_complete_cases(self):
        for task in TASKS:
            with self.subTest(task=task), tempfile.TemporaryDirectory() as temporary:
                report = self.assess(task, Path(temporary))
                self.assertFalse(report["assessment"]["all_checks_passed"])
                self.assertEqual(report["assessment"]["functional"]["passed"], 0)

    def test_passing_partial_workspace_does_not_become_completed_execution(self):
        task = "ledger-repair-v1"
        report = self.assess(task, ROOT / "assessments" / task / "reference",
                             outcome="timed_out", metadata={"harness": "control-fixture"})
        self.assertTrue(report["assessment"]["all_checks_passed"])
        self.assertEqual(report["execution"]["outcome"], "timed_out")

    def test_deadline_is_incomplete_and_cleans_its_container(self):
        def containers():
            output = subprocess.check_output(
                ["docker", "ps", "-aq", "--filter", "label=playtime.assessment=coding-repair-v1"],
                text=True, timeout=15)
            return set(output.splitlines())
        before = containers()
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary)
            (path / "planner.py").write_text("while True:\n    pass\n")
            report, code = checker.assess("dependency-planner-repair-v1", path, timeout=1)
        self.assertEqual(code, 2)
        self.assertEqual(report["assessment"], {"status": "incomplete", "reason": "assessment_timeout"})
        self.assertEqual(report["cleanup"]["status"], "completed")
        self.assertEqual(containers(), before)

    def test_cli_cancellation_cleans_the_owned_container(self):
        def containers():
            return set(subprocess.check_output(
                ["docker", "ps", "-q", "--filter", "label=playtime.assessment=coding-repair-v1"],
                text=True, timeout=15).splitlines())
        before = containers()
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary)
            (path / "planner.py").write_text("while True:\n    pass\n")
            process = subprocess.Popen(
                [sys.executable, str(ROOT / "scripts/check-coding-repair.py"),
                 "dependency-planner-repair-v1", str(path)],
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            try:
                deadline = time.monotonic() + 15
                while not (containers() - before):
                    self.assertIsNone(process.poll(), "assessment exited before container startup")
                    self.assertLess(time.monotonic(), deadline, "container did not start")
                    time.sleep(0.05)
                process.send_signal(signal.SIGTERM)
                output, errors = process.communicate(timeout=25)
                self.assertEqual(process.returncode, 2, errors)
                report = json.loads(output)
                self.assertEqual(report["assessment"], {
                    "status": "incomplete", "reason": "assessment_interrupted"})
                self.assertEqual(report["cleanup"]["status"], "completed")
                self.assertEqual(containers(), before)
            finally:
                if process.poll() is None:
                    process.send_signal(signal.SIGTERM)
                    process.communicate(timeout=25)


if __name__ == "__main__":
    unittest.main()
