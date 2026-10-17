import http.client
import json
import shutil
import sys
import tempfile
import threading
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from playtime.common import PlaytimeError
from playtime.jobs import RunLock, RunManager, ensure_idle, worker
from playtime.live import events_since, model_health
from playtime.server import ReportServer

ROOT = Path(__file__).resolve().parents[1]


class FakeSystemd:
    def __init__(self):
        self.units = {}
        self.started = []
        self.stopped = []

    def start(self, root, directory):
        self.started.append(directory.name)
        self.units["playtime-" + directory.name + ".service"] = "active"

    def state(self, unit):
        return self.units.get(unit, "inactive")

    def stop(self, unit):
        self.stopped.append(unit)
        self.units[unit] = "deactivating"


class DashboardTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        for name in [
            "experiments",
            "prompts",
            "models",
            "environments",
            "runs",
            "workshop/docs",
            "seeds/assortment-01",
        ]:
            (self.root / name).mkdir(parents=True)
        shutil.copyfile(ROOT / "experiments/pilot.toml", self.root / "experiments/pilot.toml")
        for name, text in {
            "prompts/inspiration.txt": "Make something interesting <script>bad()</script>",
            "prompts/protocol.txt": "Carry memory through files.",
            "workshop/docs/INVENTORY.md": "Python and tools",
            "seeds/assortment-01/seed.txt": "starting point",
            "models/gpt-oss-20b.lock.json": '{"name":"gpt-oss:20b"}',
            "environments/general.lock.json": '{"image_id":"test", "toolsets":["core"]}',
        }.items():
            (self.root / name).write_text(text)
        self.backend = FakeSystemd()
        self.manager = RunManager(self.root, self.backend)
        self.addCleanup(patch.stopall)
        patch(
            "playtime.runner.check",
            return_value=({"image_id": "test", "toolsets": ["core"]}, {"name": "gpt-oss:20b"}),
        ).start()
        patch("playtime.runner.command", return_value=b"{}").start()

    def launch(self, mode="experiment"):
        preview = self.manager.preview("experiments/pilot.toml", mode)
        return self.manager.start(preview["path"], mode, preview["revision"])

    def server(self):
        server = ReportServer(self.root / "runs", port=0, manager=self.manager)
        server.RequestHandlerClass.log_message = lambda *args: None
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()

        def close():
            server.shutdown()
            server.server_close()
            thread.join()

        self.addCleanup(close)
        return server

    def request(self, server, path, *, data=None, headers=None):
        connection = http.client.HTTPConnection("127.0.0.1", server.server_port, timeout=5)
        try:
            connection.request(
                "GET" if data is None else "POST",
                path,
                body=json.dumps(data) if data is not None else None,
                headers=headers or {},
            )
            response = connection.getresponse()
            return response.status, dict(response.getheaders()), response.read()
        finally:
            connection.close()

    def test_freezes_inputs_and_rejects_launches_from_cli_and_second_manager(self):
        launched = self.launch()
        directory = self.manager.directory(launched["id"])
        original = (directory / "inputs/prompt.txt").read_text()
        (self.root / "prompts/inspiration.txt").write_text("changed")
        self.assertEqual((directory / "inputs/prompt.txt").read_text(), original)
        self.assertTrue((directory / "inputs/controller/src/playtime/dashboard.js").exists())
        other = RunManager(self.root, self.backend)
        with self.assertRaisesRegex(PlaytimeError, "already running"):
            other.start("experiments/pilot.toml", "experiment", "unused")
        with RunLock(self.root), self.assertRaisesRegex(PlaytimeError, "already running"):
            ensure_idle(self.root, self.backend)
        self.assertTrue(other.status(directory)["active"])
        self.backend.units.clear()
        self.assertEqual(other.status(directory)["state"], "interrupted")
        self.assertFalse(other.status(directory)["can_stop"])

    def test_cli_lock_blocks_launch_and_changed_preview_cannot_start(self):
        preview = self.manager.preview("experiments/pilot.toml")
        with RunLock(self.root), self.assertRaisesRegex(PlaytimeError, "already running"):
            self.manager.start(preview["path"], "experiment", preview["revision"])
        (self.root / "seeds/assortment-01/seed.txt").write_text("changed")
        with self.assertRaisesRegex(PlaytimeError, "inputs changed"):
            self.manager.start(preview["path"], "experiment", preview["revision"])
        self.assertEqual(self.backend.started, [])

    def test_service_failure_is_visible_and_unknown_service_state_blocks_new_work(self):
        with patch.object(
            self.backend, "start", side_effect=PlaytimeError("No user service manager")
        ):
            run = self.launch()
        record = self.manager.status(self.manager.directory(run["id"]))
        self.assertEqual(record["state"], "failed")
        self.assertIn("No user service manager", record["error"])
        with patch.object(self.backend, "state", side_effect=PlaytimeError("Unavailable")):
            self.assertTrue(self.manager.status(self.manager.directory(run["id"]))["active"])
            with self.assertRaisesRegex(PlaytimeError, "Unavailable"):
                self.launch()

    def test_stop_targets_only_the_owned_service_and_is_visible_after_reconnect(self):
        run = self.launch()
        self.manager.stop(run["id"])
        other = RunManager(self.root, self.backend)
        self.assertEqual(other.status(other.directory(run["id"]))["state"], "stopping")
        self.assertEqual(self.backend.stopped, ["playtime-" + run["id"] + ".service"])
        self.backend.units.clear()
        self.assertEqual(other.status(other.directory(run["id"]))["state"], "interrupted")
        with self.assertRaisesRegex(PlaytimeError, "not an active"):
            other.stop(run["id"])

    def test_worker_rejects_modified_archived_inputs_before_model_or_tools(self):
        run = self.launch()
        directory = self.manager.directory(run["id"])
        (directory / "inputs/prompt.txt").write_text("tampered")
        with patch("playtime.runner.execute_prepared") as execute:
            with self.assertRaisesRegex(PlaytimeError, "Archived inputs changed"):
                worker(self.root, run["id"])
            execute.assert_not_called()
        self.assertIn(
            "Archived inputs changed", json.loads((directory / "job.json").read_text())["error"]
        )

    def test_incremental_events_wait_for_complete_records_and_do_not_send_history(self):
        directory = self.manager.directory(self.launch()["id"])
        record = {
            "sequence": 0,
            "kind": "model_request",
            "payload": {"messages": ["private history"], "options": {"num_predict": 8192}},
        }
        path = directory / "events.jsonl"
        line = json.dumps(record) + "\n"
        path.write_text(
            line + '{"sequence":1,"kind":"model_chunk","chunk":{"message":{"thinking":"caf'
        )
        first = events_since(directory, 0)
        self.assertEqual(first["cursor"], len(line.encode()))
        self.assertNotIn("private history", json.dumps(first))
        self.assertEqual(first["events"][0]["maximum"], 8192)
        self.assertEqual(events_since(directory, first["cursor"])["events"], [])
        with path.open("a") as target:
            target.write('é"}}}\n')
        second = events_since(directory, first["cursor"])
        self.assertEqual(second["events"][0]["message"]["thinking"], "café")
        self.assertEqual(len(events_since(directory, 0)["events"]), 2)
        for cursor in [-1, 2, path.stat().st_size + 1]:
            with self.assertRaises(PlaytimeError):
                events_since(directory, cursor)

    def test_web_launch_requires_origin_token_and_known_recipe(self):
        server = self.server()
        preview = self.manager.preview("experiments/pilot.toml", "scripted-smoke")
        payload = {
            "recipe": preview["path"],
            "mode": preview["mode"],
            "revision": preview["revision"],
        }
        good = {
            "Origin": f"http://127.0.0.1:{server.server_port}",
            "X-Playtime-Token": server.token,
            "Content-Type": "application/json",
        }
        for headers in [
            {},
            {**good, "Origin": "https://evil.example"},
            {**good, "X-Playtime-Token": "bad"},
            {**good, "Host": "evil.example"},
        ]:
            self.assertEqual(
                self.request(server, "/api/launch", data=payload, headers=headers)[0], 403
            )
        self.assertEqual(self.backend.started, [])
        bad = {**payload, "recipe": "../../outside.toml"}
        self.assertEqual(self.request(server, "/api/launch", data=bad, headers=good)[0], 409)
        status, _, body = self.request(server, "/api/launch", data=payload, headers=good)
        self.assertEqual(status, 201, body)
        run = json.loads(body)
        self.assertEqual(self.request(server, "/api/launch", data=payload, headers=good)[0], 409)
        status, headers, body = self.request(server, run["url"])
        self.assertEqual(status, 200)
        self.assertIn("connect-src 'self'", headers["Content-Security-Policy"])
        self.assertNotIn(b"<script>bad()", body)
        self.assertEqual(self.request(server, "/api/run/../../events")[0], 404)
        status, _, body = self.request(server, f"/api/run/{run['id']}/stop", data={}, headers=good)
        self.assertEqual(status, 202, body)

    def test_model_health_distinguishes_unloaded_cpu_and_gpu(self):
        recipe = self.manager.preview("experiments/pilot.toml")["recipe"]
        for models, expected in [
            ([], "unloaded"),
            ([{"name": "gpt-oss:20b", "size": 10, "size_vram": 0}], "cpu"),
            ([{"name": "gpt-oss:20b", "size": 10, "size_vram": 10}], "gpu"),
        ]:
            with patch("playtime.live.Ollama.request", return_value={"models": models}):
                self.assertEqual(model_health(recipe)["state"], expected)


if __name__ == "__main__":
    unittest.main()
