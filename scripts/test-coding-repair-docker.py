"""Real assessment controls. All candidate execution stays inside Docker."""
import importlib.util
import json
import os
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
         "byte-range-repair-v1": "ranges.py", "async-singleflight-v1": "singleflight.py",
         "streaming-jsonlines-v1": "jsonlines.py", "shortest-walk-v1": "shortestwalk.py"}


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

    def test_imported_graders_reject_targeted_behavioral_faults(self):
        mutations = [
            ("async-singleflight-v1", "return await asyncio.shield(task)", "return await task", "one_cancel"),
            ("async-singleflight-v1", "task is None or task.done()", "task is None", "completion_cleanup_race"),
            ("async-singleflight-v1", "done.exception()", "pass", "orphan_error"),
            ("async-singleflight-v1", "if self._tasks.get(key) is done:", "if key in self._tasks:", "completion_cleanup_race"),
            ("streaming-jsonlines-v1", ", parse_constant=bad_constant", "", "nan"),
            ("streaming-jsonlines-v1", "self.closed = True\n            self.pending.clear()", "self.closed = False\n            self.pending.clear()", "bad_json"),
            ("streaming-jsonlines-v1", "out = []", "unbounded_copy = bytes(bytearray(chunk))\n            out = []", "bounded_allocation"),
            ("streaming-jsonlines-v1", "> self.limit:", "> self.limit + 1:", "exact_limits"),
            ("shortest-walk-v1", "if target in affected else", "if affected else", "cycle_cannot_reach_target"),
            ("shortest-walk-v1", "else distance[target]", "else (float(distance[target]) if distance[target] is not None else None)", "above_float_precision"),
        ]
        for task, before, after, failed_case in mutations:
            with self.subTest(task=task, case=failed_case), tempfile.TemporaryDirectory() as temporary:
                path = Path(temporary) / "candidate"
                shutil.copytree(ROOT / "assessments" / task / "reference", path)
                program = path / TASKS[task]
                source = program.read_text()
                self.assertEqual(source.count(before), 1)
                program.write_text(source.replace(before, after))
                report = self.assess(task, path)
                failed = [row["case"] for row in report["assessment"]["results"] if not row["passed"]]
                self.assertIn(failed_case, failed)

    def test_readme_directory_is_rejected_without_rewriting_historical_checks(self):
        for task in ("dependency-planner-repair-v1", "byte-range-repair-v1"):
            with self.subTest(task=task), tempfile.TemporaryDirectory() as temporary:
                path = Path(temporary) / "candidate"
                shutil.copytree(ROOT / "assessments" / task / "reference", path)
                readme = path / "README.md"
                readme.unlink()
                readme.mkdir()
                for number in range(8):
                    (readme / f"placeholder-{number}").touch()
                self.assertGreater(readme.stat().st_size, 50)
                old = self.assess(task, path, assessment_revision=2)
                current = self.assess(task, path)
                self.assertEqual(old["task"]["assessment_revision"], 2)
                self.assertTrue(old["assessment"]["all_checks_passed"])
                self.assertEqual(current["task"]["assessment_revision"], checker.CURRENT_REVISION)
                self.assertEqual([row["case"] for row in current["assessment"]["results"]
                                  if not row["passed"]], ["readme"])
                self.assertEqual(current["assessment"]["functional"], old["assessment"]["functional"])

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

    def test_imported_modules_support_dataclasses_and_self_import(self):
        for task in ("dependency-planner-repair-v1", "byte-range-repair-v1"):
            with self.subTest(task=task), tempfile.TemporaryDirectory() as temporary:
                path = Path(temporary) / "candidate"
                shutil.copytree(ROOT / "assessments" / task / "reference", path)
                program = path / TASKS[task]
                program.write_text(
                    "from __future__ import annotations\n"
                    "from dataclasses import dataclass\n"
                    "@dataclass\n"
                    "class Marker:\n"
                    "    value: int\n"
                    "if __name__ != '__main__':\n"
                    "    import importlib, sys\n"
                    "    assert importlib.import_module(__name__) is sys.modules[__name__]\n"
                    + program.read_text())
                self.assertTrue(self.assess(task, path)["assessment"]["all_checks_passed"])

    def test_executable_local_helper_is_preserved(self):
        task = "ledger-repair-v1"
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "candidate"
            shutil.copytree(ROOT / "assessments" / task / "reference", path)
            program = path / "report.py"
            helper = path / "implementation.py"
            helper.write_text("#!/usr/bin/env python3\n" + program.read_text())
            helper.chmod(0o755)
            program.write_text(
                "import subprocess, sys\n"
                "from pathlib import Path\n"
                "raise SystemExit(subprocess.call([str(Path(__file__).with_name('implementation.py')),\n"
                "                                  *sys.argv[1:]]))\n")
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

    def test_cancellation_during_removal_finishes_cleanup_before_scratch_removal(self):
        docker = shutil.which("docker")
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            candidate = root / "candidate"
            candidate.mkdir()
            (candidate / "planner.py").write_text("while True:\n    pass\n")
            shim = root / "bin"
            shim.mkdir()
            marker, release = root / "removing", root / "release"
            # Trusted transport fixture: pause only the cleanup client. Candidate
            # code still executes in the actual locked Docker container.
            wrapper = shim / "docker"
            wrapper.write_text(
                f"#!{sys.executable}\n"
                "import os, sys, time\n"
                "from pathlib import Path\n"
                "if sys.argv[1:3] == ['rm', '--force']:\n"
                f"    Path({str(marker)!r}).write_text(sys.argv[-1])\n"
                "    deadline = time.monotonic() + 10\n"
                f"    while not Path({str(release)!r}).exists() and time.monotonic() < deadline:\n"
                "        time.sleep(0.02)\n"
                f"os.execv({docker!r}, [{docker!r}, *sys.argv[1:]])\n")
            wrapper.chmod(0o755)
            process = subprocess.Popen(
                [sys.executable, str(ROOT / "scripts/check-coding-repair.py"),
                 "dependency-planner-repair-v1", str(candidate), "--assessment-timeout", "1"],
                env={**os.environ, "PATH": str(shim) + os.pathsep + os.environ["PATH"]},
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
                start_new_session=True)
            name = None
            try:
                deadline = time.monotonic() + 20
                while not marker.exists() or not marker.read_text():
                    self.assertIsNone(process.poll(), "assessment exited before cleanup")
                    self.assertLess(time.monotonic(), deadline, "cleanup did not start")
                    time.sleep(0.02)
                name = marker.read_text()
                state = json.loads(subprocess.check_output(
                    [docker, "inspect", name], text=True, timeout=15))[0]
                self.assertTrue(state["State"]["Running"])
                for mount in state["Mounts"]:
                    if mount["Destination"] in ("/candidate", "/assessment"):
                        self.assertTrue(Path(mount["Source"]).is_dir(), "snapshot removed before container")
                os.killpg(process.pid, signal.SIGTERM)
                time.sleep(0.05)
                os.killpg(process.pid, signal.SIGINT)
                release.touch()
                output, errors = process.communicate(timeout=25)
                self.assertEqual(process.returncode, 2, errors)
                report = json.loads(output)
                self.assertEqual(report["assessment"], {
                    "status": "incomplete", "reason": "assessment_interrupted"})
                self.assertEqual(report["cleanup"]["status"], "completed")
                remaining = subprocess.check_output(
                    [docker, "ps", "-aq", "--filter", f"name=^/{name}$"], text=True, timeout=15)
                self.assertEqual(remaining.strip(), "")
                for mount in state["Mounts"]:
                    if mount["Destination"] in ("/candidate", "/assessment"):
                        self.assertFalse(Path(mount["Source"]).exists())
            finally:
                release.touch()
                if process.poll() is None:
                    process.send_signal(signal.SIGTERM)
                    process.communicate(timeout=25)
                if name is None and marker.exists():
                    name = marker.read_text()
                if name:
                    subprocess.run([docker, "rm", "--force", name], timeout=15,
                                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


if __name__ == "__main__":
    unittest.main()
