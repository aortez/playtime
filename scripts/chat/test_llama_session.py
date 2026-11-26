"""Owner lifecycle tests for local llama-server sessions, against a fake server."""

import json
import os
from pathlib import Path
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
import unittest

SOURCE = Path(__file__).resolve().parent
# Serves /health and /props from its own flags. The alias "never-ready" keeps
# /health at 503, as llama-server does while it loads a model.
FAKE_SERVER = r'''
import http.server, json
argv = [part.decode() for part in open("/proc/self/cmdline", "rb").read().split(b"\0") if part]
flag = lambda name: argv[argv.index(name) + 1]
alias = flag("--alias")
props = {"model_alias": alias, "total_slots": int(flag("-np")),
         "default_generation_settings": {"n_ctx": int(flag("-c"))}}

class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        loading = self.path == "/health" and alias == "never-ready"
        body = json.dumps({"status": "ok"} if self.path == "/health" else props).encode()
        self.send_response(503 if loading else 200)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, format, *args):
        print(format % args, flush=True)

http.server.HTTPServer(("127.0.0.1", int(flag("--port"))), Handler).serve_forever()
'''


class OwnerTests(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name).resolve()
        self.package = self.root / "package"
        self.package.mkdir()
        shutil.copyfile(SOURCE / "llama-session.py", self.package / "llama-session.py")
        # The owner checks its server's exact command line, so the fake is a
        # binary too: the interpreter in a venv, whose .pth file starts serving
        # before Python could read llama-server's `-m MODEL` as a module.
        self.runtime = self.root / "runtime"
        (self.runtime / "bin").mkdir(parents=True)
        shutil.copyfile(sys.executable, self.runtime / "bin/llama-server")
        (self.runtime / "bin/llama-server").chmod(0o700)
        (self.runtime / "pyvenv.cfg").write_text(
            f"home = {Path(sys.executable).parent}\ninclude-system-site-packages = false\n")
        packages = self.runtime / f"lib/python{sys.version_info.major}.{sys.version_info.minor}/site-packages"
        packages.mkdir(parents=True)
        (packages / "fake_llama_server.py").write_text(FAKE_SERVER)
        (packages / "fake-llama-server.pth").write_text("import fake_llama_server\n")
        self.model = self.root / "model.gguf"
        self.model.write_bytes(b"GGUF")
        profiles = {name: {"llama": {"runtime": str(self.runtime), "model": str(self.model), "alias": alias,
                                     "context": 4096, "threads": 2},
                           "state_parent": f".local/state/playtime-llama/{name}", "model_lock": "unused.lock.json",
                           "startup_timeout": 1 if alias == "never-ready" else 15, "cleanup_timeout": 30}
                    for name, alias in (("fixture", "fixture-model"), ("loading", "never-ready"))}
        profiles["remote"] = {"host": "elsewhere", "helper": "helper.py"}
        (self.package / "presets.json").write_text(json.dumps({"schema_version": 1, "profiles": profiles}))
        self.env = dict(os.environ, HOME=str(self.root))
        self.state = self.root / ".local/state/playtime-llama"

    def helper(self, *args):
        return subprocess.run([sys.executable, "-I", str(self.package / "llama-session.py"), *args],
                              env=self.env, capture_output=True, text=True, timeout=30)

    def status(self):
        result = self.helper("--status", "fixture")
        self.assertEqual(result.returncode, 0, result.stderr)
        return json.loads(result.stdout)

    def start_owner(self, profile="fixture"):
        with socket.socket() as reservation:
            reservation.bind(("127.0.0.1", 0))
            port = reservation.getsockname()[1]
        owner = subprocess.Popen([sys.executable, "-I", str(self.package / "llama-session.py"), profile,
                                  "--port", str(port)], env=self.env, text=True,
                                 stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        self.addCleanup(self.stop_owner, owner)
        return owner, port

    def stop_owner(self, owner):
        if owner.poll() is None:
            owner.kill()
        owner.wait(timeout=10)
        for stream in (owner.stdin, owner.stdout, owner.stderr):
            stream.close()

    def ready(self, owner):
        """The owner's state directory, session record and readiness report."""
        line = owner.stderr.readline()
        prefix = "llama-server: starting fixture; logs: "
        self.assertTrue(line.startswith(prefix), line)
        directory = Path(line[len(prefix):].strip())
        ready = owner.stdout.readline()
        self.assertTrue(ready.startswith("GYRO_READY "), ready)
        record = json.loads((directory / "llama-session.json").read_text())
        return directory, record, json.loads(ready[len("GYRO_READY "):])

    def gone(self, pid):
        # A zombie keeps its /proc entry but loses its command line.
        cmdline = Path(f"/proc/{pid}/cmdline")
        return not cmdline.exists() or cmdline.read_bytes() == b""

    def test_owner_reports_its_server_and_stops_it_on_release(self):
        owner, port = self.start_owner()
        directory, record, ready = self.ready(owner)
        self.assertEqual(ready, {"model": "fixture-model", "parallel": 1, "context": 4096})
        self.assertEqual(directory.parent, self.state / "fixture")
        self.assertRegex(directory.name, r"^\d{8}T\d{6}Z-\d+$")
        self.assertEqual((record["owner_pid"], record["port"], record["environment"]), (owner.pid, port, {}))
        self.assertEqual(record["argv"], [
            str(self.runtime / "bin/llama-server"), "-m", str(self.model), "--alias", "fixture-model",
            "--host", "127.0.0.1", "--port", str(port), "-np", "1", "-c", "4096", "-t", "2", "-fa", "on",
            "--jinja", "--no-context-shift", "--reasoning-format", "deepseek", "--offline",
            "--load-mode", "none", "--no-ui"])
        # The server inherits nothing from the owner's environment.
        self.assertEqual(Path(f"/proc/{record['pid']}/environ").read_bytes(), b"")
        status = self.status()
        self.assertEqual(status["state"], "busy")
        self.assertIn(f"pid {record['pid']}, owner pid {owner.pid}", status["message"])
        owner.stdin.write(".\nGYRO_RELEASE\n")
        owner.stdin.flush()
        self.assertEqual(owner.wait(timeout=20), 0)
        self.assertIn("stopping fixture: released", owner.stderr.read())
        self.assertTrue(self.gone(record["pid"]))
        self.assertEqual(self.status(), {"state": "starts_on_launch"})
        self.assertIn("GET /props", (directory / "server.log").read_text())

    def test_eof_stops_the_server_and_unexpected_input_fails_the_session(self):
        for ending, code, reason in (("", 0, "the launcher closed the session"),
                                     ("park\n", 1, "unexpected launcher input")):
            with self.subTest(reason=reason):
                owner, _ = self.start_owner()
                _, record, _ = self.ready(owner)
                owner.stdin.write(ending)
                owner.stdin.close()
                self.assertEqual(owner.wait(timeout=20), code)
                self.assertIn(reason, owner.stderr.read())
                self.assertTrue(self.gone(record["pid"]))

    def test_a_running_server_refuses_another_owner_without_starting_one(self):
        owner, _ = self.start_owner()
        directory, record, _ = self.ready(owner)
        second, _ = self.start_owner()
        self.assertEqual(second.wait(timeout=20), 1)
        self.assertIn(f"pid {record['pid']}", second.stderr.read())
        self.assertEqual(list(directory.parent.iterdir()), [directory])

    def test_a_killed_owner_takes_its_server_with_it(self):
        owner, _ = self.start_owner()
        _, record, _ = self.ready(owner)
        owner.kill()
        owner.wait(timeout=5)
        deadline = time.monotonic() + 10
        while not self.gone(record["pid"]) and time.monotonic() < deadline:
            time.sleep(.05)
        self.assertEqual(self.status(), {"state": "starts_on_launch"})

    def test_a_server_that_exits_or_never_becomes_ready_fails_the_session(self):
        owner, _ = self.start_owner()
        _, record, _ = self.ready(owner)
        os.kill(record["pid"], signal.SIGKILL)
        self.assertEqual(owner.wait(timeout=20), 1)
        self.assertIn("llama-server exited with -9", owner.stderr.read())
        slow, _ = self.start_owner("loading")
        self.assertEqual(slow.wait(timeout=20), 1)
        self.assertIn("did not become ready in time", slow.stderr.read())
        self.assertEqual(slow.stdout.read(), "")
        record = json.loads(next((self.state / "loading").glob("*/llama-session.json")).read_text())
        self.assertTrue(self.gone(record["pid"]))

    def test_stop_signals_only_servers_whose_owner_is_gone(self):
        processes = {}
        for name, owner in (("owned", os.getpid()), ("orphaned", os.getppid())):
            argv = [sys.executable, "-c", "import time; time.sleep(60)", name]
            process = subprocess.Popen(argv)
            self.addCleanup(lambda process=process: (process.kill(), process.wait(timeout=5)))
            directory = self.state / name / f"20261125T000000Z-{process.pid}"
            directory.mkdir(parents=True)
            started = Path(f"/proc/{process.pid}/stat").read_text().rsplit(")", 1)[1].split()[19]
            # The test is each server's parent, so only the first owner still holds one.
            (directory / "llama-session.json").write_text(json.dumps(
                {"profile": name, "owner_pid": owner, "pid": process.pid, "start_ticks": started, "argv": argv}))
            processes[name] = process
        self.assertIn("no owner; --stop-server stops it", self.status()["message"])
        result = self.helper("--stop")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(processes["orphaned"].wait(timeout=5), -signal.SIGTERM)
        self.assertIsNone(processes["owned"].poll())
        self.assertIn(f"stopping pid {processes['orphaned'].pid}", result.stderr)
        self.assertIn(f"owner pid {os.getpid()}", self.status()["message"])

    def test_unknown_remote_or_malformed_requests_start_nothing(self):
        for args, code, message in ((["--status", "missing"], 1, "missing is not a local llama-server profile"),
                                    (["remote", "--port", "1"], 1, "remote is not a local llama-server profile"),
                                    (["fixture", "--port", "0"], 2, "usage"),
                                    (["fixture"], 2, "usage")):
            with self.subTest(args=args):
                result = self.helper(*args)
                self.assertEqual(result.returncode, code)
                self.assertIn(message, result.stderr)
        self.assertFalse(self.state.exists())


if __name__ == "__main__":
    unittest.main()
