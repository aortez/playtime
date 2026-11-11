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
from unittest.mock import patch

ROOT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location("coding_repair", ROOT / "scripts/check-coding-repair.py")
checker = importlib.util.module_from_spec(spec)
spec.loader.exec_module(checker)
TASKS = {"ledger-repair-v1": "report.py", "dependency-planner-repair-v1": "planner.py",
         "byte-range-repair-v1": "ranges.py", "async-singleflight-v1": "singleflight.py",
         "streaming-jsonlines-v1": "jsonlines.py", "shortest-walk-v1": "shortestwalk.py",
         "sql-tombstones-v1": "query.sql", "delta-debugging-v1": "ddmin.py",
         "stable-quadratic-v1": "quadratic.py", "crash-journal-v1": "journal.py",
         "linearizable-kv-v1": "linearize.py", "context-policy-edit-v1": "shipping.py"}


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

    def test_positive_and_negative_controls_at_every_revision(self):
        for task in TASKS:
            folder = ROOT / "assessments" / task
            provenance = json.loads((folder / "provenance.json").read_text())
            for revision in checker.ASSESSMENT_REVISIONS:
                if revision < provenance["assessment_revision"]:
                    continue
                controls = provenance.get("revision_controls", {}).get(
                    str(revision), provenance["controls"])
                for kind, path, expected in [
                    ("reference", folder / "reference", controls["reference_passed"]),
                    ("starter", ROOT / "seeds" / task, controls["starter_passed"]),
                ]:
                    with self.subTest(task=task, revision=revision, kind=kind):
                        report = self.assess(task, path, assessment_revision=revision)
                        self.assertEqual(report["task"]["assessment_revision"], revision)
                        self.assertEqual(report["assessment"]["passed"], expected)
                        self.assertEqual(report["assessment"]["total"], controls["total"])
                        self.assertEqual(report["execution"]["outcome"], "unknown")

    def test_revision_9_fails_ddmin_that_absorbs_predicate_errors(self):
        task = "delta-debugging-v1"
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "candidate"
            path.mkdir()
            (path / "README.md").write_text("Exhaustive-search fixture.\n")
            (path / "ddmin.py").write_text(
                "from itertools import combinations\n"
                "def ddmin(items, fails):\n"
                "    for size in range(len(items) + 1):\n"
                "        for indexes in combinations(range(len(items)), size):\n"
                "            try:\n"
                "                if fails([items[i] for i in indexes]):\n"
                "                    return [items[i] for i in indexes]\n"
                "            except Exception:\n"
                "                pass\n"
                "    return list(items)\n")
            old = self.assess(task, path, assessment_revision=8)
            new = self.assess(task, path)
            self.assertTrue(old["assessment"]["all_checks_passed"])
            self.assertEqual([r["case"] for r in new["assessment"]["results"] if not r["passed"]],
                             ["all_required"])

    def test_revision_9_keeps_single_flight_warnings_in_their_own_case(self):
        task = "async-singleflight-v1"
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "candidate"
            shutil.copytree(ROOT / "assessments" / task / "reference", path)
            program = path / "singleflight.py"
            source = program.read_text()
            before = "if self._tasks.get(key) is done:"
            self.assertEqual(source.count(before), 1)
            program.write_text(source.replace(before, "if key in self._tasks:"))
            old = self.assess(task, path, assessment_revision=8)
            new = self.assess(task, path)
            self.assertIn("synchronous_factory_failure",
                          [r["case"] for r in old["assessment"]["results"] if not r["passed"]])
            self.assertEqual([r["case"] for r in new["assessment"]["results"] if not r["passed"]],
                             ["completion_cleanup_race"])

    def test_revision_9_fails_a_slow_history_case_without_losing_the_rest(self):
        task = "linearizable-kv-v1"
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "candidate"
            shutil.copytree(ROOT / "assessments" / task / "reference", path)
            program = path / "linearize.py"
            source = program.read_text()
            memo = "        if key in impossible:\n            return None\n"
            self.assertEqual(source.count(memo), 1)
            program.write_text(source.replace(memo, ""))
            old, code = checker.assess(task, path, assessment_revision=8, timeout=5)
            self.assertEqual(code, 2)
            self.assertEqual(old["assessment"], {"status": "incomplete", "reason": "assessment_timeout"})
            self.assertEqual(old["cleanup"]["status"], "completed")
            new = self.assess(task, path)
            failed = [r for r in new["assessment"]["results"] if not r["passed"]]
            self.assertEqual([r["case"] for r in failed], ["equivalent_prefixes"])
            self.assertEqual(failed[0]["error"], "case exceeded 1 CPU second")

    def test_revision_9_refuses_to_grade_without_its_interpreter_environment(self):
        task = "ledger-repair-v1"
        real = checker.command
        for change in ("drop_seed", "add_import_path"):
            def launch(args, *rest, **options):
                if args[:2] == ["docker", "create"]:
                    seed = args.index("PYTHONHASHSEED=0")
                    if change == "drop_seed":
                        args = args[:seed - 1] + args[seed + 1:]
                    else:
                        args = args[:seed + 1] + ["--env", "PYTHONPATH=/candidate"] + args[seed + 1:]
                return real(args, *rest, **options)

            with self.subTest(change=change), patch.object(checker, "command", side_effect=launch):
                report, code = checker.assess(task, ROOT / "assessments" / task / "reference")
                self.assertEqual(code, 2)
                self.assertEqual(report["assessment"]["status"], "incomplete")
                self.assertEqual(report["cleanup"]["status"], "completed")

    def test_candidate_local_modules_work_for_all_tasks(self):
        for task, program in TASKS.items():
            if not program.endswith('.py'):
                continue
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

    def test_hard_graders_reject_targeted_faults(self):
        mutations = [
            ("crash-journal-v1", "if (zlib.crc32(payload) & 0xffffffff) != checksum:", "if False:", "checksum_no_resync"),
            ("crash-journal-v1", "else:\n                active.pop(tx)", "else:\n                active.pop(tx)\n                state.clear()", "abort_discard"),
            ("crash-journal-v1", "committed.append(tx)", "committed.append(tx)\n                seen.discard(tx)", "closed_id_reuse"),
            ("crash-journal-v1", "'valid_bytes': offset", "'valid_bytes': len(data)", "every_crash_byte"),
            ("crash-journal-v1", "active[tx].append(record)", "active[tx].append(record)\n                if op == 'put':\n                    state[record[2]] = record[3]", "pending_not_visible"),
            ("linearizable-kv-v1", "other['end'] <= entry['start']", "other['end'] < entry['start']", "touching_intervals"),
            ("linearizable-kv-v1", "key = done, tuple(sorted(state.items()))", "key = done", "state_matters_for_pruning"),
            ("linearizable-kv-v1", "matches = all(state.get(k) == v for k, v in op[1].items())", "matches = any(state.get(k) == v for k, v in op[1].items())", "cas_checks_every_key"),
            ("linearizable-kv-v1", "if matches:", "if True:", "failed_cas_no_writes"),
            ("linearizable-kv-v1", "if suffix is not None:", "if suffix is None:\n                return None\n            if suffix is not None:", "backtrack_legal_write"),
            ("linearizable-kv-v1", "entries = sorted(history, key=lambda e: e['id'])", "entries = sorted(history, key=lambda e: e['id'], reverse=True)", "lexicographic_witness"),
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

    def test_deferred_noop_write_is_versioned_and_rejects_greedy_pruning(self):
        task = "linearizable-kv-v1"
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "candidate"
            shutil.copytree(ROOT / "assessments" / task / "reference", path)
            program = path / "linearize.py"
            source = program.read_text()
            marker = "            suffix = search(done | (1 << i), next_state)"
            self.assertEqual(source.count(marker), 1)
            source = source.replace(marker, marker + "\n            if op[0] == 'put' and next_state == state and suffix is None:\n                return None")
            program.write_text(source)
            old = self.assess(task, path, assessment_revision=6)
            new = self.assess(task, path, assessment_revision=7)
            self.assertTrue(old["assessment"]["all_checks_passed"])
            self.assertNotIn("deferred_noop_write", [r["case"] for r in old["assessment"]["results"]])
            failed = [r["case"] for r in new["assessment"]["results"] if not r["passed"]]
            self.assertIn("deferred_noop_write", failed)

    def test_context_policy_rejects_wrong_rules_and_legacy_regressions(self):
        task = "context-policy-edit-v1"
        mutations = [
            ("(grams - 500 + 249) // 250", "(grams - 500) // 250", "island_rounding"),
            ("return 750 +", "return 900 +", "island_base"),
            ("grams > 5000 or express", "grams > 5000", "island_no_express"),
            ("base = 400 if", "base = 450 if", "legacy_prices"),
        ]
        for before, after, expected in mutations:
            with self.subTest(case=expected), tempfile.TemporaryDirectory() as temporary:
                candidate = Path(temporary) / "candidate"
                shutil.copytree(ROOT / "assessments" / task / "reference", candidate)
                program = candidate / "shipping.py"
                source = program.read_text()
                self.assertEqual(source.count(before), 1)
                program.write_text(source.replace(before, after))
                result = self.assess(task, candidate)
                self.assertIn(expected, [r["case"] for r in result["assessment"]["results"] if not r["passed"]])

    def test_diverse_graders_reject_targeted_faults(self):
        mutations = [
            ("sql-tombstones-v1", "WHERE version <= :cutoff", "WHERE version <= :cutoff AND deleted = 0", "tombstone_wins"),
            ("sql-tombstones-v1", "event_id DESC", "event_id ASC", "equal_version_live_wins"),
            ("sql-tombstones-v1", "version <= :cutoff", "version < :cutoff", "cutoff_inclusive"),
            ("delta-debugging-v1", "if fails([]):", "if False:", "always_fails"),
            ("delta-debugging-v1", "candidate = current[:start] + current[end:]", "candidate = sorted(current[:start] + current[end:])", "ordered_pair"),
            ("stable-quadratic-v1", "q = -0.5 * (b + math.copysign(math.sqrt(discriminant), b))\n    return tuple(sorted((q / a, c / q)))", "s = math.sqrt(discriminant)\n    return tuple(sorted(((-b-s)/(2*a), (-b+s)/(2*a))))", "cancellation_positive"),
            ("stable-quadratic-v1", "return tuple(sorted((q / a, c / q)))", "return q / a, c / q", "ordinary"),
            ("stable-quadratic-v1", "if a == 0 or not all(math.isfinite(x) for x in (a, b, c)):", "if a == 0:", "nonfinite_inputs"),
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
                self.assertIn(failed_case, [row["case"] for row in report["assessment"]["results"] if not row["passed"]])
        with tempfile.TemporaryDirectory() as temporary:
            task = "delta-debugging-v1"
            path = Path(temporary) / "candidate"
            shutil.copytree(ROOT / "assessments" / task / "reference", path)
            (path / "ddmin.py").write_text(
                "def ddmin(items, fails):\n"
                "    if fails([]): return []\n"
                "    result = list(items)\n"
                "    i = 0\n"
                "    while i < len(result):\n"
                "        candidate = result[:i] + result[i+1:]\n"
                "        if fails(list(candidate)): result = candidate\n"
                "        else: i += 1\n"
                "    return result\n")
            report = self.assess(task, path)
            self.assertIn("reconsider_deletions", [row["case"] for row in report["assessment"]["results"] if not row["passed"]])

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
