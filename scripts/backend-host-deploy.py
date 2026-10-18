#!/usr/bin/env python3
"""Fixed-host staged deployment. No installer, downloads, model or key generation.

prepare/inspect are read-only with respect to live paths. activate/deactivate
require root and the exact reviewed plan digest. Interrupted transitions stay
closed; rollback never clears the durable ownership journals.
"""
import argparse
import base64
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import subprocess
import sys
import tempfile
import time

REPO = Path(__file__).resolve().parent.parent
SOURCE = REPO / "deploy/backend-host"
if (Path(__file__).resolve().parent / "plan.json").is_file():
    SOURCE = Path(__file__).resolve().parent
FILES = {
    "playtime-backend-host": "/usr/libexec/playtime-backend-host",
    "playtime-backend-bridge": "/usr/libexec/playtime-backend-bridge",
    "playtime-backend-policy": "/usr/libexec/playtime-backend-policy",
    "playtime-backend-owner.service": "/etc/systemd/system/playtime-backend-owner.service",
    "playtime-backend-guard.service": "/etc/systemd/system/playtime-backend-guard.service",
    "playtime-backend-firewall.service": "/etc/systemd/system/playtime-backend-firewall.service",
    "ollama-managed.conf": "/etc/systemd/system/ollama.service.d/90-playtime-managed.conf",
    "sshd.conf": "/etc/ssh/sshd_config.d/90-playtime-backend.conf",
    "authorized_keys": "/etc/playtime-backend/authorized_keys",
    "firewall.nft": "/etc/playtime-backend/firewall.nft",
    "model.json": "/etc/playtime-backend/model.json",
}
STATE = Path("/var/lib/playtime-backend-deployment")
UNITS = ("ollama.service", "playtime-backend-owner.service", "playtime-backend-guard.service", "playtime-backend-firewall.service")


def digest(data):
    return hashlib.sha256(data).hexdigest()


def encode(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":")).encode() + b"\n"


def run(argv, *, check=True, data=None):
    result = subprocess.run(argv, input=data, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=125)
    if check and result.returncode:
        raise RuntimeError("fixed host operation failed: " + " ".join(argv[:2]))
    if len(result.stdout) > 4 * 1024 * 1024:
        raise RuntimeError("inspection exceeded bound")
    return result


def read_regular(path, limit=256 * 1024 * 1024, *, allow_build_hardlinks=False):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd, "rb") as source:
        info = os.fstat(source.fileno())
        if not stat.S_ISREG(info.st_mode) or (info.st_nlink != 1 and not allow_build_hardlinks) or info.st_size > limit:
            raise RuntimeError("invalid deployment file")
        data = source.read(limit + 1)
        if len(data) > limit:
            raise RuntimeError("deployment file grew beyond bound")
        return data, info


def snapshot(path):
    try:
        data, info = read_regular(path)
    except FileNotFoundError:
        return None
    return dict(sha256=digest(data), bytes=base64.b64encode(data).decode(),
                mode=stat.S_IMODE(info.st_mode), uid=info.st_uid, gid=info.st_gid)


def atomic(path, data, mode=0o600, uid=None, gid=None):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    for parent in (path.parent, *path.parent.parents):
        info = parent.lstat()
        if not stat.S_ISDIR(info.st_mode) or (uid is not None and (info.st_uid != 0 or info.st_mode & 0o022)):
            raise RuntimeError("untrusted installation parent")
    fd, temporary = tempfile.mkstemp(prefix=".playtime-stage-", dir=path.parent)
    try:
        with os.fdopen(fd, "wb") as output:
            os.fchmod(output.fileno(), mode)
            if uid is not None:
                os.fchown(output.fileno(), uid, gid)
            output.write(data)
            output.flush()
            os.fsync(output.fileno())
        os.replace(temporary, path)
        directory = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(directory)
        finally:
            os.close(directory)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def prepare(args):
    output = args.stage.resolve()
    if output.exists():
        return verify(output)
    key = read_regular(args.public_key, 4096)[0].decode().strip()
    if not re.fullmatch(r"ssh-ed25519 [A-Za-z0-9+/]+={0,3}(?: [^\r\n]+)?", key):
        raise RuntimeError("a dedicated public Ed25519 key is required; never pass a private key")
    if not 1 <= args.guard_uid < (1 << 32):
        raise RuntimeError("invalid dedicated guard UID")
    assets = {}
    for name in FILES:
        if name in ("playtime-backend-host", "playtime-backend-bridge"):
            # Cargo links its final binary to deps/; only this trusted build
            # input may have multiple links. Installed/staged files cannot.
            data = read_regular(args.bin_dir / name, allow_build_hardlinks=True)[0]
            if not data.startswith(b"\x7fELF"):
                raise RuntimeError("build native host binaries before preparing deployment")
        elif name == "model.json":
            data = read_regular(args.model_lock, 16 * 1024 * 1024)[0]
        elif name == "authorized_keys":
            data = (SOURCE / "authorized_keys.template").read_bytes().replace(b"@PUBLIC_ED25519_KEY@", key.encode())
        else:
            data = (SOURCE / name).read_bytes().replace(b"@GUARD_UID@", str(args.guard_uid).encode())
        assets[name] = data
    profile = inspect_profile(args.bin_dir, assets["model.json"])
    plan = dict(version=1, host="oldman-bird.local", uid=args.guard_uid, profile=profile,
                deployment_tool_sha256=digest(read_regular(Path(__file__))[0]),
                manifest_sha256=digest(read_regular(SOURCE / "manifest.json")[0]),
                client_known_hosts_sha256=digest(read_regular(SOURCE / "client_known_hosts")[0]),
                files={name: dict(path=FILES[name], sha256=digest(data)) for name, data in assets.items()},
                root_bypass="Administrative root can change policy; it is outside managed mode.",
                gate="Review stage; finish existing work; then explicitly activate. Live acceptance remains outstanding.")
    output.mkdir(mode=0o700, parents=True)
    for name, data in assets.items():
        atomic(output / name, data)
    atomic(output / "backend-host-deploy.py", read_regular(Path(__file__))[0])
    atomic(output / "manifest.json", read_regular(SOURCE / "manifest.json")[0])
    atomic(output / "client_known_hosts", read_regular(SOURCE / "client_known_hosts")[0])
    atomic(output / "plan.json", encode(plan))
    return verify(output)


def inspect_profile(bin_dir, lock):
    return json.loads(run([str(bin_dir / "playtime-backend-profile")], data=lock).stdout)


def prepare_private(args):
    import secrets
    checked = verify(args.stage)
    if args.output.exists() or args.output.resolve().is_relative_to(REPO):
        raise RuntimeError("private config requires a new path outside the repository")
    config = dict(version=1, backend_id=secrets.token_hex(16), guard_id=secrets.token_hex(16),
                  generation=1, uid=checked["plan"]["uid"], credential=secrets.token_hex(32),
                  profile_sha256=checked["plan"]["profile"]["profile_sha256"])
    atomic(args.output, encode(config))
    return dict(private_config_written=True, plan_sha256=checked["plan_sha256"])


def verify(stage):
    data = read_regular(stage / "plan.json", 65536)[0]
    plan = json.loads(data)
    if plan["version"] != 1 or plan["host"] != "oldman-bird.local" or set(plan["files"]) != set(FILES):
        raise RuntimeError("unsupported deployment plan")
    if plan["deployment_tool_sha256"] != digest(read_regular(Path(__file__))[0]) or plan["manifest_sha256"] != digest(read_regular(SOURCE / "manifest.json")[0]):
        raise RuntimeError("deployment tool or fixed-host manifest drift")
    if plan["client_known_hosts_sha256"] != digest(read_regular(stage / "client_known_hosts")[0]):
        raise RuntimeError("client host-key pin drift")
    for name, record in plan["files"].items():
        if record["path"] != FILES[name] or digest(read_regular(stage / name)[0]) != record["sha256"]:
            raise RuntimeError("staged artifact drift")
    return dict(plan_sha256=digest(data), plan=plan)


def unit_properties(unit, user=False):
    observed = run(["systemctl", *(["--user"] if user else []), "show", unit, "--property=LoadState,InvocationID,ControlGroup,ActiveState,UnitFileState,Restart,MainPID,FragmentPath,DropInPaths"], check=False)
    values = dict(line.split("=", 1) for line in observed.stdout.decode().splitlines() if "=" in line)
    if observed.returncode and values.get("LoadState") != "not-found":
        raise RuntimeError("unit inspection failed")
    return values


def inspect_host():
    units = {unit: unit_properties(unit) for unit in UNITS}
    import shlex
    sources = [units["ollama.service"].get("FragmentPath", "")]
    sources += shlex.split(units["ollama.service"].get("DropInPaths", ""))
    return dict(host=run(["hostname", "--fqdn"]).stdout.decode().strip(),
                boot=Path("/proc/sys/kernel/random/boot_id").read_text().strip(),
                units=units, unit_sources={path: snapshot(path) for path in sources if path},
                files={path: snapshot(path) for path in FILES.values()},
                nft=digest(run(["nft", "--stateless", "list", "ruleset"]).stdout),
                ufw=digest(run(["ufw", "status", "verbose"]).stdout),
                sshd=digest(run(["sshd", "-T"]).stdout))


class HeldCgroup:
    """Capture before stopping. MainPID exit and path disappearance never pass."""
    def __init__(self, *, fixture_unit=None, fixture_observation_ms=None):
        if fixture_unit is not None and not re.fullmatch(r"playtime-backend-fixture-[a-f0-9]{32}\.service", fixture_unit):
            raise RuntimeError("invalid disposable fixture unit")
        self.unit = fixture_unit or "ollama.service"
        self.user = fixture_unit is not None
        self.observation_ms = fixture_observation_ms if self.user and fixture_observation_ms else 120000
        self.before = unit_properties(self.unit, self.user)
        path = self.before.get("ControlGroup", "")
        prefix = "/user.slice/" if self.user else "/system.slice/"
        if not path.startswith(prefix) or ".." in path or not self.before.get("InvocationID"):
            raise RuntimeError("cannot hold original Ollama identity")
        self.fd = os.open("/sys/fs/cgroup" + path, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
        self.events = os.open("cgroup.events", os.O_RDONLY | os.O_NONBLOCK | os.O_NOFOLLOW, dir_fd=self.fd)

    def prove(self):
        deadline = time.clock_gettime(time.CLOCK_BOOTTIME) + self.observation_ms / 1000
        try:
            while time.clock_gettime(time.CLOCK_BOOTTIME) < deadline:
                fresh = unit_properties(self.unit, self.user)
                if fresh.get("InvocationID") != self.before["InvocationID"]:
                    raise RuntimeError("replacement observed; preserve closed policy")
                try:
                    os.lseek(self.events, 0, os.SEEK_SET)
                    empty = "populated 0" in os.read(self.events, 4096).decode().splitlines()
                except OSError as error:
                    import errno
                    if error.errno != errno.ENODEV:
                        raise
                    # Original held kernfs object is dead, not a missing path.
                    empty = True
                if empty and fresh["ActiveState"] in ("inactive", "failed") and fresh["MainPID"] == "0":
                    return
                time.sleep(0.05)
            raise RuntimeError("termination remains uncertain; never reopen legacy access")
        finally:
            os.close(self.events)
            os.close(self.fd)


def require_admin(args):
    checked = verify(args.stage)
    if os.geteuid() != 0 or args.reviewed_plan != checked["plan_sha256"] or not args.legacy_work_finished:
        raise RuntimeError("requires root, exact reviewed plan digest and explicit finished legacy work")
    stage = args.stage.resolve()
    for parent in (stage, *stage.parents):
        info = parent.lstat()
        if not stat.S_ISDIR(info.st_mode) or info.st_uid != 0 or info.st_mode & 0o022:
            raise RuntimeError("activation requires a root-owned protected staging path")
    for path in stage.iterdir():
        _, info = read_regular(path)
        if info.st_uid != 0 or info.st_mode & 0o022:
            raise RuntimeError("activation requires root-owned reviewed artifacts")
    host = run(["hostname", "--fqdn"]).stdout.decode().strip()
    if host.split(".")[0] != "oldman-bird":
        raise RuntimeError("fixed dedicated host mismatch")
    # Existing deployment identity must be provisioned by the reviewed admin;
    # this tool never creates accounts, credentials, models or software packages.
    uid = int(run(["id", "-u", "playtime-bridge"]).stdout)
    if uid != checked["plan"]["uid"]:
        raise RuntimeError("dedicated UID mismatch")
    STATE.mkdir(mode=0o700, exist_ok=True)
    info = STATE.lstat()
    if not stat.S_ISDIR(info.st_mode) or info.st_uid != 0 or stat.S_IMODE(info.st_mode) != 0o700:
        raise RuntimeError("private deployment root policy mismatch")
    lock = os.open(STATE / ".lock", os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW | os.O_NONBLOCK, 0o600)
    info = os.fstat(lock)
    if not stat.S_ISREG(info.st_mode) or info.st_uid != 0 or stat.S_IMODE(info.st_mode) != 0o600 or info.st_nlink != 1 or info.st_size != 0:
        os.close(lock)
        raise RuntimeError("invalid stable maintenance fence")
    fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    return checked, lock


def activate(args):
    checked, lock = require_admin(args)
    try:
        if (STATE / "transaction.json").exists():
            saved = json.loads(read_regular(STATE / "transaction.json", 4 * 1024 * 1024)[0])
            if saved["plan_sha256"] == checked["plan_sha256"] and saved["phase"] == "managed-awaiting-live-gate":
                for path, installed in saved["installed"].items():
                    if snapshot(path) != installed:
                        raise RuntimeError("installed file drift")
                return dict(phase=saved["phase"], live_gate="outstanding")
            raise RuntimeError("partial deployment retained; inspect and reconcile, never reset")
        before = inspect_host()
        if run(["nft", "list", "table", "inet", "playtime_backend"], check=False).returncode == 0:
            raise RuntimeError("owned firewall table already exists; no adoption by name")
        for path in FILES.values():
            if before["files"][path] is not None:
                raise RuntimeError("owned destination already exists; review conflict before activation")
        # Host config is separately provisioned private root-owned material. It
        # contains credentials, so it is never copied into this public stage.
        config = snapshot("/etc/playtime-backend/host.json")
        if not config or config["uid"] != 0 or config["mode"] & 0o027:
            raise RuntimeError("private administrator host config is required")
        transaction = dict(plan_sha256=checked["plan_sha256"], phase="closing", before=before,
                           config_sha256=config["sha256"], installed={})
        atomic(STATE / "transaction.json", encode(transaction))
        # The dedicated UID is not serving yet; every legacy path is denied now.
        run(["nft", "--check", "-f", str(args.stage / "firewall.nft")])
        run(["nft", "-f", str(args.stage / "firewall.nft")])
        proof = HeldCgroup()
        run(["systemctl", "stop", "--no-block", "ollama.service"])
        proof.prove()
        transaction["phase"] = "closed"
        transaction["table_sha256"] = digest(run(["nft", "--stateless", "list", "table", "inet", "playtime_backend"]).stdout)
        atomic(STATE / "transaction.json", encode(transaction))
        gid = int(run(["id", "-g", "playtime-bridge"]).stdout)
        for name, path in FILES.items():
            mode = 0o755 if name.startswith("playtime-backend-") and not name.endswith(".service") else 0o644
            atomic(Path(path), read_regular(args.stage / name)[0], mode, 0, 0)
            transaction["installed"][path] = snapshot(path)
            atomic(STATE / "transaction.json", encode(transaction))
        run(["sshd", "-t"])
        effective = dict(line.split(" ", 1) for line in run(["sshd", "-T", "-C", "user=playtime-bridge,host=oldman-bird.local,addr=192.0.2.1"]).stdout.decode().splitlines() if " " in line)
        for key, expected in {"forcecommand": "/usr/libexec/playtime-backend-bridge", "disableforwarding": "yes", "permittty": "no", "permituserrc": "no", "permituserenvironment": "no", "passwordauthentication": "no", "kbdinteractiveauthentication": "no"}.items():
            if effective.get(key) != expected:
                raise RuntimeError("effective SSH restriction mismatch; leave policy closed")
        run(["systemctl", "daemon-reload"])
        # The table is already loaded. Mark its oneshot active without a second
        # nft insertion by starting it against an exact idempotent table script.
        run(["systemctl", "start", "playtime-backend-firewall.service"])
        for path, uid, group in [(Path("/var/lib/playtime-backend"),checked["plan"]["uid"],gid),
                                  (Path("/var/lib/playtime-backend/guard"),checked["plan"]["uid"],gid)]:
            path.mkdir(mode=0o700, exist_ok=False)
            os.chown(path, uid, group)
        run(["systemctl", "start", "--no-block", "playtime-backend-owner.service"])
        run(["systemctl", "start", "ollama.service"])
        run(["systemctl", "start", "playtime-backend-guard.service"])
        run(["systemctl", "reload", "ssh.service"])
        transaction["phase"] = "managed-awaiting-live-gate"
        atomic(STATE / "transaction.json", encode(transaction))
        return dict(phase=transaction["phase"], live_gate="outstanding")
    finally:
        os.close(lock)


def deactivate(args):
    checked, lock = require_admin(args)
    try:
        tx = json.loads(read_regular(STATE / "transaction.json", 4 * 1024 * 1024)[0])
        if tx["plan_sha256"] != checked["plan_sha256"]:
            raise RuntimeError("rollback plan mismatch")
        if tx["phase"] == "legacy-restored-journals-retained":
            return dict(phase=tx["phase"])
        for path, installed in tx["installed"].items():
            if snapshot(path) != installed:
                raise RuntimeError("installed file drift; leave admission closed")
        for path, original in tx["before"].get("unit_sources", {}).items():
            if snapshot(path) != original:
                raise RuntimeError("original unit configuration drift; leave admission closed")
        table = run(["nft", "--stateless", "list", "table", "inet", "playtime_backend"]).stdout
        if digest(table) != tx.get("table_sha256"):
            raise RuntimeError("owned firewall drift; never replace the host ruleset")
        # Stop guard invalidates every capability before removing enforcement.
        # Capture evidence first; an already missing cgroup is not accepted.
        proof = HeldCgroup()
        run(["systemctl", "stop", "--no-block", "playtime-backend-guard.service", "playtime-backend-owner.service"])
        proof.prove()
        tx["phase"] = "cleanup-proved"
        atomic(STATE / "transaction.json", encode(tx))
        for path in tx["installed"]:
            original = tx["before"]["files"][path]
            if original is None:
                Path(path).unlink()
            else:
                atomic(Path(path), base64.b64decode(original["bytes"]), original["mode"], original["uid"], original["gid"])
        run(["systemctl", "daemon-reload"])
        run(["sshd", "-t"])
        run(["systemctl", "reload", "ssh.service"])
        run(["systemctl", "stop", "playtime-backend-firewall.service"])
        run(["nft", "delete", "table", "inet", "playtime_backend"])
        if tx["before"]["units"]["ollama.service"]["ActiveState"] == "active":
            run(["systemctl", "start", "ollama.service"])
        tx["phase"] = "legacy-restored-journals-retained"
        atomic(STATE / "transaction.json", encode(tx))
        return dict(phase=tx["phase"])
    finally:
        os.close(lock)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    prep = sub.add_parser("prepare")
    prep.add_argument("--stage", type=Path, required=True)
    prep.add_argument("--public-key", type=Path, required=True)
    prep.add_argument("--guard-uid", type=int, required=True)
    prep.add_argument("--bin-dir", type=Path, default=REPO / "target/release")
    prep.add_argument("--model-lock", type=Path, required=True)
    check = sub.add_parser("inspect")
    check.add_argument("--stage", type=Path, required=True)
    private = sub.add_parser("prepare-private")
    private.add_argument("--stage", type=Path, required=True)
    private.add_argument("--output", type=Path, required=True)
    for name in ("activate", "deactivate"):
        cmd = sub.add_parser(name)
        cmd.add_argument("--stage", type=Path, required=True)
        cmd.add_argument("--reviewed-plan", required=True)
        cmd.add_argument("--legacy-work-finished", action="store_true")
    args = parser.parse_args()
    try:
        result = {"prepare": prepare, "prepare-private": prepare_private, "inspect": lambda a: verify(a.stage), "activate": activate, "deactivate": deactivate}[args.command](args)
        print(json.dumps(result, indent=2))
    except (OSError, ValueError, KeyError, RuntimeError, subprocess.SubprocessError) as error:
        print(f"deployment remains unconfirmed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
