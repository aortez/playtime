"""CPU fixtures for the launcher_v1 provider path: no SSH, no models, no installs.

These reuse the owner wrapper's fake `ssh`/`playtime-chat` programs so the launch
contract is exercised through the real startup, verification, client and cleanup
order. The base launcher suite is called for its fixture only, never rerun here.
"""

import hashlib
import importlib.util
import json
import os
import signal
import subprocess
import sys
import time
import unittest
from unittest import mock
from pathlib import Path

import test_launcher

spec = importlib.util.spec_from_file_location("playtime_chat_launch_protocol",
                                              test_launcher.SOURCE / "launch_protocol.py")
protocol = importlib.util.module_from_spec(spec)
spec.loader.exec_module(protocol)

# A prompt far past the per-argument limit, with the exact Unicode and newline
# shapes a caller must get back byte for byte.
LARGE = "λ recorder 记录\r\n" + ("line with tabs\tand unicode ✓\n" * 6000) + "tail\r\n\r\n"


class LaunchProtocolTests(unittest.TestCase):
    def setUp(self):
        test_launcher.LauncherTests.setUp(self)
        self.project = self.root / "project"
        self.project.mkdir()
        self.caller = self.root / "caller"
        self.caller.mkdir(mode=0o700)
        self.prompt = self.root / "prompt.txt"
        self.prompt.write_text("Recorder instructions\n\nλ task line 1\nline 2\r\n")
        self.request_path = self.caller / "request.json"
        self.receipt = self.caller / "receipt.json"
        self.install_client()

    def tearDown(self):
        test_launcher.LauncherTests.tearDown(self)

    def write_program(self, path, source):
        test_launcher.LauncherTests.write_program(self, path, source)

    def command(self, *args):
        return test_launcher.LauncherTests.command(self, *args)

    def install_client(self):
        """A chat stand-in that reports its own tool cleanup when asked to."""
        self.write_program(self.package / "playtime-chat", r'''
import hashlib, json, os, pathlib, signal, sys, time
root = pathlib.Path(os.environ['FIXTURE_ROOT'])
argv = sys.argv[1:]
mode = os.environ.get('FIXTURE_MODE', '')
if '--prompt-file' not in argv:
    print('launch fixture needs --prompt-file', file=sys.stderr)
    sys.exit(2)
def value(name):
    return argv[argv.index(name) + 1]
prompt = pathlib.Path(value('--prompt-file')).read_bytes()
(root / 'client').write_text(json.dumps({
    'argv': argv, 'cwd': os.getcwd(), 'prompt_bytes': len(prompt),
    'prompt_sha256': hashlib.sha256(prompt).hexdigest(),
    'prompt_text': prompt.decode('utf-8') if mode == 'echo' else None}))
print('fixture client stdout', flush=True)
print('fixture client stderr', file=sys.stderr, flush=True)
def complete(outcome, tools, digest=None, launch=None, schema=1):
    if '--completion-receipt' not in argv:
        return
    pathlib.Path(value('--completion-receipt')).write_text(json.dumps({
        'schema_version': schema,
        'launch_id': value('--launch-id') if launch is None else launch,
        'prompt_sha256': hashlib.sha256(prompt).hexdigest() if digest is None else digest,
        'outcome': outcome, 'local_tools': tools, 'session_id': 'fixture-session'}))
if mode == 'silent':
    sys.exit(0)
if mode == 'crash':
    sys.exit(3)
if mode == 'forged':
    complete('completed', 'confirmed', launch='another-run')
    sys.exit(0)
if mode == 'wrong-hash':
    complete('completed', 'confirmed', digest='f' * 64)
    sys.exit(0)
if mode == 'bool-schema':
    complete('completed', 'confirmed', schema=True)
    sys.exit(0)
if mode == 'duplicate-schema':
    complete('completed', 'confirmed')
    target = pathlib.Path(value('--completion-receipt'))
    target.write_text(target.read_text().replace('{', '{"schema_version": 1,', 1))
    sys.exit(0)
if mode == 'future-schema':
    complete('completed', 'confirmed', schema=2)
    sys.exit(0)
if mode == 'unresolved-tools':
    complete('completed', 'unresolved')
    sys.exit(0)
if mode == 'turn-failed':
    complete('failed', 'confirmed')
    sys.exit(1)
if mode == 'turn-timed-out':
    complete('timed_out', 'confirmed')
    sys.exit(1)
# 'owner-loss' and 'owner-exit-zero' are shared fake-helper modes that end the owner mid-turn.
if mode in ('hang', 'owner-loss', 'owner-exit-zero'):
    while True:
        time.sleep(.05)
if mode == 'delayed-cleanup':
    def stop(*_args):
        time.sleep(.4)
        complete('cancelled', 'confirmed')
        (root / 'client-cleaned').touch()
        sys.exit(130)
    signal.signal(signal.SIGTERM, stop)
    while True:
        time.sleep(.05)
complete('completed', 'confirmed')
sys.exit(0)
''')

    def request_text(self, **changes):
        values = {"schema_version": 1, "launch_id": "run-2026_11-18_a",
                  "workspace": str(self.project), "prompt_file": str(self.prompt),
                  "prompt_sha256": hashlib.sha256(self.prompt.read_bytes()).hexdigest(),
                  "receipt_file": str(self.receipt), "timeout_seconds": 600}
        values.update(changes)
        return json.dumps(values)

    def write_request(self, text=None, **changes):
        self.request_path.write_text(self.request_text(**changes) if text is None else text)
        return self.request_path

    def run_request(self, request_path, mode="", profile="framework-gyro-m-fast", timeout=25):
        """Run one already written request against the shared fake helper/client fixture."""
        return subprocess.run(self.command(profile, protocol.REQUEST_FLAG, str(request_path)),
                              env=dict(self.env, FIXTURE_MODE=mode), cwd=self.root,
                              capture_output=True, text=True, timeout=timeout)

    def launch(self, mode="", timeout=25):
        return self.run_request(self.write_request(), mode, timeout=timeout)

    def receipt_report(self):
        return json.loads(self.receipt.read_text())

    def preset(self):
        return json.loads((self.package / "presets.json").read_text())["profiles"]["framework-gyro-m-fast"]

    def evidence(self):
        directories = list((self.root / ".local/state/playtime-launcher").iterdir())
        self.assertEqual(len(directories), 1, directories)
        return directories[0]

    def test_large_unicode_prompt_reaches_the_client_by_file_not_argv(self):
        self.prompt.write_text(LARGE)
        self.assertTrue(len(LARGE.encode()) > 128 * 1024)
        result = self.launch("echo")
        self.assertEqual(result.returncode, 0, result.stderr)
        child = json.loads((self.root / "client").read_text())
        self.assertEqual(child["prompt_text"], LARGE)
        self.assertEqual(child["prompt_sha256"], hashlib.sha256(LARGE.encode()).hexdigest())
        self.assertNotIn("--prompt", child["argv"])
        self.assertLess(len(json.dumps(child["argv"])), 4096)
        frozen = Path(child["argv"][child["argv"].index("--prompt-file") + 1])
        self.assertEqual(frozen.read_bytes(), LARGE.encode())
        self.assertEqual(self.receipt_report()["prompt_sha256"],
                         hashlib.sha256(LARGE.encode()).hexdigest())

    def test_launch_arguments_select_a_fresh_coding_host_turn(self):
        result = self.launch()
        self.assertEqual(result.returncode, 0, result.stderr)
        argv = json.loads((self.root / "client").read_text())["argv"]
        self.assertEqual(argv[argv.index("--preset") + 1], "coding-host")
        self.assertIn("--json", argv)
        self.assertEqual(argv[argv.index("--workspace") + 1], str(self.project))
        self.assertEqual(argv[argv.index("--launch-id") + 1], "run-2026_11-18_a")
        self.assertGreaterEqual(int(argv[argv.index("--wall-seconds") + 1]), 1)
        self.assertLess(int(argv[argv.index("--wall-seconds") + 1]), 600)
        for forbidden in ("--prompt", "--session", "--recover-interrupted", "--recover-compaction",
                          "--model", "--effort", "--endpoint-override"):
            self.assertNotIn(forbidden, argv)
        self.assertTrue(Path(argv[argv.index("--prompt-file") + 1]).is_relative_to(self.evidence()))
        self.assertEqual(Path(argv[argv.index("--completion-receipt") + 1]).parent,
                         self.evidence() / "private")
        self.assertFalse((self.root / "picked").exists())
        self.assertFalse((self.root / "listed").exists())

    def test_success_waits_for_the_owner_release_before_confirming(self):
        result = self.launch()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.receipt_report(),
                         {"schema_version": 1, "launch_id": "run-2026_11-18_a",
                          "prompt_sha256": hashlib.sha256(self.prompt.read_bytes()).hexdigest(),
                          "outcome": "completed", "local_tools": "confirmed",
                          "inference_owner": "confirmed", "session_id": "fixture-session"})
        self.assertTrue((self.root / "restored").exists())
        self.assertIn("GYRO_RELEASE\n", (self.root / "owner-input").read_text())
        self.assertEqual(json.loads((self.evidence() / "exit.json").read_text())["exit"], 0)

    def test_missing_client_completion_never_confirms_local_tools(self):
        result = self.launch("silent")
        self.assertNotEqual(result.returncode, 0, result.stderr)
        report = self.receipt_report()
        self.assertEqual(report["outcome"], "failed")
        self.assertEqual(report["local_tools"], "unresolved")
        self.assertEqual(report["inference_owner"], "confirmed")
        self.assertIsNone(report["session_id"])
        self.assertNotIn("GYRO_RELEASE", (self.root / "owner-input").read_text())

    def test_forged_or_unreadable_client_completion_never_confirms_local_tools(self):
        for mode in ("forged", "wrong-hash", "future-schema", "bool-schema", "duplicate-schema"):
            with self.subTest(mode=mode):
                (self.root / "client").unlink(missing_ok=True)
                self.receipt.unlink(missing_ok=True)
                self.receipt.with_name(self.receipt.name + ".started").unlink(missing_ok=True)
                result = self.launch(mode)
                self.assertNotEqual(result.returncode, 0, result.stderr)
                report = self.receipt_report()
                self.assertEqual(report["local_tools"], "unresolved", mode)
                self.assertEqual(report["outcome"], "failed", mode)
                self.assertIsNone(report["session_id"], mode)
                self.assertIn("client completion was not usable", result.stderr)

    def test_client_that_cannot_confirm_its_tools_is_reported_as_unresolved(self):
        result = self.launch("unresolved-tools")
        self.assertNotEqual(result.returncode, 0, result.stderr)
        report = self.receipt_report()
        self.assertEqual(report["outcome"], "completed")
        self.assertEqual(report["local_tools"], "unresolved")
        self.assertEqual(report["inference_owner"], "confirmed")

    def test_failed_and_timed_out_turns_keep_their_distinct_outcomes(self):
        for mode, expected in (("turn-failed", "failed"), ("turn-timed-out", "timed_out")):
            with self.subTest(mode=mode):
                self.receipt.unlink(missing_ok=True)
                self.receipt.with_name(self.receipt.name + ".started").unlink(missing_ok=True)
                result = self.launch(mode)
                self.assertNotEqual(result.returncode, 0, result.stderr)
                report = self.receipt_report()
                self.assertEqual(report["outcome"], expected)
                self.assertEqual(report["local_tools"], "confirmed")
                self.assertEqual(report["inference_owner"], "confirmed")

    def test_lost_owner_stops_the_client_and_leaves_both_owners_unresolved(self):
        # The fake helper dies on its own once the client is running, so neither
        # actor can report a release this wrapper is allowed to believe.
        result = self.launch("owner-loss")
        self.assertNotEqual(result.returncode, 0, result.stderr)
        self.assertIn("lost the inference host owner", result.stderr)
        report = self.receipt_report()
        self.assertEqual(report["outcome"], "failed")
        self.assertEqual(report["local_tools"], "unresolved")
        self.assertEqual(report["inference_owner"], "unresolved")

    def test_deadline_stops_the_client_and_never_claims_its_tools(self):
        request = self.write_request(timeout_seconds=3)
        started = time.monotonic()
        result = self.run_request(request, "hang")
        self.assertNotEqual(result.returncode, 0, result.stderr)
        self.assertLess(time.monotonic() - started, 20)
        report = self.receipt_report()
        self.assertEqual(report["outcome"], "timed_out")
        self.assertEqual(report["local_tools"], "unresolved")
        # The helper still saw EOF and shut the model down itself, so only the
        # tools this wrapper had to kill stay unproven.
        self.assertEqual(report["inference_owner"], "confirmed")

    def test_signal_waits_for_delayed_tool_cleanup_then_reports_cancellation(self):
        argv = self.command("framework-gyro-m-fast", protocol.REQUEST_FLAG, str(self.write_request()))
        with subprocess.Popen(argv, env=dict(self.env, FIXTURE_MODE="delayed-cleanup"), cwd=self.root,
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True) as process:
            deadline = time.monotonic() + 10
            while not (self.root / "client").exists():
                self.assertIsNone(process.poll())
                self.assertLess(time.monotonic(), deadline)
                time.sleep(.02)
            process.send_signal(signal.SIGINT)
            try:
                _, errors = process.communicate(timeout=20)
            finally:
                if process.poll() is None:
                    process.kill()
            self.assertEqual(process.returncode, 130, errors)
        report = self.receipt_report()
        self.assertEqual(report["outcome"], "cancelled")
        # Only possible if the receipt was written after the child finished its
        # own cleanup and was reaped, not when the signal arrived.
        self.assertEqual(report["local_tools"], "confirmed")
        self.assertEqual(report["inference_owner"], "confirmed")
        self.assertTrue((self.root / "client-cleaned").exists())
        self.assertTrue((self.root / "restored").exists())
        self.assertNotIn("GYRO_RELEASE", (self.root / "owner-input").read_text())

    def test_mismatched_prompt_is_refused_before_the_owner_starts(self):
        result = self.run_request(self.write_request(prompt_sha256="0" * 64))
        self.assertNotEqual(result.returncode, 0, result.stderr)
        self.assertIn("the frozen prompt changed", result.stderr)
        self.assertFalse((self.root / "started").exists())
        self.assertFalse((self.root / "client").exists())
        self.assertFalse((self.root / ".local/state/playtime-launcher").exists())
        self.assertEqual(self.receipt_report(),
                         {"schema_version": 1, "launch_id": "run-2026_11-18_a",
                          "prompt_sha256": "0" * 64, "outcome": "failed",
                          "local_tools": "not_acquired", "inference_owner": "not_acquired",
                          "session_id": None})

    def test_unusable_inputs_are_refused_before_anything_is_acquired(self):
        self.prompt.write_text("   \n\t\n")
        cases = {"blank prompt": {}, "missing prompt": {"prompt_file": str(self.root / "gone.txt")},
                 "workspace is a file": {"workspace": str(self.prompt)},
                 "missing workspace": {"workspace": str(self.root / "nowhere")},
                 "symlinked prompt": {}}
        for label, changes in cases.items():
            with self.subTest(label=label):
                if label == "symlinked prompt":
                    link = self.root / "link.txt"
                    if not link.exists():
                        link.symlink_to(self.prompt)
                    changes = {"prompt_file": str(link),
                               "prompt_sha256": hashlib.sha256(self.prompt.read_bytes()).hexdigest()}
                self.receipt.unlink(missing_ok=True)
                self.receipt.with_name(self.receipt.name + ".started").unlink(missing_ok=True)
                argv = self.command("framework-gyro-m-fast", protocol.REQUEST_FLAG,
                                    str(self.write_request(**changes)))
                result = subprocess.run(argv, env=self.env, cwd=self.root, capture_output=True,
                                        text=True, timeout=10)
                self.assertNotEqual(result.returncode, 0, label)
                self.assertFalse((self.root / "started").exists(), label)
                self.assertFalse((self.root / "client").exists(), label)

    def test_malformed_requests_are_refused_without_a_receipt(self):
        bad = {"unknown key": self.request_text(extra=1),
               "missing key": json.dumps({"schema_version": 1, "launch_id": "run-1"}),
               "future schema": self.request_text(schema_version=2),
               "bool schema": self.request_text(schema_version=True),
               "duplicate schema": self.request_text().replace("{", '{"schema_version":1,', 1),
               "path-like launch id": self.request_text(launch_id=str(self.project)),
               "empty launch id": self.request_text(launch_id=""),
               "uppercase hash": self.request_text(prompt_sha256="A" * 64),
               "short hash": self.request_text(prompt_sha256="abc"),
               "relative prompt": self.request_text(prompt_file="prompt.txt"),
               "NUL in prompt path": self.request_text(prompt_file="/tmp/a\x00b"),
               "lone surrogate in prompt path": self.request_text(prompt_file="/tmp/\ud800"),
               "NUL in workspace": self.request_text(workspace=str(self.project) + "\x00"),
               "lone surrogate in receipt path": self.request_text(receipt_file=str(self.caller / "\ud800")),
               "zero timeout": self.request_text(timeout_seconds=0),
               "huge timeout": self.request_text(timeout_seconds=86_401),
               "fractional timeout": self.request_text(timeout_seconds=60.5),
               "truncated json": self.request_text()[:40],
               "json array": "[1, 2]",
               "not json": "launch me"}
        for label, text in bad.items():
            with self.subTest(label=label):
                self.request_path.write_text(text)
                result = subprocess.run(self.command("framework-gyro-m-fast", protocol.REQUEST_FLAG,
                                                     str(self.request_path)),
                                        env=self.env, cwd=self.root, capture_output=True,
                                        text=True, timeout=10)
                self.assertNotEqual(result.returncode, 0, label)
                self.assertFalse(self.receipt.exists(), label)
                self.assertFalse(self.receipt.with_name("receipt.json.started").exists(), label)
                self.assertFalse((self.root / "started").exists(), label)
                self.assertFalse((self.root / "client").exists(), label)

    def test_any_failure_after_the_claim_still_publishes_a_receipt(self):
        with mock.patch.object(protocol.Request, "check_inputs", side_effect=RuntimeError("disk vanished")):
            with self.assertRaisesRegex(RuntimeError, "disk vanished"):
                protocol.begin(self.preset(), "framework-gyro-m-fast", str(self.write_request()))
        report = self.receipt_report()
        self.assertEqual((report["outcome"], report["local_tools"], report["inference_owner"]),
                         ("failed", "not_acquired", "not_acquired"))

    def test_signal_after_the_claim_publishes_a_cancelled_receipt_without_starting(self):
        # The window between the claim and the session's own signal handlers.
        import launch_protocol
        begin = launch_protocol.begin

        def begin_then_signal(*args):
            launch = begin(*args)
            signal.raise_signal(signal.SIGTERM)
            return launch
        with mock.patch.object(launch_protocol, "begin", begin_then_signal), \
                mock.patch.object(test_launcher.launcher, "session", side_effect=AssertionError("session started")):
            code = test_launcher.launcher.one_shot(self.preset(), "framework-gyro-m-fast", str(self.write_request()))
        self.assertEqual(code, 128 + signal.SIGTERM)
        report = self.receipt_report()
        self.assertEqual((report["outcome"], report["local_tools"], report["inference_owner"]),
                         ("cancelled", "not_acquired", "not_acquired"))

    def test_relative_request_name_is_refused(self):
        self.write_request()
        result = subprocess.run(self.command("framework-gyro-m-fast", protocol.REQUEST_FLAG,
                                             "caller/request.json"),
                                env=self.env, cwd=self.root, capture_output=True, text=True, timeout=10)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("must be an absolute path", result.stderr)
        self.assertFalse((self.root / "started").exists())

    def test_oversized_request_is_refused_without_being_buffered(self):
        self.request_path.write_text(self.request_text() + " " * (64 * 1024))
        result = subprocess.run(self.command("framework-gyro-m-fast", protocol.REQUEST_FLAG,
                                             str(self.request_path)),
                                env=self.env, cwd=self.root, capture_output=True, text=True, timeout=10)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("exceeds 65536 bytes", result.stderr)
        self.assertFalse(self.receipt.exists())

    def test_existing_receipt_is_never_rerun_or_overwritten(self):
        self.assertEqual(self.launch().returncode, 0)
        written = self.receipt.read_bytes()
        (self.root / "client").unlink()
        (self.root / "started").unlink()
        result = self.run_request(self.request_path)
        self.assertNotEqual(result.returncode, 0, result.stderr)
        self.assertIn("never rerun over it", result.stderr)
        self.assertEqual(self.receipt.read_bytes(), written)
        self.assertFalse((self.root / "client").exists())
        self.assertFalse((self.root / "started").exists())

    def test_receipt_must_land_in_a_private_caller_owned_directory(self):
        shared = self.root / "shared"
        shared.mkdir()
        shared.chmod(0o755)
        target = self.caller / "target.json"
        link = self.root / "link-receipt.json"
        link.symlink_to(target)
        cases = {"group readable parent": (shared / "receipt.json", "must be private"),
                 "missing parent": (self.root / "absent" / "receipt.json", "unavailable"),
                 "symlinked receipt": (link, "already exists")}
        for label, (path, reason) in cases.items():
            with self.subTest(label=label):
                target.write_text("{}\n")
                result = self.run_request(self.write_request(receipt_file=str(path)))
                self.assertNotEqual(result.returncode, 0, label)
                self.assertIn(reason, result.stderr)
                self.assertFalse((self.root / "started").exists(), label)
                self.assertEqual(target.read_bytes(), b"{}\n")

    def test_launch_entry_point_accepts_no_other_chat_arguments(self):
        for argv in ((protocol.REQUEST_FLAG, str(self.write_request()), "--prompt", "hello"),
                     (protocol.REQUEST_FLAG,), (protocol.DESCRIBE_FLAG, "--full-verify")):
            with self.subTest(argv=argv):
                result = subprocess.run(self.command("framework-gyro-m-fast", *argv), env=self.env,
                                        cwd=self.root, capture_output=True, text=True, timeout=10)
                self.assertNotEqual(result.returncode, 0)
                self.assertFalse((self.root / "started").exists())

    def test_launch_flags_after_other_wrapper_flags_are_refused_before_ownership(self):
        for argv in (("--full-verify", protocol.DESCRIBE_FLAG),
                     ("--preset", "coding-host", protocol.DESCRIBE_FLAG),
                     ("--full-verify", protocol.REQUEST_FLAG, str(self.write_request()))):
            with self.subTest(argv=argv):
                result = subprocess.run(self.command("framework-gyro-m-fast", *argv), env=self.env,
                                        cwd=self.root, capture_output=True, text=True, timeout=10)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("must directly follow the profile", result.stderr)
                self.assertFalse((self.root / "started").exists())
                self.assertFalse(self.receipt.with_name("receipt.json.started").exists())

    def test_describe_launch_reports_the_contract_without_acquiring_anything(self):
        result = subprocess.run(self.command("framework-gyro-m-fast", protocol.DESCRIBE_FLAG),
                                env=self.env, cwd=self.root, capture_output=True, text=True, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout),
                         {"schema_version": 1, "protocol": "launcher_v1",
                          "max_prompt_bytes": 262144, "cleanup_timeout_seconds": 240})
        for name in ("started", "verified", "client", "status", "stop-warm"):
            self.assertFalse((self.root / name).exists(), name)
        self.assertFalse((self.root / ".local/state/playtime-launcher").exists())

    def test_every_shipped_preset_reports_cleanup_within_the_contract_bound(self):
        presets = json.loads((self.package / "presets.json").read_text())["profiles"]
        self.assertTrue(presets)
        for profile, preset in presets.items():
            with self.subTest(profile=profile):
                budget = protocol.cleanup_budget(preset)
                self.assertGreater(budget, preset["cleanup_timeout"])
                self.assertLessEqual(budget, protocol.MAX_CLEANUP_BUDGET)

    def test_a_preset_that_cannot_fit_the_cleanup_bound_is_refused(self):
        preset = dict(self.preset(), cleanup_timeout=400)
        with self.assertRaises(protocol.LaunchError):
            protocol.describe(preset, "framework-gyro-m-fast")
        with self.assertRaisesRegex(protocol.LaunchError, "cleanup"):
            protocol.begin(preset, "framework-gyro-m-fast", str(self.write_request()))
        self.assertFalse(self.receipt.with_name("receipt.json.started").exists())

    def test_receipt_publication_exposes_only_complete_bytes(self):
        launch = protocol.begin({"cleanup_timeout": 1}, "fixture", self.write_request())
        real_link = os.link
        def inspect_link(source, destination):
            self.assertFalse(self.receipt.exists())
            self.assertEqual(json.loads(Path(source).read_bytes())["launch_id"], launch.request.launch_id)
            real_link(source, destination)
        with mock.patch.object(protocol.os, "link", side_effect=inspect_link):
            launch.publish("completed", "confirmed", "confirmed", None)
        self.assertEqual(self.receipt_report()["outcome"], "completed")
        self.assertEqual(list(self.caller.glob(".receipt.json.*")), [])

    def test_receipt_sync_failures_do_not_acknowledge_publication(self):
        launch = protocol.begin({"cleanup_timeout": 1}, "fixture", self.write_request())
        with mock.patch.object(protocol.os, "fsync", side_effect=OSError("file sync failed")):
            with self.assertRaises(OSError):
                launch.publish("completed", "confirmed", "confirmed", None)
        self.assertFalse(self.receipt.exists())
        self.assertIsNone(launch.published)
        self.assertEqual(list(self.caller.glob(".receipt.json.*")), [])
        with mock.patch.object(protocol, "sync_directory", side_effect=OSError("directory sync failed")):
            with self.assertRaises(OSError):
                launch.publish("completed", "confirmed", "confirmed", None)
        self.assertEqual(self.receipt_report()["outcome"], "completed")
        self.assertIsNone(launch.published)

    def test_publish_never_replaces_an_existing_receipt(self):
        launch = protocol.begin({"cleanup_timeout": 1}, "fixture", self.write_request())
        self.receipt.write_bytes(b"other writer")
        with self.assertRaises(FileExistsError):
            launch.publish("completed", "confirmed", "confirmed", None)
        self.assertEqual(self.receipt.read_bytes(), b"other writer")
        self.assertEqual(list(self.caller.glob(".receipt.json.*")), [])

    def test_persistent_claim_refuses_restart_without_a_receipt(self):
        protocol.begin({"cleanup_timeout": 1}, "fixture", self.write_request())
        claim = self.receipt.with_name(self.receipt.name + ".started")
        before = claim.read_bytes()
        self.assertFalse(self.receipt.exists())
        result = self.run_request(self.request_path)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("instead of replaying", result.stderr)
        self.assertFalse((self.root / "started").exists())
        self.assertEqual(claim.read_bytes(), before)

    def test_concurrent_request_dispatches_only_once(self):
        self.write_request()
        command = self.command("framework-gyro-m-fast", protocol.REQUEST_FLAG, str(self.request_path))
        processes = [subprocess.Popen(command, env=self.env, cwd=self.root,
                                      stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
                     for _ in range(2)]
        try:
            outputs = [process.communicate(timeout=20) for process in processes]
            self.assertEqual(sorted(process.returncode for process in processes), [0, 1], outputs)
            self.assertEqual((self.root / "owner-starts").read_text().splitlines(), ["start"])
            self.assertEqual(self.receipt_report()["outcome"], "completed")
        finally:
            for process in processes:
                if process.poll() is None:
                    process.kill()
                    process.wait()

    def test_owner_exit_before_release_is_unresolved_even_when_it_exits_zero(self):
        result = self.launch("owner-exit-zero")
        self.assertNotEqual(result.returncode, 0, result.stderr)
        self.assertIn("lost the inference host owner", result.stderr)
        self.assertEqual(json.loads((self.evidence() / "exit.json").read_text())["owner_exit"], 0)
        self.assertEqual(self.receipt_report()["inference_owner"], "unresolved")

    def test_launch_output_stays_in_its_evidence_not_the_callers_pipes(self):
        result = self.launch()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("fixture client", result.stdout + result.stderr)
        self.assertNotIn("Gyro-M:", result.stderr)
        evidence = self.evidence()
        self.assertEqual((evidence / "client.stdout").read_text(), "fixture client stdout\n")
        self.assertEqual((evidence / "client.stderr").read_text(), "fixture client stderr\n")
        self.assertIn("Gyro-M: starting", (evidence / "owner.log").read_text())

    def test_forced_owner_shutdown_is_unresolved_even_when_it_exits_zero(self):
        path = self.package / "presets.json"
        config = json.loads(path.read_text())
        config["profiles"]["framework-gyro-m-fast"]["cleanup_timeout"] = .2
        path.write_text(json.dumps(config))
        result = self.launch("owner-cleanup-timeout")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(json.loads((self.evidence() / "exit.json").read_text())["owner_exit"], 0)
        self.assertEqual(self.receipt_report()["local_tools"], "confirmed")
        self.assertEqual(self.receipt_report()["inference_owner"], "unresolved")


    def test_file_prompt_through_the_ordinary_wrapper_skips_the_session_picker(self):
        # A generic file prompt is as explicit as --prompt: no picker, no resume,
        # and the environment scan must not trip over the extra chat option.
        result = test_launcher.LauncherTests.run_interactive(self, "", "--prompt-file", str(self.prompt))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse((self.root / "listed").exists())
        self.assertFalse((self.root / "picked").exists())
        argv = json.loads((self.root / "client").read_text())["argv"]
        self.assertEqual(argv[-2:], ["--prompt-file", str(self.prompt)])


if __name__ == "__main__":
    unittest.main()
