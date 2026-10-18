"""Deployment contract tests; no host mutation, SSH, models or third parties."""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest import mock

SCRIPT = Path(__file__).resolve().parents[1] / "scripts/backend-host-deploy.py"
SPEC = importlib.util.spec_from_file_location("backend_host_deploy", SCRIPT)
deploy = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(deploy)


class HostDeploymentTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="pt-host-deploy-")
        self.root = Path(self.temporary.name)
        self.addCleanup(self.temporary.cleanup)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        for name in ("playtime-backend-host", "playtime-backend-bridge"):
            (self.bin / name).write_bytes(b"\x7fELFsynthetic-test-only")
        self.key = self.root / "public.key"
        self.key.write_text("ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAISyntheticFixturePublicOnly fixture\n")
        self.lock = self.root / "lock.json"
        self.lock.write_bytes(b"synthetic-lock-not-a-supported-live-profile")
        self.args = argparse.Namespace(stage=self.root / "stage", public_key=self.key, guard_uid=65432,
                                       bin_dir=self.bin, model_lock=self.lock)
        profile = mock.patch.object(deploy, "inspect_profile", return_value={"profile_sha256": "0" * 64})
        profile.start()
        self.addCleanup(profile.stop)

    def test_stage_is_repeatable_hash_bound_and_outside_live_paths(self):
        with mock.patch.object(deploy, "run", side_effect=AssertionError("no live command during prepare")):
            first = deploy.prepare(self.args)
            self.assertEqual(deploy.prepare(self.args), first)
            self.assertEqual(deploy.verify(self.args.stage), first)
        firewall = (self.args.stage / "firewall.nft").read_text()
        self.assertIn("meta skuid != 65432", firewall)
        self.assertIn("table inet playtime_backend", firewall)
        self.assertNotIn("flush ruleset", firewall)
        self.assertNotIn("@GUARD_UID@", firewall)
        keys = (self.args.stage / "authorized_keys").read_text()
        self.assertTrue(keys.startswith('restrict,command="/usr/libexec/playtime-backend-bridge" ssh-ed25519 '))
        (self.args.stage / "sshd.conf").write_text("modified")
        with self.assertRaisesRegex(RuntimeError, "artifact drift"):
            deploy.verify(self.args.stage)

    def test_no_private_keys_special_files_or_unreviewed_activation(self):
        self.key.write_text("-----BEGIN OPENSSH PRIVATE KEY-----")
        with self.assertRaisesRegex(RuntimeError, "public Ed25519"):
            deploy.prepare(self.args)
        fifo = self.root / "fifo"
        os.mkfifo(fifo)
        with self.assertRaisesRegex(RuntimeError, "invalid deployment file"):
            deploy.read_regular(fifo)
        link = self.root / "link"
        link.symlink_to(self.lock)
        with self.assertRaises(OSError):
            deploy.read_regular(link)

    def test_guard_survives_backend_stop_to_release_terminated_lease(self):
        import configparser
        deploy.prepare(self.args)
        guard = configparser.ConfigParser(interpolation=None)
        guard.read(self.args.stage / "playtime-backend-guard.service")
        self.assertEqual(guard["Unit"]["BindsTo"].split(), ["playtime-backend-owner.service"])
        self.assertIn("ollama.service", guard["Unit"]["After"].split())
        backend = configparser.ConfigParser(interpolation=None)
        backend.read(self.args.stage / "ollama-managed.conf")
        self.assertIn("playtime-backend-owner.service", backend["Unit"]["BindsTo"].split())
        self.assertEqual(backend["Service"]["Restart"], "no")

    def rollback_fixture(self):
        stage = deploy.prepare(self.args)
        state = self.root / "state"
        state.mkdir()
        installed = self.root / "owned.conf"
        installed.write_bytes(b"managed policy")
        journal = self.root / "guard-journal"
        journal.write_bytes(b"unresolved request must survive")
        tx = dict(plan_sha256=stage["plan_sha256"], phase="closed", table_sha256=deploy.digest(b"owned table"),
                  installed={str(installed): deploy.snapshot(installed)},
                  before=dict(files={str(installed): None}, units={"ollama.service": {"ActiveState": "active"}}))
        deploy.atomic(state / "transaction.json", deploy.encode(tx))
        handle = os.open(self.root / "lock", os.O_CREAT | os.O_RDWR, 0o600)
        return stage, state, installed, journal, handle

    def test_rollback_never_reopens_on_uncertain_cleanup(self):
        stage, state, installed, journal, handle = self.rollback_fixture()
        events = []

        def run(argv, **_):
            events.append(argv)
            return argparse.Namespace(stdout=b"owned table", returncode=0)

        with mock.patch.object(deploy, "STATE", state), mock.patch.object(deploy, "require_admin", return_value=(stage, handle)), \
                mock.patch.object(deploy, "run", side_effect=run), mock.patch.object(deploy, "HeldCgroup") as held:
            held.return_value.prove.side_effect = RuntimeError("termination uncertain")
            with self.assertRaisesRegex(RuntimeError, "termination uncertain"):
                deploy.deactivate(self.args)
        self.assertTrue(installed.exists())
        self.assertEqual(journal.read_bytes(), b"unresolved request must survive")
        self.assertFalse(any("delete" in argv or "start" in argv for argv in events))

    def test_rollback_restores_only_owned_changes_after_proof_and_detects_drift(self):
        stage, state, installed, journal, handle = self.rollback_fixture()
        events = []

        def run(argv, **_):
            events.append(argv)
            return argparse.Namespace(stdout=b"owned table", returncode=0)

        with mock.patch.object(deploy, "STATE", state), mock.patch.object(deploy, "require_admin", return_value=(stage, handle)), \
                mock.patch.object(deploy, "run", side_effect=run), mock.patch.object(deploy, "HeldCgroup") as held:
            held.return_value.prove.side_effect = lambda: events.append(["proved"])
            result = deploy.deactivate(self.args)
        self.assertEqual(result["phase"], "legacy-restored-journals-retained")
        self.assertFalse(installed.exists())
        self.assertTrue(journal.exists())
        self.assertLess(events.index(["proved"]), events.index(["nft", "delete", "table", "inet", "playtime_backend"]))
        self.assertLess(events.index(["proved"]), events.index(["systemctl", "start", "ollama.service"]))

    @unittest.skipUnless(os.environ.get("PLAYTIME_DEPLOY_FIXTURE_UNIT"), "explicit disposable systemd rollback fixture")
    def test_real_systemd_rollback_waits_for_original_process_tree(self):
        import subprocess
        unit = os.environ["PLAYTIME_DEPLOY_FIXTURE_UNIT"]
        real_run = deploy.run
        real_proof = deploy.HeldCgroup
        for stop in (False, True):
            # A fresh disposable transaction is used per fault case; no
            # ownership journal is removed by the rollback itself.
            self.args.stage = self.root / ("stage-stopped" if stop else "stage-running")
            stage = deploy.prepare(self.args)
            state = self.root / ("tx-stopped" if stop else "tx-running")
            state.mkdir()
            installed = state / "owned.conf"
            installed.write_bytes(b"reviewed policy")
            journal = state / "journal"
            journal.write_bytes(b"retain pending ownership")
            tx = dict(plan_sha256=stage["plan_sha256"], phase="closed", table_sha256=deploy.digest(b"owned table"),
                      installed={str(installed): deploy.snapshot(installed)},
                      before=dict(files={str(installed): None}, units={"ollama.service": {"ActiveState": "inactive"}}))
            deploy.atomic(state / "transaction.json", deploy.encode(tx))
            handle = os.open(state / ".lock", os.O_CREAT | os.O_RDWR, 0o600)
            events = []

            def run(argv, **kwargs):
                if "show" in argv:
                    return real_run(argv, **kwargs)
                events.append(argv)
                if argv[:3] == ["systemctl", "stop", "--no-block"] and stop:
                    return subprocess.run(["systemctl", "--user", "stop", "--no-block", unit], check=True, capture_output=True)
                return argparse.Namespace(stdout=b"owned table", returncode=0)

            with mock.patch.object(deploy, "STATE", state), mock.patch.object(deploy, "require_admin", return_value=(stage, handle)), \
                    mock.patch.object(deploy, "run", side_effect=run), \
                    mock.patch.object(deploy, "HeldCgroup", side_effect=lambda: real_proof(fixture_unit=unit, fixture_observation_ms=1500)):
                if stop:
                    deploy.deactivate(self.args)
                    self.assertFalse(installed.exists())
                else:
                    with self.assertRaisesRegex(RuntimeError, "termination remains uncertain"):
                        deploy.deactivate(self.args)
                    self.assertTrue(installed.exists())
                    self.assertFalse(any("delete" in argv for argv in events))
                self.assertTrue(journal.exists())


if __name__ == "__main__":
    unittest.main()
