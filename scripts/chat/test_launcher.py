"""CPU lifecycle fixtures for the standalone owner wrapper, without SSH or GPUs."""

import hashlib
import importlib.util
import json
import marshal
import os
from pathlib import Path
import pty
import select
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
        for name in ("run-strata.py", "verify-strata-host.py", "project_environment.py",
                     "launch_protocol.py", "presets.json"):
            shutil.copyfile(SOURCE / name, self.package / name)
        self.lock = {"identity": {"model": "fixture", "context_tokens": 163840}, "host": {"digest": "reviewed"}}
        (self.package / "framework-strata-gyro-m-fast-v2.lock.json").write_text(json.dumps(self.lock))
        self.bird_lock = {"identity": {"model": "fixture", "context_tokens": 131072}, "host": {"digest": "bird-reviewed"}}
        (self.package / "bird-strata-gyro-m-fast-v2.lock.json").write_text(json.dumps(self.bird_lock))
        (self.package / "general.lock.json").write_text("{}")
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.write_program(self.bin / "ssh", r'''
import json, os, pathlib, select, shlex, signal, sys, time
root = pathlib.Path(os.environ['FIXTURE_ROOT'])
args = shlex.split(sys.argv[-1])
mode = os.environ.get('FIXTURE_MODE', '')
if '--stop' in args:
    (root / 'stop-warm').touch()
elif '--status' in args:
    (root / 'status').touch()
    (root / 'status-args').write_text(json.dumps(sys.argv[1:]))
    if mode == 'status-down':
        sys.exit(255)
    if mode == 'slow-status':
        time.sleep(1)
    print(json.dumps({'state': 'busy', 'message': 'host is in use: chat'} if mode == 'status-busy'
                     else {'state': 'starts_on_launch'}))
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
    with (root / 'owner-starts').open('a') as starts:
        starts.write('start\n')
    (root / 'owner-args').write_text(json.dumps(sys.argv[1:]))
    if mode == 'busy':
        print('Another session owns Framework', file=sys.stderr)
        sys.exit(1)
    if mode == 'slow-ready':
        time.sleep(1)
    profile = args[-1]
    bird = profile == 'bird-gyro-m-fast'
    state = '.local/state/codex-gyro' if bird else '.local/state/codex-framework-gyro'
    verb = 'reusing' if mode == 'warm' else 'starting'
    print('Gyro-M: ' + verb + ' ' + profile + '; logs: ' + str(root / state / '20261113T123456Z-123'), file=sys.stderr, flush=True)
    print('GYRO_READY ' + json.dumps({'model':'fixture', 'context':131072 if bird else 163840, 'drafting':'dflash2'}), flush=True)
    while True:
        if mode in ('owner-loss', 'owner-exit-zero') and (root / 'client').exists():
            sys.exit(1 if mode == 'owner-loss' else 0)
        if select.select([sys.stdin], [], [], .05)[0]:
            line = sys.stdin.readline()
            if not line:
                break
            with (root / 'owner-input').open('a') as received:
                received.write(line)
    if mode == 'owner-cleanup-timeout':
        signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
        while True:
            time.sleep(.02)
    (root / 'restored').touch()
    if mode in ('restore-failure', 'restart-restore-failure'):
        sys.exit(1)
''')
        self.write_program(self.package / "playtime-chat", r'''
import json, os, pathlib, signal, sys, time
root = pathlib.Path(os.environ['FIXTURE_ROOT'])
if '--list-sessions' in sys.argv:
    assert not (root / 'started').exists(), 'discovery must precede SSH ownership'
    (root / 'listed').write_text(json.dumps(sys.argv[1:]))
    mode = os.environ.get('FIXTURE_MODE', '')
    if mode == 'list-failure':
        print('playtime-chat: fixture workspace rejected', file=sys.stderr)
        sys.exit(2)
    state = mode.removeprefix('sessions-') if mode.startswith('sessions-') else 'ready'
    print(json.dumps([] if state == 'empty' else [{'state': state}]))
    sys.exit(0)
if '--inspect-interrupted' in sys.argv:
    assert not (root / 'started').exists(), 'inspection must not acquire inference'
    (root / 'inspected').touch()
    sys.exit(0)
if '--select-session' in sys.argv:
    assert not (root / 'started').exists(), 'picker must precede SSH ownership'
    (root / 'picked').write_text(json.dumps(sys.argv[1:]))
    mode = os.environ.get('FIXTURE_MODE')
    if mode == 'picker-failure':
        print('playtime-chat: fixture workspace rejected', file=sys.stderr)
        sys.exit(2)
    print(json.dumps({'session': str(root / 'saved-session') if mode == 'picker-resume' else None,
                      'cancelled': mode == 'picker-cancel',
                      'recover_interrupted': str(root / 'stopped-session') if mode == 'picker-recover' else None,
                      'acknowledge_uncertain_tools': mode == 'picker-recover'}))
    sys.exit(0)
(root / 'client').write_text(json.dumps({'argv': sys.argv[1:], 'cwd': os.getcwd()}))
if os.environ.get('FIXTURE_MODE', '').startswith('restart'):
    if not (root / 'restart-requested').exists():
        (root / 'restart-requested').touch()
        destination = root / 'fork'
        destination.mkdir()
        (destination / 'config.json').write_text('{}')
        pathlib.Path(os.environ['PLAYTIME_CHAT_RESTART_FILE']).write_text(json.dumps({'session': str(destination)}))
        sys.exit(75)
    assert (root / 'restored').exists(), 'owner must finish before client restart'
    assert 'GYRO_RELEASE' not in (root / 'owner-input').read_text(), 'interrupted inference must not be parked'
    assert '--session' in sys.argv and str(root / 'fork') in sys.argv
    (root / 'restarted').touch()
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

    def run_interactive(self, mode="", *args):
        master, terminal = pty.openpty()
        try:
            return subprocess.run(self.command("framework-gyro-m-fast", *args),
                                  env=dict(self.env, FIXTURE_MODE=mode), cwd=self.root,
                                  stdin=terminal, capture_output=True, text=True, timeout=15)
        finally:
            os.close(terminal)
            os.close(master)

    def start_hold(self, mode="", *args):
        # Unbuffered, so select() sees every event line the hold writes.
        process = subprocess.Popen(self.command("framework-gyro-m-fast", "--hold", *args),
                                   env=dict(self.env, FIXTURE_MODE=mode), cwd=self.root, bufsize=0,
                                   stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        self.addCleanup(self.stop_hold, process)
        return process

    def stop_hold(self, process):
        # Close the pipes directly: a test may already have closed stdin, which
        # communicate() refuses before Python 3.13.
        if process.poll() is None:
            process.kill()
        process.wait(timeout=10)
        for stream in (process.stdin, process.stdout, process.stderr):
            stream.close()

    def hold_event(self, process, timeout=10):
        self.assertTrue(select.select([process.stdout], [], [], timeout)[0], "no hold event in time")
        event = json.loads(process.stdout.readline())
        self.assertEqual((event["schema_version"], event["protocol"], event["profile"]),
                         (1, "hold_v1", "framework-gyro-m-fast"))
        return event

    def owner_input(self):
        path = self.root / "owner-input"
        return path.read_text() if path.exists() else ""

    def test_hold_lends_the_verified_endpoint_until_the_caller_releases_it(self):
        process = self.start_hold()
        held = self.hold_event(process)
        self.assertEqual(held["event"], "held")
        self.assertRegex(held["endpoint"], r"^http://127\.0\.0\.1:\d+/v1$")
        port = held["endpoint"].split(":")[2].split("/")[0]
        self.assertIn(f"127.0.0.1:{port}:127.0.0.1:8080", json.loads((self.root / "owner-args").read_text()))
        self.assertEqual(held["report"]["host"], self.lock["host"])
        self.assertEqual(held["model_lock"], "framework-strata-gyro-m-fast-v2.lock.json")
        self.assertEqual(held["model_lock_sha256"],
                         hashlib.sha256((self.package / held["model_lock"]).read_bytes()).hexdigest())
        self.assertEqual(held["ready"]["context"], 163840)
        self.assertTrue(Path(held["evidence"]).is_dir())
        # The caller runs its own client; the host stays owned until it answers.
        time.sleep(.3)
        self.assertFalse((self.root / "client").exists())
        self.assertFalse((self.root / "restored").exists())
        self.assertEqual(process.poll(), None)
        process.stdin.write(b"release\n")
        process.stdin.close()
        ended = self.hold_event(process)
        self.assertEqual({key: ended[key] for key in ("event", "ending", "released", "inference_owner", "owner_exit")},
                         {"event": "ended", "ending": "released", "released": True, "inference_owner": "confirmed",
                          "owner_exit": 0})
        self.assertNotIn("message", ended)
        self.assertEqual(process.wait(timeout=10), 0)
        self.assertIn("GYRO_RELEASE", self.owner_input())
        self.assertTrue((self.root / "restored").exists())

    def test_hold_closed_or_misused_by_its_caller_stops_the_server(self):
        for mode, caller_input, ending, owner, code in (
                ("", b"", "closed", "confirmed", 0), ("", b"stop\n", "protocol_error", "confirmed", 1),
                ("", b"release now\n", "protocol_error", "confirmed", 1),
                ("restore-failure", b"", "closed", "unresolved", 1)):
            with self.subTest(mode=mode, ending=ending, caller_input=caller_input):
                for name in ("owner-input", "restored", "verify-args"):
                    (self.root / name).unlink(missing_ok=True)
                process = self.start_hold(mode, "--full-verify")
                self.assertEqual(self.hold_event(process)["event"], "held")
                self.assertIn("--full-verify", json.loads((self.root / "verify-args").read_text()))
                process.stdin.write(caller_input)
                process.stdin.close()
                ended = self.hold_event(process)
                self.assertEqual((ended["ending"], ended["released"], ended["inference_owner"]), (ending, False, owner))
                self.assertEqual(process.wait(timeout=10), code)
                self.assertNotIn("GYRO_RELEASE", self.owner_input())
                self.assertTrue((self.root / "restored").exists())

    def test_hold_refuses_a_busy_or_unreachable_host_without_owning_it(self):
        for mode, reason, message in (("status-busy", "busy", "host is in use: chat"),
                                      ("status-down", "host_unavailable", "remote helper status failed")):
            with self.subTest(mode=mode):
                process = self.start_hold(mode)
                refused = self.hold_event(process)
                self.assertEqual((refused["event"], refused["reason"], refused["message"], refused["inference_owner"]),
                                 ("refused", reason, message, "not_acquired"))
                self.assertEqual(process.wait(timeout=10), 1)
                self.assertTrue((self.root / "status").exists())
                self.assertFalse((self.root / "started").exists())

    def test_hold_refuses_a_host_it_cannot_verify_and_stops_it(self):
        # A helper that exits early can't prove it left nothing running.
        for mode, reason, owner in (("mismatch", "mismatch", "confirmed"),
                                    ("verify-failure", "verification_failed", "confirmed"),
                                    ("busy", "host_unavailable", "unresolved")):
            with self.subTest(mode=mode):
                for name in ("owner-input", "restored"):
                    (self.root / name).unlink(missing_ok=True)
                process = self.start_hold(mode)
                refused = self.hold_event(process)
                self.assertEqual((refused["event"], refused["reason"], refused["inference_owner"]),
                                 ("refused", reason, owner))
                self.assertNotIn("endpoint", refused)
                self.assertEqual(process.wait(timeout=10), 1)
                self.assertNotIn("GYRO_RELEASE", self.owner_input())
                self.assertFalse((self.root / "client").exists())
                self.assertEqual((self.root / "restored").exists(), mode != "busy")

    def test_hold_reports_a_failure_before_the_helper_starts(self):
        # A UnicodeDecodeError has a `reason` attribute that is not a hold reason.
        (self.package / "framework-strata-gyro-m-fast-v2.lock.json").write_bytes(b"\xff")
        process = self.start_hold()
        refused = self.hold_event(process)
        self.assertEqual((refused["event"], refused["reason"], refused["inference_owner"]),
                         ("refused", "failed", "not_acquired"))
        self.assertIn("invalid start byte", refused["message"])
        self.assertEqual(process.wait(timeout=10), 1)
        self.assertFalse((self.root / "started").exists())

    def wait_for(self, name):
        deadline = time.monotonic() + 10
        while not (self.root / name).exists() and time.monotonic() < deadline:
            time.sleep(.02)

    def test_hold_abandoned_or_signalled_before_it_is_held_stops_the_server(self):
        for ending, reason, code in (("close", "abandoned", 1), ("signal", "signal", 128 + signal.SIGTERM)):
            with self.subTest(ending=ending):
                for name in ("owner-input", "restored", "started"):
                    (self.root / name).unlink(missing_ok=True)
                process = self.start_hold("slow-ready")
                if ending == "close":
                    process.stdin.close()
                else:
                    # Signal once the owner exists, before the slow helper is ready.
                    self.wait_for("started")
                    process.send_signal(signal.SIGTERM)
                refused = self.hold_event(process)
                self.assertEqual((refused["event"], refused["reason"], refused["inference_owner"]),
                                 ("refused", reason, "confirmed"))
                self.assertEqual(process.wait(timeout=10), code)
                self.assertNotIn("GYRO_RELEASE", self.owner_input())
                self.assertTrue((self.root / "restored").exists())

    def test_hold_signalled_during_its_status_check_never_acquires(self):
        process = self.start_hold("slow-status")
        self.wait_for("status")
        process.send_signal(signal.SIGTERM)
        refused = self.hold_event(process)
        self.assertEqual((refused["event"], refused["reason"], refused["message"], refused["inference_owner"]),
                         ("refused", "signal", "received signal 15", "not_acquired"))
        self.assertEqual(process.wait(timeout=10), 128 + signal.SIGTERM)
        self.assertFalse((self.root / "started").exists())

    def test_hold_reports_a_lost_owner_or_a_signal_as_its_end(self):
        # An owner that exits before the wrapper's EOF is lost, whatever its code.
        for mode, owner_exit in (("owner-loss", 1), ("owner-exit-zero", 0)):
            with self.subTest(mode=mode):
                (self.root / "client").unlink(missing_ok=True)
                process = self.start_hold(mode)
                self.assertEqual(self.hold_event(process)["event"], "held")
                (self.root / "client").touch()
                ended = self.hold_event(process)
                self.assertEqual((ended["ending"], ended["released"], ended["inference_owner"], ended["owner_exit"],
                                  ended["message"]),
                                 ("lost", False, "unresolved", owner_exit, "lost the inference host owner"))
                self.assertEqual(process.wait(timeout=10), 1)
        (self.root / "client").unlink()
        (self.root / "owner-input").unlink(missing_ok=True)
        process = self.start_hold()
        self.assertEqual(self.hold_event(process)["event"], "held")
        process.send_signal(signal.SIGTERM)
        ended = self.hold_event(process)
        self.assertEqual((ended["ending"], ended["released"], ended["inference_owner"], ended["message"]),
                         ("signal", False, "confirmed", "received signal 15"))
        self.assertEqual(process.wait(timeout=10), 128 + signal.SIGTERM)
        self.assertNotIn("GYRO_RELEASE", self.owner_input())

    def test_hold_is_described_without_contacting_the_host(self):
        result = self.run_fixture("", "--describe-hold")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout),
                         {"schema_version": 1, "protocol": "hold_v1", "profile": "framework-gyro-m-fast",
                          "model_lock": "framework-strata-gyro-m-fast-v2.lock.json", "startup_timeout": 930,
                          "verify_timeout": 600, "cleanup_timeout": 160})
        for args in (("--describe-hold", "extra"), ("--hold", "--prompt", "hi")):
            with self.subTest(args=args):
                result = self.run_fixture("", *args)
                self.assertEqual(result.returncode, 1)
                self.assertEqual(result.stdout, "")
        self.assertFalse((self.root / "status").exists())
        self.assertFalse((self.root / "started").exists())

    def test_interactive_launch_starts_new_without_resumable_sessions(self):
        for state in ('empty', 'interrupted', 'in_use', 'settings_differ', 'unavailable'):
            with self.subTest(state=state):
                # Each discovery must precede this launch's owner acquisition.
                (self.root / 'started').unlink(missing_ok=True)
                result = self.run_interactive('sessions-' + state)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertTrue((self.root / 'listed').exists())
                self.assertFalse((self.root / 'picked').exists())
                argv = json.loads((self.root / 'client').read_text())['argv']
                self.assertNotIn('--session', argv)
                self.assertNotIn('--recover-interrupted', argv)
                if state != 'empty':
                    self.assertIn('--sessions to inspect or recover', result.stderr)

    def test_interactive_launch_offers_resumable_session(self):
        result = self.run_interactive('picker-resume')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue((self.root / 'listed').exists())
        self.assertTrue((self.root / 'picked').exists())
        argv = json.loads((self.root / 'client').read_text())['argv']
        self.assertEqual(argv[argv.index('--session') + 1], str(self.root / 'saved-session'))

    def test_interactive_discovery_failure_never_starts_host(self):
        result = self.run_interactive('list-failure')
        self.assertEqual(result.returncode, 2)
        self.assertEqual(result.stderr.strip(), 'playtime-chat: fixture workspace rejected')
        self.assertFalse((self.root / 'started').exists())

    def test_forced_picker_does_not_require_a_resumable_session(self):
        result = self.run_interactive('picker-recover', '--sessions')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse((self.root / 'listed').exists())
        argv = json.loads((self.root / 'client').read_text())['argv']
        self.assertEqual(argv[argv.index('--recover-interrupted') + 1], str(self.root / 'stopped-session'))

    def test_host_preset_skips_environment_and_preserves_picker_options(self):
        (self.package / 'general.lock.json').unlink()
        definitions = self.root / '.devcontainer'
        definitions.mkdir()
        (definitions / 'devcontainer.json').write_text('invalid and never read for host tools')
        result = self.run_interactive('picker-resume', '--preset', 'coding-host', '--sessions')
        self.assertEqual(result.returncode, 0, result.stderr)
        argv = json.loads((self.root / 'client').read_text())['argv']
        self.assertNotIn('--image-lock', argv)
        self.assertEqual(argv[argv.index('--preset') + 1], 'coding-host')
        self.assertIn('--session', argv)
        self.assertTrue((self.root / 'restored').exists())

    def test_host_preset_rejects_container_selection_before_ownership(self):
        result = self.run_fixture('', '--preset', 'coding-host', '--environment', 'general')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('cannot select a Docker environment', result.stderr)
        self.assertFalse((self.root / 'started').exists())

    def test_host_preset_rejects_unknown_chat_options_before_ownership(self):
        result = self.run_fixture('', '--preset', 'coding-host', '--bogus')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('unsupported chat option: --bogus', result.stderr)
        self.assertFalse((self.root / 'started').exists())

    def test_host_preset_help_does_not_acquire_inference(self):
        (self.package / 'general.lock.json').unlink()
        result = self.run_fixture('', '--preset', 'coding-host', '--help')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse((self.root / 'started').exists())
        argv = json.loads((self.root / 'client').read_text())['argv']
        self.assertIn('--help', argv)
        self.assertIn('coding-host', argv)

    def worker_fixture(self):
        self.write_program(self.package / "playtime-task", r'''
import json, os, pathlib, sys
root=pathlib.Path(os.environ['FIXTURE_ROOT'])
if '--describe-protocol' in sys.argv:
    if '--role' in sys.argv and os.environ.get('FIXTURE_MODE') != 'missing-readonly':
        readonly = {'protocol':'playtime-readonly','version':1,'capability':'playtime-readonly-v1','roles':['setup','supervisor','reviewer'],'report':'structured-result-v1','output_contract':'json-schema-subset-v1','continuation':'none','transport':'strata-responses-v1','tool_backends':{'native':'project-native-v1'}}
        # A package built before read-only repair turns offers neither field.
        if os.environ.get('FIXTURE_MODE') != 'old-readonly':
            readonly.update({'repair_turns':1,'caller_deadline':'unix-ms-v1'})
        print(json.dumps(readonly))
        sys.exit(0)
    contract = {} if os.environ.get('FIXTURE_MODE') == 'missing-contract' else {'capability':'playtime-worker-contract-v1','output_contract':'json-schema-subset-v1','report':'structured-result-v1','evidence':'evidence-ref-v1','continuation':'completed-session-only','transport':'strata-responses-v1','repair_turns':1,'caller_deadline':'unix-ms-v1'}
    if os.environ.get('FIXTURE_MODE') == 'missing-deadline':
        del contract['caller_deadline']
    print(json.dumps({'protocol':'playtime-worker','version':2 if os.environ.get('FIXTURE_MODE')=='wrong-protocol' else 1, 'tool_backends': {} if os.environ.get('FIXTURE_MODE')=='missing-native' else {'native':'project-native-v1'}, 'contract_invocation': contract}))
elif '--check-inputs' in sys.argv:
    assert not (root/'started').exists()
    if os.environ.get('FIXTURE_MODE') == 'invalid-readonly-input':
        sys.exit(1)
    print(json.dumps({'inputs_valid':True}))
else:
    assert sys.stdin.read() == ''
    assert 'SSH_AUTH_SOCK' not in os.environ
    (root/'client').write_text(json.dumps(sys.argv[1:]))
''')
        project=self.root/'project';project.mkdir()
        records=self.root/'records';records.mkdir(mode=0o700)
        request=records/'prompt.txt';request.write_text('Assignment');request.chmod(0o600)
        return ['--worker','--environment','general','--workspace',str(project),'--run-dir',str(records),'--request',str(request)]

    def test_worker_gate_and_protocol_precede_host_ownership(self):
        args=self.worker_fixture()
        self.env['SSH_AUTH_SOCK']='/trusted/fixture-agent'
        for text,mode in [('Changed',''),('Assignment','wrong-protocol')]:
            result=subprocess.run(self.command('framework-gyro-m-fast',*args), input=text, env=dict(self.env,FIXTURE_MODE=mode),capture_output=True,text=True,timeout=10)
            self.assertNotEqual(result.returncode,0)
            self.assertFalse((self.root/'started').exists())
        result=subprocess.run(self.command('framework-gyro-m-fast',*args),input='Assignment',env=self.env,capture_output=True,text=True,timeout=10)
        self.assertEqual(result.returncode,0,result.stderr)
        self.assertTrue((self.root/'restored').exists())
        self.assertFalse((self.root/'picked').exists())

    def test_native_worker_needs_no_docker_image_or_environment(self):
        args = self.worker_fixture()
        args[1:3] = ['--tool-backend', 'native', '--native-policy', '/etc/playtime/native/fixture.json']
        (self.package / 'general.lock.json').unlink()
        result = subprocess.run(self.command('framework-gyro-m-fast', *args), input='Assignment', env=self.env, capture_output=True, text=True, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)
        child = json.loads((self.root / 'client').read_text())
        self.assertNotIn('--image-lock', child)
        self.assertIn('/etc/playtime/native/fixture.json', child)
        self.assertTrue((self.root / 'restored').exists())

    def test_native_capability_and_mixed_environment_fail_before_owner(self):
        args = self.worker_fixture()
        args += ['--tool-backend', 'native', '--native-policy', '/etc/playtime/native/fixture.json']
        result = subprocess.run(self.command('framework-gyro-m-fast', *args), input='Assignment', env=self.env, capture_output=True, text=True, timeout=10)
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse((self.root / 'started').exists())
        del args[1:3]
        result = subprocess.run(self.command('framework-gyro-m-fast', *args), input='Assignment', env=dict(self.env, FIXTURE_MODE='missing-native'), capture_output=True, text=True, timeout=10)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('lacks project-native-v1', result.stderr)
        self.assertFalse((self.root / 'started').exists())

    def test_worker_waits_for_durable_start_release(self):
        args=self.worker_fixture()
        with subprocess.Popen(self.command('framework-gyro-m-fast',*args),stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,env=self.env) as child:
            time.sleep(.3)
            self.assertIsNone(child.poll())
            self.assertFalse((self.root/'started').exists())
            _,err=child.communicate(b'Assignment',timeout=10)
            self.assertEqual(child.returncode,0,err)

    def contract_fixture(self):
        import hashlib
        args = self.worker_fixture()
        schema = self.root / 'records' / 'schema.json'
        schema.write_text('{"type":"object","properties":{},"additionalProperties":false}')
        schema.chmod(0o600)
        return args + ['--output-contract', str(schema), '--output-contract-sha256', hashlib.sha256(schema.read_bytes()).hexdigest()]

    def test_worker_contract_pins_request_and_keeps_version_one_arguments_unchanged(self):
        import hashlib
        args = self.contract_fixture()
        result = subprocess.run(self.command('framework-gyro-m-fast', *args, '--evidence-property', 'verification', '--repair-turns', '1',
                                             '--caller-deadline-unix-ms', '1800000000000'),
                                input='Assignment', env=self.env, capture_output=True, text=True, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)
        child = json.loads((self.root / 'client').read_text())
        self.assertEqual(child[child.index('--request-sha256') + 1], hashlib.sha256(b'Assignment').hexdigest())
        self.assertEqual(child[child.index('--evidence-property') + 1], 'verification')
        self.assertEqual(child[child.index('--caller-deadline-unix-ms') + 1], '1800000000000')
        self.assertIn('--image-lock', child)
        self.assertTrue((self.root / 'restored').exists())
        # A version-one worker is launched exactly as before, with no pinned request.
        for marker in ('client', 'started'):
            (self.root / marker).unlink()
        result = subprocess.run(self.command('framework-gyro-m-fast', *args[:args.index('--output-contract')]),
                                input='Assignment', env=self.env, capture_output=True, text=True, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn('--request-sha256', json.loads((self.root / 'client').read_text()))

    def test_worker_contract_rejections_precede_inference_acquisition(self):
        args = self.contract_fixture()
        plain = args[:args.index('--output-contract')]
        inside = self.root / 'project' / 'schema.json'
        inside.write_text('{}')
        moved = args.copy()
        moved[moved.index('--output-contract') + 1] = str(inside)
        cases = [(args, 'Changed', ''), (args, 'Assignment', 'missing-contract'),
                 ([*args, '--repair-turns', '2'], 'Assignment', ''),
                 ([*args, '--worker-session', 'ptw-' + 'a' * 64], 'Assignment', ''),
                 ([*args, '--role', 'reviewer'], 'Assignment', ''),
                 ([*args[:-1], 'invalid'], 'Assignment', ''),
                 (moved, 'Assignment', ''),
                 ([*plain, '--evidence-property', 'verification'], 'Assignment', ''),
                 ([*plain, '--repair-turns', '1'], 'Assignment', ''),
                 ([*plain, '--caller-deadline-unix-ms', '1800000000000'], 'Assignment', ''),
                 ([*args, '--caller-deadline-unix-ms', '1800000000000'], 'Assignment', 'missing-deadline'),
                 ([*plain, '--output-contract-sha256', 'a' * 64], 'Assignment', ''),
                 ([*plain, '--worker-session', 'ptc-' + 'a' * 64], 'Assignment', '')]
        for argv, delivered, mode in cases:
            result = subprocess.run(self.command('framework-gyro-m-fast', *argv), input=delivered,
                                    env=dict(self.env, FIXTURE_MODE=mode), capture_output=True, text=True, timeout=10)
            self.assertNotEqual(result.returncode, 0, (argv, mode))
            self.assertFalse((self.root / 'started').exists(), (argv, mode))

    def readonly_fixture(self):
        import hashlib
        args = self.worker_fixture()
        schema = self.root / 'records' / 'schema.json'
        schema.write_text('{"type":"object","properties":{},"additionalProperties":false}')
        schema.chmod(0o600)
        args[:3] = ['--read-only']
        args += ['--role', 'reviewer', '--tool-backend', 'native',
                 '--native-policy', '/etc/playtime/native/readonly.json',
                 '--output-contract', str(schema), '--output-contract-sha256', hashlib.sha256(schema.read_bytes()).hexdigest()]
        (self.package / 'general.lock.json').unlink()
        return args

    def test_readonly_gate_pins_request_and_keeps_owner_credential_out_of_child(self):
        import hashlib
        args = self.readonly_fixture()
        self.env['SSH_AUTH_SOCK'] = '/trusted/fixture-agent'
        offered = [*args, '--repair-turns', '1', '--caller-deadline-unix-ms', '1800000000000']
        with subprocess.Popen(self.command('framework-gyro-m-fast', *offered), stdin=subprocess.PIPE,
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=self.env) as child:
            time.sleep(.3)
            self.assertIsNone(child.poll())
            self.assertFalse((self.root / 'started').exists())
            _, errors = child.communicate(b'Assignment', timeout=10)
            self.assertEqual(child.returncode, 0, errors)
        child_args = json.loads((self.root / 'client').read_text())
        self.assertEqual(child_args[child_args.index('--request-sha256') + 1], hashlib.sha256(b'Assignment').hexdigest())
        self.assertIn('--role', child_args)
        self.assertEqual(child_args[child_args.index('--repair-turns') + 1], '1')
        self.assertEqual(child_args[child_args.index('--caller-deadline-unix-ms') + 1], '1800000000000')
        self.assertNotIn('--image-lock', child_args)
        self.assertTrue((self.root / 'restored').exists())
        # An older package still runs read-only roles that ask for no repair turn.
        for marker in ('client', 'started'):
            (self.root / marker).unlink()
        result = subprocess.run(self.command('framework-gyro-m-fast', *args), input='Assignment',
                                env=dict(self.env, FIXTURE_MODE='old-readonly'), capture_output=True, text=True, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn('--repair-turns', json.loads((self.root / 'client').read_text()))

    def test_readonly_rejections_precede_inference_acquisition(self):
        args = self.readonly_fixture()
        cases = [(['--read-only', '--prepare-environment', '--workspace', str(self.root / 'project')], 'Assignment', ''),
                 (['--read-only', '--stop-server'], 'Assignment', ''),
                 (args, 'Changed', ''), (args, 'Assignment', 'missing-readonly'),
                 (args, 'Assignment', 'invalid-readonly-input'),
                 ([*args, '--worker-session', 'ptw-' + 'a' * 64], 'Assignment', ''),
                 ([*args, '--role', 'setup'], 'Assignment', ''),
                 ([*args, '--repair-turns', '2'], 'Assignment', ''),
                 ([*args, '--evidence-property', 'verification'], 'Assignment', ''),
                 ([*args, '--repair-turns', '1'], 'Assignment', 'old-readonly'),
                 ([*args, '--caller-deadline-unix-ms', '1800000000000'], 'Assignment', 'old-readonly'),
                 ([args[0], '--environment', 'general', *args[1:]], 'Assignment', '')]
        for argv, delivered, mode in cases:
            result = subprocess.run(self.command('framework-gyro-m-fast', *argv), input=delivered,
                                    env=dict(self.env, FIXTURE_MODE=mode), capture_output=True, text=True, timeout=10)
            self.assertNotEqual(result.returncode, 0, (argv, mode))
            self.assertFalse((self.root / 'started').exists())
            self.assertFalse((self.root / 'stop-warm').exists())
        for name, value in [('--role', 'worker'), ('--tool-backend', 'docker'), ('--output-contract-sha256', 'invalid')]:
            changed = args.copy()
            changed[changed.index(name) + 1] = value
            result = subprocess.run(self.command('framework-gyro-m-fast', *changed), input='Assignment',
                                    env=self.env, capture_output=True, text=True, timeout=10)
            self.assertNotEqual(result.returncode, 0)
            self.assertFalse((self.root / 'started').exists())

    def test_status_never_starts_or_verifies_a_model(self):
        result = subprocess.run(self.command("--status", "framework-gyro-m-fast"), env=self.env, capture_output=True, text=True, timeout=5)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout)['state'], 'starts_on_launch')
        self.assertFalse((self.root / 'started').exists())
        self.assertFalse((self.root / 'verified').exists())
        self.assertFalse((self.root / 'client').exists())

    def test_picker_resumes_before_host_start_and_preserves_context(self):
        chosen = self.root / "chosen-project"
        chosen.mkdir()
        result = self.run_fixture("picker-resume", "--sessions", "--full-verify", "--workspace", str(chosen), "--context-tokens", "65536")
        self.assertEqual(result.returncode, 0, result.stderr)
        picked = json.loads((self.root / 'picked').read_text())
        self.assertIn(str(chosen), picked)
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

    def test_unprepared_project_environment_fails_before_host_ownership(self):
        directory = self.root / '.devcontainer'
        directory.mkdir()
        (directory / 'devcontainer.json').write_text(json.dumps({'image':'fixture:local','customizations':{'playtime':{}}}))
        result = self.run_fixture('', '--prompt', 'hello')
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn('not prepared', result.stderr)
        self.assertFalse((self.root / 'started').exists())
        self.assertFalse((self.root / 'client').exists())

    def test_saved_environment_is_forwarded_to_the_client(self):
        session = self.root / 'saved'
        session.mkdir(mode=0o700)
        (session / 'config.json').write_text('{}')
        lock = session / 'environment.lock.json'
        lock.write_text('{}')
        lock.chmod(0o600)
        project = self.root / 'project'
        project.mkdir()
        result = self.run_fixture('', '--workspace', str(project), '--session', str(session), '--prompt', 'hello')
        self.assertEqual(result.returncode, 0, result.stderr)
        argv = json.loads((self.root / 'client').read_text())['argv']
        self.assertEqual(argv[argv.index('--image-lock') + 1], str(lock))

    def test_environment_preparation_help_does_not_touch_inference(self):
        result = self.run_fixture('', '--prepare-environment', '--help')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse((self.root / 'started').exists())

    def test_files_added_to_the_package_cannot_add_code(self):
        planted = self.root / 'planted'
        planted.mkdir()
        # Two standard modules, and optional ones the standard library tries but
        # lacks on Linux, so their lookups reach every import path entry.
        for name in ('json', 'subprocess', '_winapi', 'msvcrt', 'nt'):
            (self.package / f'{name}.py').write_text(f"open({str(planted / name)!r}, 'w').close()\n")
        # Its header matches the source's size and mtime, so an ordinary import
        # would run this bytecode instead of the source.
        source = self.package / 'launch_protocol.py'
        info = source.stat()
        cached = Path(importlib.util.cache_from_source(str(source)))
        cached.parent.mkdir()
        code = compile(f"open({str(planted / 'bytecode')!r}, 'w').close()\n", str(source), 'exec')
        cached.write_bytes(importlib.util.MAGIC_NUMBER + bytes(4) + int(info.st_mtime).to_bytes(4, 'little')
                           + (info.st_size & 0xFFFFFFFF).to_bytes(4, 'little') + marshal.dumps(code))
        # A caller that appends the package to the import path. It loads what
        # runpy imports first, so only the launcher's own imports are tested.
        appended = ('import pkgutil, runpy, sys; sys.path.append(sys.argv[1]); del sys.argv[:2]; '
                    "runpy.run_path(sys.argv[0], run_name='__main__')")
        # Preparation imports both modules and runs one of them as a program.
        arguments = ['framework-gyro-m-fast', '--prepare-environment', '--help']
        for command in (self.command(*arguments),
                        [sys.executable, '-I', '-S', '-B', '-c', appended, str(self.package),
                         str(self.package / 'run-strata.py'), *arguments]):
            with self.subTest(command=command[1]):
                result = subprocess.run(command, env=self.env, cwd=self.root, capture_output=True, text=True, timeout=15)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn('--workspace', result.stdout)
                self.assertEqual(list(planted.iterdir()), [])
                # Nothing read the planted bytecode, and nothing wrote more.
                self.assertEqual([path.name for path in cached.parent.iterdir()], [cached.name])

    def test_modules_a_caller_already_loaded_are_kept(self):
        # This caller runs the launcher from bytes it read itself, with its own
        # launch_protocol, so a file read again could differ from what it checked.
        caller = ('import sys; path = sys.argv.pop(1); module = sys.modules["launch_protocol"] = type(sys)("launch_protocol"); '
                  'module.DESCRIBE_FLAG = "--describe-launch"; '
                  'module.describe = lambda preset, profile: print("caller module") or 0; '
                  'exec(compile(open(path, "rb").read(), path, "exec"), {"__name__": "__main__", "__file__": path})')
        result = subprocess.run([sys.executable, '-I', '-S', '-B', '-c', caller, str(self.package / 'run-strata.py'),
                                 'framework-gyro-m-fast', '--describe-launch'],
                                env=self.env, cwd=self.root, capture_output=True, text=True, timeout=15)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, 'caller module\n')

    def test_picker_cancel_and_failure_never_start_host(self):
        for mode, expected in [('picker-cancel', 0), ('picker-failure', 2)]:
            result = self.run_fixture(mode, '--sessions')
            self.assertEqual(result.returncode, expected, result.stderr)
            self.assertTrue((self.root / 'picked').exists())
            self.assertFalse((self.root / 'started').exists())
            self.assertFalse((self.root / 'client').exists())
            if mode == 'picker-failure':
                self.assertEqual(result.stderr.strip(), 'playtime-chat: fixture workspace rejected')

    def test_explicit_sessions_and_headless_calls_skip_picker(self):
        for args in [('--session', '/saved'), ('--prompt', '--sessions'), ('--recover-interrupted', '/stopped'),
                     ('--agent-prompt', 'minimal', '--session', '/saved'), ('--stream', '--session', '/saved')]:
            result = self.run_fixture('', *args)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertFalse((self.root / 'picked').exists())
            argv = json.loads((self.root / 'client').read_text())['argv']
            self.assertEqual(argv[-len(args):], list(args))

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
                         str(self.package / 'bird-strata-gyro-m-fast-v2.lock.json'))
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

    def llama_fixture(self):
        """A local llama-server profile's owner, verifier and lock, in the package."""
        self.write_program(self.package / "llama-session.py", r'''
import json, os, pathlib, sys
root = pathlib.Path(os.environ['FIXTURE_ROOT'])
with (root / 'llama-helper-args').open('a') as calls:
    calls.write(json.dumps(sys.argv[1:]) + '\n')
if sys.argv[1] == '--status':
    print(json.dumps({'state': 'starts_on_launch'}))
    sys.exit(0)
if sys.argv[1] == '--stop':
    sys.exit(0)
profile = sys.argv[1]
state = root / '.local/state/playtime-llama' / profile / '20261125T123456Z-123'
print('llama-server: starting ' + profile + '; logs: ' + str(state), file=sys.stderr, flush=True)
print('GYRO_READY ' + json.dumps({'model': 'fixture', 'context': 40960, 'parallel': 1}), flush=True)
while line := sys.stdin.readline():
    with (root / 'owner-input').open('a') as received:
        received.write(line)
(root / 'restored').touch()
''')
        (self.package / "verify-strata-host.py").write_text(r'''
import json, os, pathlib, sys
root = pathlib.Path(os.environ['FIXTURE_ROOT'])
(root / 'local-verify').write_text(json.dumps({'argv': sys.argv, 'isolated': sys.flags.isolated}))
host = json.loads(sys.argv[sys.argv.index('--expected-host') + 1])
print(json.dumps({'host': host, 'verification': {'reused_files': 1, 'hashed_files': 1, 'hashed_bytes': 10}}))
''')
        lock = {"identity": {"service": "llama_server", "model": "fixture", "context_tokens": 40960},
                "host": {"digest": "llama-reviewed"}}
        (self.package / "local-llama-qwen3-1.7b-v1.lock.json").write_text(json.dumps(lock))
        return lock

    def llama_helper_calls(self):
        path = self.root / 'llama-helper-args'
        return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []

    def test_local_llama_handoff_owns_and_verifies_beside_the_wrapper_without_ssh(self):
        lock = self.llama_fixture()
        result = subprocess.run(self.command('local-llama-qwen3-1.7b', '--prompt', 'hello'),
                                env=self.env, cwd=self.root, capture_output=True, text=True, timeout=15)
        self.assertEqual(result.returncode, 0, result.stderr)
        [owner] = self.llama_helper_calls()
        child = json.loads((self.root / 'client').read_text())
        endpoint = child['argv'][child['argv'].index('--endpoint') + 1]
        # The server listens on the wrapper's reserved port itself, with no forward.
        self.assertEqual(owner, ['local-llama-qwen3-1.7b', '--port', endpoint.split(':')[-1].removesuffix('/v1')])
        self.assertEqual(child['argv'][child['argv'].index('--model-lock') + 1],
                         str(self.package / 'local-llama-qwen3-1.7b-v1.lock.json'))
        verified = json.loads((self.root / 'local-verify').read_text())
        self.assertEqual(verified['isolated'], 1)
        self.assertEqual(verified['argv'][1], str(self.root / '.local/state/playtime-llama/local-llama-qwen3-1.7b/20261125T123456Z-123'))
        self.assertEqual(json.loads(verified['argv'][verified['argv'].index('--expected-host') + 1]), lock['host'])
        self.assertIn('GYRO_RELEASE\n', self.owner_input())
        self.assertTrue((self.root / 'restored').exists())
        for name in ('started', 'status', 'verified', 'stop-warm'):
            self.assertFalse((self.root / name).exists(), f'ssh was used: {name}')

    def test_local_llama_status_and_stop_use_the_local_helper(self):
        self.llama_fixture()
        status = subprocess.run(self.command('--status', 'local-llama-qwen3-1.7b'), env=self.env,
                                capture_output=True, text=True, timeout=5)
        self.assertEqual((status.returncode, json.loads(status.stdout)), (0, {'state': 'starts_on_launch'}), status.stderr)
        stop = subprocess.run(self.command('local-llama-qwen3-1.7b', '--stop-server'), env=self.env,
                              capture_output=True, text=True, timeout=5)
        self.assertEqual(stop.returncode, 0, stop.stderr)
        self.assertEqual(self.llama_helper_calls(), [['--status', 'local-llama-qwen3-1.7b'], ['--stop']])
        for name in ('started', 'status', 'stop-warm', 'client'):
            self.assertFalse((self.root / name).exists(), name)

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

    def test_interrupted_model_restarts_only_after_owner_shutdown(self):
        result = self.run_fixture('restart', '--session', str(self.root / 'old'))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue((self.root / 'restarted').exists())
        self.assertNotIn(str(self.root / 'old'), (self.root / 'client').read_text())

    def test_failed_owner_shutdown_blocks_restart(self):
        result = self.run_fixture('restart-restore-failure')
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse((self.root / 'restarted').exists())

    def test_restart_replaces_selection_and_preserves_launch_settings(self):
        self.assertEqual(launcher.resume_arguments(['--preset', 'coding-host', '--sessions',
            '--recover-interrupted=/old', '--acknowledge-uncertain-tools', '--context-tokens', '163840'], Path('/fork')),
            ['--preset', 'coding-host', '--context-tokens', '163840', '--session', '/fork'])

    def test_host_and_readiness_require_exact_reviewed_values(self):
        with self.assertRaises(RuntimeError):
            launcher.check_host({'host': {'digest': 'different'}}, self.lock)
        with self.assertRaises(RuntimeError):
            launcher.check_ready({'model': 'fixture', 'context': 128000, 'drafting': 'dflash2'}, self.lock)
        lock = {'identity': {'model': 'fixture', 'context_tokens': 163840, 'service': 'basalt', 'parallel': 2}}
        ready = {'model': 'fixture', 'context': 163840, 'drafting': 'mtp8', 'parallel': 2}
        launcher.check_ready(ready, lock)
        for changed in ({'parallel': 1}, {'drafting': 'dflash2'}, {'model': 'other'}):
            with self.assertRaises(RuntimeError):
                launcher.check_ready(dict(ready, **changed), lock)
        # A llama-server lock omits its one slot, and the server reports no drafting.
        lock = {'identity': {'model': 'fixture', 'context_tokens': 40960, 'service': 'llama_server'}}
        ready = {'model': 'fixture', 'context': 40960, 'parallel': 1}
        launcher.check_ready(ready, lock)
        for changed in ({'parallel': 2}, {'context': 4096}, {'model': 'other'}, {'parallel': None}):
            with self.assertRaises(RuntimeError):
                launcher.check_ready(dict(ready, **changed), lock)


if __name__ == '__main__':
    unittest.main()
