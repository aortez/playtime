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
        self.bird_lock = {"identity": {"model": "fixture", "context_tokens": 131072}, "host": {"digest": "bird-reviewed"}}
        (self.package / "bird-strata-gyro-m-fast-v1.lock.json").write_text(json.dumps(self.bird_lock))
        (self.package / "general.lock.json").write_text("{}")
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.write_program(self.bin / "ssh", r'''
import json, os, pathlib, select, shlex, sys, time
root = pathlib.Path(os.environ['FIXTURE_ROOT'])
args = shlex.split(sys.argv[-1])
mode = os.environ.get('FIXTURE_MODE', '')
if '--stop' in args:
    (root / 'stop-warm').touch()
elif '--status' in args:
    (root / 'status').touch()
    (root / 'status-args').write_text(json.dumps(sys.argv[1:]))
    print(json.dumps({'state': 'starts_on_launch'}))
elif args[1] == '-':
    sys.stdin.read()
    (root / 'verify-args').write_text(json.dumps(args))
    (root / 'verified').touch()
    if mode == 'verify-failure':
        sys.exit(1)
    reviewed = 'bird-reviewed' if 'oldman@oldman-bird.local' in sys.argv else 'reviewed'
    print(json.dumps({'host': {'digest': 'changed' if mode == 'mismatch' else reviewed},
                      'verification': {'reused_files': 0 if '--full-verify' in args else 15,
                                       'hashed_files': 17 if '--full-verify' in args else 2,
                                       'hashed_bytes': 100}}))
else:
    (root / 'started').touch()
    (root / 'owner-args').write_text(json.dumps(sys.argv[1:]))
    if mode == 'busy':
        print('Another session owns Framework', file=sys.stderr)
        sys.exit(1)
    profile = args[-1]
    bird = profile == 'bird-gyro-m-fast'
    state = '.local/state/codex-gyro' if bird else '.local/state/codex-framework-gyro'
    verb = 'reusing' if mode == 'warm' else 'starting'
    print('Gyro-M: ' + verb + ' ' + profile + '; logs: ' + str(root / state / '20261113T123456Z-123'), file=sys.stderr, flush=True)
    print('GYRO_READY ' + json.dumps({'model':'fixture', 'context':131072 if bird else 163840, 'drafting':'dflash2'}), flush=True)
    while True:
        if mode == 'owner-loss' and (root / 'client').exists():
            sys.exit(1)
        if select.select([sys.stdin], [], [], .05)[0]:
            line = sys.stdin.readline()
            if not line:
                break
            with (root / 'owner-input').open('a') as received:
                received.write(line)
    (root / 'restored').touch()
    if mode == 'restore-failure':
        sys.exit(1)
''')
        self.write_program(self.package / "playtime-chat", r'''
import json, os, pathlib, signal, sys, time
root = pathlib.Path(os.environ['FIXTURE_ROOT'])
if '--inspect-interrupted' in sys.argv:
    assert not (root / 'started').exists(), 'inspection must not acquire inference'
    (root / 'inspected').touch()
    sys.exit(0)
if '--select-session' in sys.argv:
    assert not (root / 'started').exists(), 'picker must precede SSH ownership'
    (root / 'picked').write_text(json.dumps(sys.argv[1:]))
    mode = os.environ.get('FIXTURE_MODE')
    if mode == 'picker-failure':
        sys.exit(2)
    print(json.dumps({'session': str(root / 'saved-session') if mode == 'picker-resume' else None,
                      'cancelled': mode == 'picker-cancel',
                      'recover_interrupted': str(root / 'stopped-session') if mode == 'picker-recover' else None,
                      'acknowledge_uncertain_tools': mode == 'picker-recover'}))
    sys.exit(0)
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

    def test_picker_resumes_before_host_start_and_preserves_context(self):
        result = self.run_fixture("picker-resume", "--sessions", "--full-verify", "--workspace", "/chosen/project", "--context-tokens", "65536")
        self.assertEqual(result.returncode, 0, result.stderr)
        picked = json.loads((self.root / 'picked').read_text())
        self.assertIn('/chosen/project', picked)
        self.assertIn('65536', picked)
        client = json.loads((self.root / 'client').read_text())['argv']
        self.assertEqual(client[-2:], ['--session', str(self.root / 'saved-session')])
        self.assertTrue((self.root / 'restored').exists())

    def test_recovery_picker_hands_off_the_explicit_fork_and_acknowledgement(self):
        result = self.run_fixture('picker-recover', '--sessions')
        self.assertEqual(result.returncode, 0, result.stderr)
        argv = json.loads((self.root / 'client').read_text())['argv']
        self.assertEqual(argv[-3:], ['--recover-interrupted', str(self.root / 'stopped-session'), '--acknowledge-uncertain-tools'])
        self.assertNotIn('--session', argv)

    def test_interruption_inspection_never_starts_inference(self):
        result = self.run_fixture('', '--inspect-interrupted', '/stopped-session')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue((self.root / 'inspected').exists())
        self.assertFalse((self.root / 'started').exists())

    def test_picker_cancel_and_failure_never_start_host(self):
        for mode, expected in [('picker-cancel', 0), ('picker-failure', 1)]:
            result = self.run_fixture(mode, '--sessions')
            self.assertEqual(result.returncode, expected, result.stderr)
            self.assertTrue((self.root / 'picked').exists())
            self.assertFalse((self.root / 'started').exists())
            self.assertFalse((self.root / 'client').exists())

    def test_explicit_sessions_and_headless_calls_skip_picker(self):
        for args in [('--session', '/saved'), ('--prompt', '--sessions'), ('--recover-interrupted', '/stopped')]:
            result = self.run_fixture('', *args)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertFalse((self.root / 'picked').exists())

    def test_picker_new_conversation_has_no_session_override(self):
        result = self.run_fixture('', '--sessions')
        self.assertEqual(result.returncode, 0, result.stderr)
        client = json.loads((self.root / 'client').read_text())['argv']
        self.assertNotIn('--session', client)

    def test_warm_handoff_is_verified_and_explicitly_released(self):
        result = self.run_fixture('warm', '--prompt', 'hello')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue((self.root / 'verified').exists())
        self.assertIn('GYRO_RELEASE\n', (self.root / 'owner-input').read_text())

    def test_failed_verification_never_parks_the_model(self):
        result = self.run_fixture('mismatch', '--prompt', 'hello')
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn('GYRO_RELEASE', (self.root / 'owner-input').read_text())

    def test_manual_stop_does_not_start_an_owner_or_client(self):
        result = self.run_fixture('', '--stop-server')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue((self.root / 'stop-warm').exists())
        self.assertFalse((self.root / 'started').exists())
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
        verify_args = json.loads((self.root / 'verify-args').read_text())
        self.assertEqual(json.loads(verify_args[verify_args.index('--expected-host') + 1]), self.lock['host'])
        timings = json.loads(next((self.root / '.local/state/playtime-launcher').glob('*/timings.json')).read_text())
        self.assertGreaterEqual(timings['launcher_ready_seconds'], timings['model_ready_seconds'] + timings['verification_seconds'])
        self.assertEqual(timings['verification']['reused_files'], 15)

    def test_bird_handoff_uses_its_host_key_helper_context_and_lock(self):
        result = subprocess.run(self.command('bird-gyro-m-fast', '--prompt', 'hello'),
                                env=self.env, cwd=self.root, capture_output=True, text=True, timeout=15)
        self.assertEqual(result.returncode, 0, result.stderr)
        owner = json.loads((self.root / 'owner-args').read_text())
        self.assertIn('oldman@oldman-bird.local', owner)
        self.assertEqual(owner[owner.index('-i') + 1], str(self.root / '.ssh/id_ed25519_oldman_bird_codex'))
        self.assertIn('.local/lib/codex-gyro/warm-session.py bird-gyro-m-fast', owner[-1])
        verify_args = json.loads((self.root / 'verify-args').read_text())
        self.assertEqual(json.loads(verify_args[verify_args.index('--expected-host') + 1]), self.bird_lock['host'])
        child = json.loads((self.root / 'client').read_text())
        self.assertEqual(child['argv'][child['argv'].index('--model-lock') + 1],
                         str(self.package / 'bird-strata-gyro-m-fast-v1.lock.json'))
        self.assertTrue((self.root / 'restored').exists())

    def test_bird_status_uses_its_helper_without_starting_a_server(self):
        result = subprocess.run(self.command('--status', 'bird-gyro-m-fast'), env=self.env,
                                capture_output=True, text=True, timeout=5)
        self.assertEqual(result.returncode, 0, result.stderr)
        arguments = json.loads((self.root / 'status-args').read_text())
        self.assertIn('oldman@oldman-bird.local', arguments)
        self.assertIn('.local/lib/codex-gyro/warm-session.py --status bird-gyro-m-fast', arguments[-1])
        self.assertFalse((self.root / 'started').exists())
        self.assertFalse((self.root / 'verified').exists())

    def test_full_verification_is_forwarded_only_to_verifier(self):
        result = self.run_fixture('', '--full-verify', '--prompt', 'hello')
        self.assertEqual(result.returncode, 0, result.stderr)
        verify_args = json.loads((self.root / 'verify-args').read_text())
        self.assertIn('--full-verify', verify_args)
        child = json.loads((self.root / 'client').read_text())
        self.assertNotIn('--full-verify', child['argv'])
        self.assertEqual(child['argv'][-2:], ['--prompt', 'hello'])
        self.assertIn('0 cached, 17 hashed', result.stderr)

    def test_full_verify_literal_prompt_is_not_a_wrapper_option(self):
        result = self.run_fixture('', '--prompt', '--full-verify')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn('--full-verify', json.loads((self.root / 'verify-args').read_text()))
        child = json.loads((self.root / 'client').read_text())
        self.assertEqual(child['argv'][-2:], ['--prompt', '--full-verify'])

    def test_help_explains_full_verification_without_startup(self):
        result = self.run_fixture('', '--help')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('--full-verify', result.stdout)
        self.assertFalse((self.root / 'started').exists())

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
