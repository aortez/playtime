import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from playtime.context import ContextWindow, encoded_size, fit_tool_result, tool_message
from playtime.tools import TOOLS


class ContextTests(unittest.TestCase):
    def test_measured_prefix_is_reused_only_for_the_same_conversation(self):
        window = ContextWindow()
        messages = [{"role": "user", "content": "Explore"}]
        response = {
            "prompt_eval_count": 100,
            "eval_count": 20,
            "message": {"role": "assistant", "content": "Look around"},
        }
        window.observe(messages, TOOLS, response)
        tail = [{"role": "tool", "content": "Evidence"}]
        current = messages + [response["message"]] + tail
        self.assertEqual(
            window.estimate(current, TOOLS),
            (120 + encoded_size(tail), "server_usage_plus_new_bytes"),
        )
        self.assertEqual(
            window.estimate(current, [TOOLS[-1]])[0],
            120 + encoded_size(tail) + encoded_size([TOOLS[-1]]),
        )
        fresh = [{"role": "user", "content": "New session"}]
        self.assertEqual(window.estimate(fresh, TOOLS)[1], "input_bytes")
        current[0]["content"] = "Changed history"
        self.assertEqual(window.estimate(current, TOOLS)[1], "input_bytes")

    def test_new_results_fit_after_both_json_encodings_and_keep_failure_metadata(self):
        result = {
            "stdout": '"\\\n\x00😀' * 1000,
            "stderr": "failure " * 1000,
            "exit_code": 1,
            "timed_out": False,
        }
        for maximum in (512, 1024, 12000):
            limited = fit_tool_result("exec", result, maximum)
            self.assertLessEqual(encoded_size(tool_message("exec", limited)), maximum)
            self.assertTrue(limited["context_truncated"])
            self.assertEqual(limited["exit_code"], 1)
            self.assertFalse(limited["timed_out"])
        small = {"stdout": "ok", "exit_code": 0}
        self.assertIs(fit_tool_result("exec", small, 512), small)
