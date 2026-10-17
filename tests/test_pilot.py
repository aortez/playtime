import copy
import json
import sys
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
import test_dashboard as fixtures
from playtime.common import PlaytimeError, tree_manifest, write_json
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
