"""Synthetic migration contracts, exercised against the pinned Python behavior."""

import hashlib
import io
import json
import sys
import unittest
from contextlib import redirect_stdout
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
import test_evidence as evidence_fixtures
import test_recovery as recovery_fixtures
from playtime.context import ContextWindow
from playtime.runner import SessionFull
from playtime.tools import TOOLS

FIXTURES = Path(__file__).resolve().parents[1] / "crates/playtime-protocol/tests/fixtures"


def recovery_excerpt():
    fixture = recovery_fixtures.RecoveryTests()
    fixture.setUp()
    try:
        result = fixture.execute([
            recovery_fixtures.parse_error(),
            recovery_fixtures.completed(finish=False),
            recovery_fixtures.completed(),
        ])
        events = [json.loads(line) for line in (fixture.directory / "events.jsonl").read_text().splitlines()]
        return {
            "result": {key: result[key] for key in [
                "stop_reason", "generated_tokens", "token_budget_charged",
                "unaccounted_token_reserve", "unaccounted_model_calls", "token_usage_complete",
                "model_calls", "tool_parse_errors",
            ]},
            "requests": [{
                "call": event["call"], "handoff": event["handoff"],
                "seed": event["payload"]["options"]["seed"],
                "num_predict": event["payload"]["options"]["num_predict"],
                "message_count": len(event["payload"]["messages"]),
                "tools": [tool["function"]["name"] for tool in event["payload"]["tools"]],
            } for event in events if event["kind"] == "model_request"],
            "session_starts": [{key: event[key] for key in ["kind", "session", "memory"]}
                               for event in events if event["kind"] == "session_start"],
            "handoffs": [{key: event[key] for key in ["kind", "session", "arguments"]}
                         for event in events if event["kind"] == "tool_request"
                         and event["name"] == "end_session"],
            "model_errors": [{key: event[key] for key in [
                "kind", "reason", "recovery_allowed", "token_budget_charge",
                "unaccounted_model_calls", "unaccounted_token_reserve",
            ]} for event in events if event["kind"] == "model_error"],
        }
    finally:
        fixture.doCleanups()


def context_seed_excerpt():
    fixture = recovery_fixtures.RecoveryTests()
    fixture.setUp()
    try:
        fixture.recipe["sampling_seed"] = 2**31 - 1
        fixture.runner.model = recovery_fixtures.SequenceModel([
            recovery_fixtures.completed(handoff=False),
            recovery_fixtures.completed(handoff=False),
        ])
        messages = [{"role": "user", "content": "Explore"}]
        fixture.runner.request(messages)
        fixture.runner.request(messages)
        try:
            fixture.runner.request([{"role": "user", "content": "x" * 100000}])
        except SessionFull as error:
            guard = str(error)
        else:
            raise AssertionError("context guard did not stop dispatch")
        window = ContextWindow()
        response = {"prompt_eval_count": 100, "eval_count": 20, "message": {"role": "assistant", "content": "Look"}}
        window.observe(messages, TOOLS, response)
        return {
            "request_seeds": [request["options"]["seed"] for request in fixture.runner.model.requests],
            "context_guard": guard,
            "calls_after_rejection": fixture.runner.calls,
            "same_context_basis": window.estimate(messages + [response["message"]], TOOLS)[1],
            "fresh_context_basis": window.estimate([{"role": "user", "content": "New session"}], TOOLS)[1],
        }
    finally:
        fixture.doCleanups()


def evidence_excerpt():
    fixture = evidence_fixtures.EvidenceTests()
    fixture.setUp()
    try:
        with redirect_stdout(io.StringIO()):
            fixture.execute([recovery_fixtures.completed(finish=False), recovery_fixtures.completed()])
        requests = fixture.runner.model.requests
        folder = fixture.directory / "sessions/002"
        evidence = (folder / "evidence-received.txt").read_text()
        if requests[1]["messages"][-1] != {"role": "user", "content": evidence}:
            raise AssertionError("received evidence differs from delivered evidence")
        return {
            "message_counts": [len(request["messages"]) for request in requests],
            "incoming_memory": json.loads((folder / "memory-received.json").read_text()),
            "received_evidence": evidence,
            "evidence_bytes": len(evidence.encode()),
            "saved_memento": (folder / "memento.txt").read_text(),
        }
    finally:
        fixture.doCleanups()


class RustFoundationTests(unittest.TestCase):
    def test_synthetic_legacy_excerpts_match_the_existing_controller(self):
        for name, collect in [
            ("recovery", recovery_excerpt),
            ("context-seed", context_seed_excerpt),
            ("evidence", evidence_excerpt),
        ]:
            with self.subTest(scenario=name):
                expected = json.loads((FIXTURES / "legacy" / f"{name}.json").read_text())
                self.assertEqual(collect(), expected)

    def test_all_synthetic_content_references_identify_exact_stored_bytes(self):
        root = FIXTURES / "two-session"

        def verify(value):
            if isinstance(value, dict):
                if {"path", "sha256", "size_bytes"} <= value.keys():
                    data = (root / value["path"]).read_bytes()
                    self.assertEqual(len(data), value["size_bytes"], value["path"])
                    self.assertEqual(hashlib.sha256(data).hexdigest(), value["sha256"], value["path"])
                for child in value.values():
                    verify(child)
            elif isinstance(value, list):
                for child in value:
                    verify(child)

        verify(json.loads((root / "inputs/manifest.json").read_bytes()))
        for line in (root / "events.jsonl").read_text().splitlines():
            verify(json.loads(line))

    def test_manifest_identity_is_its_stored_bytes_not_a_new_serialization(self):
        root = FIXTURES / "two-session"
        stored = (root / "inputs/manifest.json").read_bytes()
        reference = json.loads((root / "events.jsonl").read_text().splitlines()[0])["data"]["input_manifest"]
        serialized = json.dumps(json.loads(stored), sort_keys=True, separators=(",", ":")).encode()
        self.assertEqual(hashlib.sha256(stored).hexdigest(), reference["sha256"])
        self.assertNotEqual(hashlib.sha256(serialized).hexdigest(), reference["sha256"])
        self.assertNotEqual(hashlib.sha256(stored + b"\n").hexdigest(), reference["sha256"])


if __name__ == "__main__":
    unittest.main()
