import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from playtime.common import StopRun
from playtime.config import load_recipe
from playtime.runner import SessionFull, Supervisor

ROOT = Path(__file__).resolve().parents[1]


class CountingModel:
    def __init__(self):
        self.budgets = []

    def chat(self, payload, timeout, on_chunk):
        count = payload["options"]["num_predict"]
        self.budgets.append(count)
        response = {
            "message": {"role": "assistant", "content": "done"},
            "eval_count": count,
            "done": True,
        }
        on_chunk(response)
        return response


class BudgetTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        recipe = load_recipe(ROOT / "experiments/pilot.toml")
        recipe["limits"].update(generated_tokens=200, tokens_per_call=150, handoff_tokens=50)
        self.runner = Supervisor(Path(self.temporary.name), recipe, {"image_id": "unused"})
        self.addCleanup(self.runner.log.close)
        self.model = CountingModel()
        self.runner.model = self.model

    def test_global_generation_budget_includes_handoff(self):
        messages = [{"role": "user", "content": "hello"}]
        self.runner.request(messages)
        self.assertEqual(self.model.budgets, [150])
        with self.assertRaisesRegex(StopRun, "token_limit"):
            self.runner.request(messages)
        self.runner.request(messages, handoff=True)
        self.assertEqual(self.model.budgets, [150, 50])
        self.assertEqual(self.runner.generated, 200)
        with self.assertRaisesRegex(StopRun, "token_limit"):
            self.runner.request(messages, handoff=True)
        self.assertEqual(self.model.budgets, [150, 50])

    def test_oversized_context_is_not_sent_even_for_handoff(self):
        with self.assertRaises(SessionFull):
            self.runner.request([{"role": "user", "content": "x" * 40000}], handoff=True)
        self.assertEqual(self.model.budgets, [])
        self.assertEqual(self.runner.calls, 0)

    def test_partial_request_marks_usage_unknown(self):
        class PartialModel:
            def chat(self, payload, timeout, on_chunk):
                on_chunk({"message": {"thinking": "unfinished"}})
                raise StopRun("model_timeout")

        self.runner.model = PartialModel()
        with self.assertRaisesRegex(StopRun, "model_timeout"):
            self.runner.request([{"role": "user", "content": "hello"}])
        self.assertFalse(self.runner.usage_complete)

    def test_oversized_memento_is_rejected_without_writing(self):
        message = {
            "tool_calls": [
                {
                    "function": {
                        "name": "end_session",
                        "arguments": {"memento": "x" * 10000, "finish_run": True},
                    }
                }
            ]
        }
        messages = []
        ended, finished = self.runner.apply_calls(message, messages, handoff=True)
        self.assertFalse(ended)
        self.assertFalse(finished)
        self.assertIn("exceeds", messages[-1]["content"])


if __name__ == "__main__":
    unittest.main()
