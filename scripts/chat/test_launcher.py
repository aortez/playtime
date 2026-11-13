"""CPU lifecycle fixtures for the standalone owner wrapper, without SSH or GPUs."""

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

SOURCE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("playtime_chat_launcher", SOURCE / "run-strata.py")
launcher = importlib.util.module_from_spec(spec)
spec.loader.exec_module(launcher)


class LauncherTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.package = self.root / "package"
        self.package.mkdir()
        for name in ("run-strata.py", "verify-strata-host.py", "presets.json"):
            shutil.copyfile(SOURCE / name, self.package / name)
        self.lock = {"identity": {"model": "fixture", "context_tokens": 163840}, "host": {"digest": "reviewed"}}
        (self.package / "framework-strata-gyro-m-fast-v1.lock.json").write_text(json.dumps(self.lock))
        (self.package / "general.lock.json").write_text("{}")
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.write_program(self.bin / "ssh", r'''
import json, os, pathlib, select, shlex, sys, time
root = pathlib.Path(os.environ['FIXTURE_ROOT'])
args = shlex.split(sys.argv[-1])
mode = os.environ.get('FIXTURE_MODE', '')
if '--status' in args:
    (root / 'status').touch()
    print(json.dumps({'state': 'starts_on_launch'}))
elif args[1] == '-':
    sys.stdin.read()
    (root / 'verified').touch()
    if mode == 'verify-failure':
        sys.exit(1)
    print(json.dumps({'host': {'digest': 'changed' if mode == 'mismatch' else 'reviewed'}}))
else:
    (root / 'started').touch()
    if mode == 'busy':
        print('Another session owns Framework', file=sys.stderr)
        sys.exit(1)
    print('Gyro-M: starting framework-gyro-m-fast; logs: ' + str(root / '.local/state/codex-framework-gyro/20261113T123456Z-123'), file=sys.stderr, flush=True)
    print('GYRO_READY ' + json.dumps({'model':'fixture', 'context':163840, 'drafting':'dflash2'}), flush=True)
    while True:
        if mode == 'owner-loss' and (root / 'client').exists():
            sys.exit(1)
        if select.select([sys.stdin], [], [], .05)[0] and not sys.stdin.readline():
            break
    (root / 'restored').touch()
    if mode == 'restore-failure':
        sys.exit(1)
''')
        self.write_program(self.package / "playtime-chat", r'''
import json, os, pathlib, signal, sys, time
root = pathlib.Path(os.environ['FIXTURE_ROOT'])
(root / 'client').write_text(json.dumps({'argv': sys.argv[1:], 'cwd': os.getcwd()}))
if os.environ.get('FIXTURE_MODE') in ('interrupt', 'owner-loss'):
    def stop(*args):
        (root / 'client-cleaned').touch()
        sys.exit(130)
    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    while True:
        time.sleep(.05)
''')
        self.env = dict(os.environ, HOME=str(self.root), PATH=str(self.bin) + os.pathsep + os.environ['PATH'], FIXTURE_ROOT=str(self.root))

    def tearDown(self):
        self.temp.cleanup()

    def write_program(self, path, source):
        path.write_text(f"#!{sys.executable}\n" + source)
        path.chmod(0o700)

    def command(self, *args):
        return [sys.executable, str(self.package / "run-strata.py"), *args]

    def run_fixture(self, mode="", *args):
        return subprocess.run(self.command("framework-gyro-m-fast", *args), env=dict(self.env, FIXTURE_MODE=mode), cwd=self.root, capture_output=True, text=True, timeout=15)

    def test_status_never_starts_or_verifies_a_model(self):
        result = subprocess.run(self.command("--status", "framework-gyro-m-fast"), env=self.env, capture_output=True, text=True, timeout=5)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout)['state'], 'starts_on_launch')
        self.assertFalse((self.root / 'started').exists())
        self.assertFalse((self.root / 'verified').exists())
        self.assertFalse((self.root / 'client').exists())

    def test_exact_handoff_preserves_project_and_arguments_then_restores(self):
        result = self.run_fixture("", "--prompt", "literal ; $(stuff)")
        self.assertEqual(result.returncode, 0, result.stderr)
        child = json.loads((self.root / 'client').read_text())
        self.assertEqual(child['cwd'], str(self.root))
        self.assertEqual(child['argv'][-2:], ['--prompt', 'literal ; $(stuff)'])
        self.assertEqual(child['argv'][0], '--endpoint')
        self.assertTrue(child['argv'][1].startswith('http://127.0.0.1:'))
        self.assertNotIn('--profile', child['argv'])
        self.assertTrue((self.root / 'verified').exists())
        self.assertTrue((self.root / 'restored').exists())

    def test_mismatched_artifacts_never_start_client_and_restore(self):
        result = self.run_fixture('mismatch')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('differ from the reviewed model lock', result.stderr)
        self.assertFalse((self.root / 'client').exists())
        self.assertTrue((self.root / 'restored').exists())

    def test_busy_owner_is_not_evicted(self):
        result = self.run_fixture('busy')
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse((self.root / 'client').exists())
        self.assertFalse((self.root / 'verified').exists())

    def test_endpoint_override_rejected_before_startup(self):
        result = self.run_fixture('', '--endpoint=http://elsewhere')
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse((self.root / 'started').exists())

    def test_failed_restoration_is_not_reported_as_success(self):
        result = self.run_fixture('restore-failure')
        self.assertNotEqual(result.returncode, 0)

    def test_verifier_failure_restores_without_starting_client(self):
        result = self.run_fixture('verify-failure')
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse((self.root / 'client').exists())
        self.assertTrue((self.root / 'restored').exists())

    def test_lost_owner_stops_client_and_cleans_local_tools(self):
        result = self.run_fixture('owner-loss')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('lost the inference host owner', result.stderr)
        self.assertTrue((self.root / 'client-cleaned').exists())

    def test_interrupt_waits_for_client_cleanup_and_owner_restoration(self):
        with subprocess.Popen(self.command('framework-gyro-m-fast'), env=dict(self.env, FIXTURE_MODE='interrupt'), stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True) as process:
            try:
                deadline = time.monotonic() + 5
                while not (self.root / 'client').exists():
                    self.assertIsNone(process.poll())
                    self.assertLess(time.monotonic(), deadline)
                    time.sleep(.02)
                process.send_signal(signal.SIGINT)
                _, errors = process.communicate(timeout=5)
                self.assertEqual(process.returncode, 130, errors)
                self.assertTrue((self.root / 'client-cleaned').exists())
                self.assertTrue((self.root / 'restored').exists())
            finally:
                if process.poll() is None:
                    process.kill()

    def test_host_and_readiness_require_exact_reviewed_values(self):
        with self.assertRaises(RuntimeError):
            launcher.check_host({'host': {'digest': 'different'}}, self.lock)
        with self.assertRaises(RuntimeError):
            launcher.check_ready({'model': 'fixture', 'context': 128000, 'drafting': 'dflash2'}, self.lock)


if __name__ == '__main__':
    unittest.main()
