"""Opt-in integration checks against the locked workshop image; no model calls."""

import json
import os
import sys
import tempfile
import time
import unittest
import uuid
from pathlib import Path
from unittest.mock import Mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from playtime.common import canonical, command
from playtime.config import load_recipe
from playtime.runner import Supervisor
from playtime.sandbox import Sandbox, initial_archive

ROOT = Path(__file__).resolve().parents[1]


@unittest.skipUnless(os.environ.get("PLAYTIME_DOCKER_TESTS") == "1", "set PLAYTIME_DOCKER_TESTS=1")
class DockerTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.recipe = load_recipe(ROOT / "experiments/pilot.toml")
        self.recipe["workshop"].update(workspace_mb=8, tmp_mb=64, memory_mb=1024)
        self.recipe["limits"].update(command_seconds=20, output_bytes=1024)
        image = json.loads((ROOT / "environments/general.lock.json").read_text())["image_id"]
        initial_archive(None, self.root / "initial.tar")
        self.sandbox = Sandbox(
            image, self.recipe, "test-" + uuid.uuid4().hex[:12], time.monotonic() + 90
        )
        self.sandbox.create(self.root / "initial.tar")
        self.sandbox.start_session(1)

    def tearDown(self):
        self.assertEqual(self.sandbox.close(), [])

    def test_toolchains_and_browser_work_without_network(self):
        result = self.sandbox.tool(
            "exec",
            {
                "command": """python - <<'PY'
import numpy as np
from scipy import signal
from PIL import Image
from playwright.sync_api import sync_playwright
assert np.arange(5).sum() == 10
Image.new('RGB', (10, 10)).save('/work/image.png')
with sync_playwright() as p:
    browser = p.chromium.launch(args=['--no-sandbox'])
    page = browser.new_page()
    page.set_content('<title>workshop-ok</title><button>hello</button>')
    assert page.title() == 'workshop-ok'
    page.locator('button').click()
    page.screenshot(path='/work/browser.png')
    browser.close()
print('toolchains-ok')
PY
node -e 'console.log("node-ok")'
cc --version | head -1
"""
            },
        )
        self.assertEqual(result["exit_code"], 0, result)
        self.assertIn("toolchains-ok", result["stdout"])
        self.assertIn("node-ok", result["stdout"])

    def test_workshop_is_unprivileged_and_output_is_bounded(self):
        result = self.sandbox.tool(
            "exec",
            {"command": "python -c 'import os; assert os.getuid() == 1000; print(\"x\"*100000)'"},
        )
        self.assertEqual(result["exit_code"], 0)
        self.assertTrue(result["output_truncated"])
        self.assertLessEqual(len(result["stdout"].encode()) + len(result["stderr"].encode()), 1024)
        result = self.sandbox.tool("write_file", {"path": "/etc/escape", "content": "no"})
        self.assertIn("error", result)
        result = self.sandbox.tool("exec", {"command": "touch /etc/escape"})
        self.assertNotEqual(result["exit_code"], 0)

    def test_workspace_has_a_hard_capacity(self):
        result = self.sandbox.tool(
            "exec", {"command": "dd if=/dev/zero of=/work/full bs=1M count=32"}
        )
        self.assertNotEqual(result["exit_code"], 0)
        self.assertIn("No space left on device", result["stderr"])

    def test_binary_read_leaves_room_for_handoff_and_persistent_memory(self):
        self.recipe["limits"]["output_bytes"] = 16384
        self.recipe["model"]["context_tokens"] = 65536
        created = self.sandbox.tool(
            "exec",
            {
                "command": """python - <<'PY'
import random
from PIL import Image
Image.frombytes('RGB', (200, 200), random.Random(1042).randbytes(120000)).save('/work/art.png')
PY"""
            },
        )
        self.assertEqual(created["exit_code"], 0, created)
        result = self.sandbox.tool("read_file", {"path": "/work/art.png"})
        self.assertIn("error", result)
        self.assertNotIn("content", result)
        self.assertGreater(result["size_bytes"], 16384)
        self.assertLess(len(canonical(result).encode()), 512)

        runner = Supervisor(self.root, self.recipe, {"image_id": "unused"})
        self.addCleanup(runner.log.close)
        runner.sandbox = self.sandbox
        runner.model = Mock()
        memento = "art.png exists; its contents have not been verified. Next: inspect with Pillow."
        runner.model.chat.return_value = {
            "done": True,
            "eval_count": 20,
            "message": {
                "role": "assistant",
                "tool_calls": [
                    {
                        "function": {
                            "name": "end_session",
                            "arguments": {"memento": memento, "finish_run": False},
                        }
                    }
                ],
            },
        }
        messages = [
            {"role": "user", "content": "Inspect art.png and save a checkpoint."},
            {"role": "tool", "tool_name": "read_file", "content": canonical(result)},
        ]
        message = runner.request(messages, handoff=True)
        self.assertEqual(runner.apply_calls(message, messages, handoff=True), (True, False))
        self.sandbox.stop_session()
        self.sandbox.start_session(2)
        self.assertEqual(self.sandbox.memory(), {"content": memento, "status": "loaded"})

    def test_reset_stops_background_writers_and_preserves_files(self):
        result = self.sandbox.tool(
            "exec",
            {
                "command": "python -u -c 'import time; from pathlib import Path; [(Path(\"/work/counter\").write_text(str(i)), time.sleep(.03)) for i in range(1000)]' >/work/job.log 2>&1 &"
            },
        )
        self.assertEqual(result["exit_code"], 0, result)
        time.sleep(0.2)
        self.sandbox.stop_session()
        self.sandbox.snapshot(self.root / "before.tar")
        self.sandbox.start_session(2)
        first = self.sandbox.tool("read_file", {"path": "/work/counter"})["content"]
        time.sleep(0.2)
        second = self.sandbox.tool("read_file", {"path": "/work/counter"})["content"]
        self.assertEqual(first, second)

    def test_container_deadline_survives_an_idle_controller(self):
        self.sandbox.stop_session()
        self.sandbox.deadline = time.monotonic() + 0.8
        self.sandbox.start_session(2)
        time.sleep(1.5)
        running = (
            command(["docker", "inspect", "--format", "{{.State.Running}}", self.sandbox.active])
            .decode()
            .strip()
        )
        self.assertEqual(running, "false")
        # The separate store still allows a final checkpoint after agent expiry.
        self.sandbox.stop_session()
        self.sandbox.snapshot(self.root / "deadline.tar")
        self.assertTrue((self.root / "deadline.tar").is_file())


if __name__ == "__main__":
    unittest.main()
