import copy
import io
import json
import sys
import unittest
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
import test_dashboard as fixtures
from playtime.cli import main
from playtime.common import PlaytimeError, digest_json, tree_manifest, write_json
from playtime.jobs import lock_held
from playtime.pilot import ORDER, execute_pilot, identity, prepare_pilot
from playtime.runner import prepare_run, validate_archive


class PilotTests(unittest.TestCase):
    def setUp(self):
        fixtures.DashboardTests.setUp(self)
        self.recipe = self.manager.preview("experiments/pilot.toml")["recipe"]

    def test_frozen_pairs_differ_only_in_condition_seed_and_membership(self):
        directory = prepare_pilot(self.root, self.recipe)
        plan = json.loads((directory / "pilot.json").read_text())
        self.assertEqual([(r["seed"], r["condition"]) for r in plan["runs"]], ORDER)
        (self.root / "prompts/inspiration.txt").write_text("later edit")
        for entry in plan["runs"]:
            archive = self.root / "runs" / entry["id"]
            manifest = json.loads((archive / "manifest.json").read_text())
            validate_archive(archive, manifest)
            self.assertEqual(identity(manifest), plan["identity"])
            self.assertEqual(manifest["pairing"]["order"], entry["order"])
            self.assertNotIn("later edit", (archive / "inputs/prompt.txt").read_text())

    def test_all_started_slots_are_retained_even_failed_runs_and_resuming_never_retries(self):
        directory = prepare_pilot(self.root, self.recipe)
        seen = []
        def execute(path, manifest):
            seen.append((manifest["recipe"]["sampling_seed"], manifest["memory_condition"]))
            result = {"stop_reason": "tool_parse_retry_limit", "error": None}
            write_json(path / "result.json", result)
            return path, result
        with patch("playtime.pilot.check"), patch("playtime.pilot.execute_prepared", side_effect=execute):
            execute_pilot(self.root, directory)
            execute_pilot(self.root, directory)
        self.assertEqual(seen, ORDER)

    def test_interrupted_pilot_exits_130_and_resume_only_runs_remaining_slots(self):
        directory = prepare_pilot(self.root, self.recipe)
        plan = json.loads((directory / "pilot.json").read_text())
        first_id = plan["runs"][0]["id"]
        first_run = self.root / "runs" / first_id
        seen = []
        def execute(path, manifest):
            seen.append((manifest["recipe"]["sampling_seed"], manifest["memory_condition"]))
            result = {
                "stop_reason": "interrupted" if len(seen) == 1 else "agent_finished",
                "error": None,
            }
            write_json(path / "result.json", result)
            return path, result
        argv = ["--root", str(self.root), "pilot-run", str(directory)]
        errors = io.StringIO()
        with (
            patch("playtime.pilot.check"),
            patch("playtime.pilot.execute_prepared", side_effect=execute),
            patch("playtime.cli.signal.signal"),
            redirect_stdout(io.StringIO()),
            redirect_stderr(errors),
        ):
            self.assertEqual(main(argv), 130)
            self.assertEqual(seen, ORDER[:1])
            progress = json.loads((directory / "progress.json").read_text())
            self.assertEqual(set(progress), {first_id})
            interrupted = progress[first_id]
            self.assertEqual(interrupted["state"], "finished")
            self.assertEqual(interrupted["result"]["stop_reason"], "interrupted")
            self.assertIn("finished", interrupted)
            self.assertEqual(json.loads((first_run / "job.json").read_text())["state"], "finished")
            self.assertFalse(lock_held(self.root))
            result_bytes = (first_run / "result.json").read_bytes()

            self.assertEqual(main(argv), 0)
            self.assertEqual(seen, ORDER)
            resumed = json.loads((directory / "progress.json").read_text())
            self.assertEqual(len(resumed), len(ORDER))
            self.assertEqual(resumed[first_id], interrupted)
            self.assertEqual((first_run / "result.json").read_bytes(), result_bytes)
            self.assertEqual(main(argv), 0)
            self.assertEqual(seen, ORDER)
        self.assertIn("playtime: interrupted", errors.getvalue())

    def test_manifest_recipe_tampering_and_input_tampering_fail_before_execution(self):
        directory, manifest = prepare_run(self.root, self.recipe)
        altered = copy.deepcopy(manifest)
        altered["recipe"]["memory_condition"] = "self_plus_evidence"
        with self.assertRaisesRegex(PlaytimeError, "recipe or memory condition"):
            validate_archive(directory, altered)
        (directory / "inputs/recipe.json").write_text("{}")
        with self.assertRaisesRegex(PlaytimeError, "Archived inputs"):
            prepare_run(self.root, self.recipe, previous=directory)

    def test_old_archives_without_condition_remain_repeatable(self):
        directory, manifest = prepare_run(self.root, self.recipe)
        manifest.pop("memory_condition")
        manifest["recipe"].pop("memory_condition")
        write_json(directory / "inputs/recipe.json", manifest["recipe"])
        manifest["inputs"] = tree_manifest(directory / "inputs")
        write_json(directory / "manifest.json", manifest)
        validate_archive(directory, manifest)
        _, repeated = prepare_run(self.root, self.recipe, previous=directory)
        self.assertEqual(repeated["memory_condition"], "self")

    def test_frozen_plan_rejects_changed_manifest_before_any_run(self):
        directory = prepare_pilot(self.root, self.recipe)
        plan = json.loads((directory / "pilot.json").read_text())
        path = self.root / "runs" / plan["runs"][-1]["id"] / "manifest.json"
        manifest = json.loads(path.read_text())
        manifest["model"]["name"] = "different"
        write_json(path, manifest)
        with patch("playtime.pilot.execute_prepared") as execute:
            with self.assertRaisesRegex(PlaytimeError, "identities changed"):
                execute_pilot(self.root, directory)
            execute.assert_not_called()

    def assert_pilot_rejected(self, directory, message):
        with patch("playtime.pilot.check") as check, patch("playtime.pilot.execute_prepared") as execute:
            with self.assertRaisesRegex(PlaytimeError, message):
                execute_pilot(self.root, directory)
            check.assert_not_called()
            execute.assert_not_called()
        self.assertFalse((directory / "progress.json").exists())

    def test_frozen_plan_rejects_swapped_archives_with_unchanged_slot_labels(self):
        directory = prepare_pilot(self.root, self.recipe)
        plan = json.loads((directory / "pilot.json").read_text())
        for key in ["id", "manifest_sha256"]:
            plan["runs"][0][key], plan["runs"][1][key] = plan["runs"][1][key], plan["runs"][0][key]
        write_json(directory / "pilot.json", plan)
        self.assert_pilot_rejected(directory, "membership")

    def test_frozen_plan_rejects_duplicate_run_ids(self):
        directory = prepare_pilot(self.root, self.recipe)
        plan = json.loads((directory / "pilot.json").read_text())
        for key in ["id", "manifest_sha256"]:
            plan["runs"][-1][key] = plan["runs"][0][key]
        write_json(directory / "pilot.json", plan)
        self.assert_pilot_rejected(directory, "unique")

    def test_frozen_plan_checks_recipe_seed_and_condition_against_slot(self):
        directory = prepare_pilot(self.root, self.recipe)
        plan = json.loads((directory / "pilot.json").read_text())
        entry = plan["runs"][-1]
        archive = self.root / "runs" / entry["id"]
        original = json.loads((archive / "manifest.json").read_text())
        for key, value in [("sampling_seed", 1042), ("memory_condition", "self")]:
            with self.subTest(field=key):
                manifest = copy.deepcopy(original)
                manifest["recipe"][key] = value
                manifest["memory_condition"] = manifest["recipe"]["memory_condition"]
                write_json(archive / "inputs/recipe.json", manifest["recipe"])
                manifest["inputs"] = tree_manifest(archive / "inputs")
                write_json(archive / "manifest.json", manifest)
                entry["manifest_sha256"] = digest_json(manifest)
                write_json(directory / "pilot.json", plan)
                self.assert_pilot_rejected(directory, "membership")

    def test_frozen_plan_checks_pairing_in_plan_manifest_and_inputs(self):
        directory = prepare_pilot(self.root, self.recipe)
        original_plan = json.loads((directory / "pilot.json").read_text())
        archive = self.root / "runs" / original_plan["runs"][-1]["id"]
        original_manifest = json.loads((archive / "manifest.json").read_text())
        for source in ["plan", "manifest", "inputs"]:
            for key, value in [("pilot", "another-pilot"), ("pair", 1), ("order", 1)]:
                with self.subTest(source=source, field=key):
                    plan = copy.deepcopy(original_plan)
                    manifest = copy.deepcopy(original_manifest)
                    pairing = copy.deepcopy(manifest["pairing"])
                    targets = {"plan": plan["runs"][-1], "manifest": manifest["pairing"], "inputs": pairing}
                    targets[source][key] = value
                    write_json(archive / "inputs/pairing.json", pairing)
                    manifest["inputs"] = tree_manifest(archive / "inputs")
                    write_json(archive / "manifest.json", manifest)
                    plan["runs"][-1]["manifest_sha256"] = digest_json(manifest)
                    write_json(directory / "pilot.json", plan)
                    self.assert_pilot_rejected(directory, "membership")
