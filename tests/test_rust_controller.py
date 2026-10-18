"""Independent Python oracles for the Rust controller's synthetic inputs."""

import json
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "src"))

from playtime.evidence import build_evidence
from playtime.common import canonical
from playtime.tools import TOOLS


def evidence_from_fixture(fixture):
    with tempfile.TemporaryDirectory() as temporary:
        directory = Path(temporary)
        for number, files in [(1, fixture["before"]), (2, fixture["after"])]:
            folder = directory / "sessions" / f"{number:03d}"
            folder.mkdir(parents=True)
            (folder / "workspace.json").write_text(
                json.dumps({"files": files, "changes": {}})
            )
        events = []
        for command in fixture["commands"]:
            source = command["sources"]
            events.extend(
                [
                    {
                        "kind": "tool_request",
                        "session": 2,
                        "sequence": source["request"],
                        "arguments": {"command": command["command"], "cwd": command["cwd"]},
                    },
                    {
                        "kind": "tool_dispatch",
                        "session": 2,
                        "sequence": source["dispatch"],
                        "name": "exec",
                        "request_sequence": source["request"],
                    },
                ]
            )
            if command["result"] is not None:
                events.append(
                    {
                        "kind": "tool_result",
                        "session": 2,
                        "sequence": source["result"],
                        "request_sequence": source["request"],
                        "result": command["result"],
                    }
                )
        (directory / "events.jsonl").write_text(
            "".join(json.dumps(event) + "\n" for event in sorted(events, key=lambda e: e["sequence"]))
        )
        return build_evidence(directory, 2)


class RustControllerTests(unittest.TestCase):
    def test_context_json_spelling_matches_python(self):
        path = ROOT / "crates/playtime-engine/tests/fixtures/canonical.json"
        fixture = json.loads(path.read_text())
        self.assertEqual(canonical(fixture["value"]), fixture["expected"])

    def test_tool_definitions_match_the_pinned_controller(self):
        definitions = json.loads((ROOT / "crates/playtime-engine/src/tools.json").read_text())
        self.assertEqual(definitions, TOOLS)

    def test_bounded_unicode_evidence_matches_python(self):
        path = ROOT / "crates/playtime-engine/tests/fixtures/evidence.json"
        fixture = json.loads(path.read_text())
        evidence = evidence_from_fixture(fixture)
        self.assertEqual(evidence["content"], fixture["expected_content"])
        self.assertLessEqual(evidence["bytes"], 4096)
        for key in ["files_omitted", "commands_omitted", "fields_truncated", "excerpt_bytes_omitted", "upstream_truncated_results"]:
            self.assertGreater(evidence["counts"][key], 0)
