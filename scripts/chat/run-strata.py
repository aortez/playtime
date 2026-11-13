#!/usr/bin/env python3
"""Own a reviewed remote Strata session, verify it, and run installed Playtime chat.

Configuration and locks sit next to this program, outside the selected project.
Status checks only invoke the remote helper's read-only status command.
"""

import json
import os
from pathlib import Path
import queue
import re
import shlex
import signal
import socket
import stat
import subprocess
import sys
import tempfile
import threading
import time

PACKAGE = Path(__file__).resolve().parent
STATUS_TIMEOUT = 8
VERIFY_TIMEOUT = 600
CLIENT_CLEANUP_TIMEOUT = 45


def log(message):
    print(f"Playtime: {message}", file=sys.stderr, flush=True)


def ssh(preset):
    return ["ssh", *[os.path.expanduser(arg) for arg in preset["ssh_options"]], "-T",
            "-o", "BatchMode=yes", "-o", "StrictHostKeyChecking=yes", "-o", "ConnectTimeout=5",
            "-o", "ServerAliveInterval=15", "-o", "ServerAliveCountMax=3"]


def remote(preset, *args):
    return [preset["host"], shlex.join(args)]


def status(preset, profile):
    try:
        result = subprocess.run(ssh(preset) + remote(preset, "python3", preset["helper"], "--status", profile),
                                stdin=subprocess.DEVNULL, capture_output=True, timeout=STATUS_TIMEOUT)
        if result.returncode:
            report = {"state": "down" if result.returncode == 255 else "unknown",
                      "message": "remote helper status failed"}
        else:
            report = json.loads(result.stdout)
            if not isinstance(report, dict) or "state" not in report:
                raise ValueError("invalid helper status")
    except (OSError, ValueError, subprocess.TimeoutExpired) as error:
        report = {"state": "unknown", "message": str(error)}
    print(json.dumps(report))
    return 0


def private_run_directory():
    root = Path.home() / ".local/state/playtime-launcher"
    root.mkdir(mode=0o700, parents=True, exist_ok=True)
    meta = root.lstat()
    if not stat.S_ISDIR(meta.st_mode) or meta.st_uid != os.getuid() or meta.st_mode & 0o077:
        raise RuntimeError(f"launcher state must be a private, caller-owned directory: {root}")
    return Path(tempfile.mkdtemp(prefix=time.strftime("%Y%m%dT%H%M%S-") , dir=root))


def stop_process(process, grace):
    if process is not None and process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=grace)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=5)


def check_ready(ready, lock):
    if (ready.get("model") != lock["identity"]["model"]
            or ready.get("context") != lock["identity"]["context_tokens"]
            or ready.get("drafting") != "dflash2"):
        raise RuntimeError("helper returned a different model, context or drafting preset")


def check_host(report, lock):
    if report.get("host") != lock["host"]:
        raise RuntimeError("running host artifacts/settings differ from the reviewed model lock; client not started")


def session(preset, profile, args):
    for arg in args:
        if arg.split("=", 1)[0] in ("--endpoint", "--model-lock", "--image-lock"):
            raise RuntimeError(f"{arg.split('=', 1)[0]} is owned by this preset")
    executable = PACKAGE / "playtime-chat"
    lock_path = PACKAGE / preset["model_lock"]
    image_path = PACKAGE / "general.lock.json"
    # Read local resources before touching the remote host.
    lock = json.loads(lock_path.read_text())
    json.loads(image_path.read_text())
    if not executable.is_file() or not os.access(executable, os.X_OK):
        raise RuntimeError("playtime-chat is not installed beside this wrapper")
    if args in (["--help"], ["-h"], ["--version"], ["-V"]):
        return subprocess.call([str(executable), *args])
    verifier = (PACKAGE / "verify-strata-host.py").read_bytes()
    evidence = private_run_directory()
    log(f"session evidence: {evidence}")
    with socket.socket() as reservation:
        reservation.bind(("127.0.0.1", 0))
        port = reservation.getsockname()[1]
    command = ssh(preset) + ["-o", "ExitOnForwardFailure=yes", "-L", f"127.0.0.1:{port}:127.0.0.1:8080"]
    command += remote(preset, "python3", preset["helper"], profile)
    owner = child = verification = None
    events = queue.Queue(maxsize=256)
    stopped = threading.Event()
    failed = threading.Event()
    signals = []
    previous = {}
    result = 1
    owner_code = None

    def receive(stream, channel):
        try:
            while line := stream.readline(16385):
                if len(line) > 16384:
                    raise RuntimeError("oversized helper output")
                events.put_nowait((channel, line))
        except (OSError, ValueError, RuntimeError, queue.Full):
            failed.set()

    def heartbeat():
        try:
            while not stopped.is_set():
                owner.stdin.write(".\n")
                owner.stdin.flush()
                stopped.wait(10)
        except (OSError, ValueError):
            failed.set()

    def interrupted(signum, _frame):
        signals.append(signum)

    def healthy():
        if signals:
            raise InterruptedError(f"received signal {signals[0]}")
        if failed.is_set() or owner.poll() is not None:
            raise RuntimeError("lost the inference host owner")

    def drain():
        nonlocal ready, state_dir
        while True:
            try:
                channel, line = events.get_nowait()
            except queue.Empty:
                return
            transcript.write(f"{channel}: {line}")
            transcript.flush()
            if channel == "stderr":
                print(line, end="", file=sys.stderr, flush=True)
                prefix = f"Gyro-M: starting {profile}; logs: "
                if line.startswith(prefix):
                    state_dir = line[len(prefix):].strip()
                    path = Path(state_dir)
                    if not path.is_absolute() or not re.fullmatch(r"\d{8}T\d{6}Z-\d+", path.name):
                        raise RuntimeError("unrecognized helper session path")
                    if not str(path.parent).endswith("/" + preset["state_parent"]):
                        raise RuntimeError("unexpected helper state directory")
            elif line.startswith("GYRO_READY "):
                if ready is not None:
                    raise RuntimeError("duplicate helper readiness report")
                ready = json.loads(line[len("GYRO_READY "):])

    ready = state_dir = None
    transcript = (evidence / "owner.log").open("x")
    try:
        for sig in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
            previous[sig] = signal.signal(sig, interrupted)
        owner = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                 stderr=subprocess.PIPE, text=True, start_new_session=True)
        for channel in ("stdout", "stderr"):
            threading.Thread(target=receive, args=(getattr(owner, channel), channel), daemon=True).start()
        threading.Thread(target=heartbeat, daemon=True).start()
        deadline = time.monotonic() + preset["startup_timeout"]
        while ready is None or state_dir is None:
            drain()
            healthy()
            if time.monotonic() >= deadline:
                raise TimeoutError("owned model startup deadline exceeded")
            time.sleep(.1)
        check_ready(ready, lock)
        log("verifying running engine and model artifacts against the tested lock…")
        start = time.monotonic()
        with (evidence / "verify-input.py").open("xb") as source:
            source.write(verifier)
        with (evidence / "verify-input.py").open("rb") as source, (evidence / "host.json").open("xb") as report, (evidence / "verify.log").open("xb") as errors:
            verification = subprocess.Popen(ssh(preset) + remote(preset, "python3", "-", state_dir),
                                            stdin=source, stdout=report, stderr=errors, start_new_session=True)
            while verification.poll() is None:
                drain()
                healthy()
                if time.monotonic() - start > VERIFY_TIMEOUT:
                    raise TimeoutError("host verification deadline exceeded")
                time.sleep(.1)
            if verification.returncode:
                raise RuntimeError(f"host verification failed; see {evidence / 'verify.log'}")
        check_host(json.loads((evidence / "host.json").read_text()), lock)
        healthy()
        log(f"verified in {time.monotonic() - start:.1f}s; starting {profile} chat in {Path.cwd()}")
        child = subprocess.Popen([str(executable), "--endpoint", f"http://127.0.0.1:{port}/v1",
                                  "--model-lock", str(lock_path), "--image-lock", str(image_path), *args])
        while child.poll() is None:
            drain()
            healthy()
            time.sleep(.1)
        result = child.returncode if child.returncode >= 0 else 128 - child.returncode
    except InterruptedError:
        result = 128 + signals[0]
        log("stopping chat and restoring the inference host…")
    except Exception as error:
        log(str(error))
    finally:
        # Keep the owner heartbeats alive until local tool cleanup has had its
        # full grace period. EOF then asks the authoritative helper to restore.
        for sig in previous:
            signal.signal(sig, signal.SIG_IGN)
        for process, grace in ((verification, 5), (child, CLIENT_CLEANUP_TIMEOUT)):
            try:
                stop_process(process, grace)
            except (OSError, subprocess.TimeoutExpired) as error:
                log(f"local child cleanup was not confirmed: {error}")
                result = 1
        stopped.set()
        if owner is not None:
            try:
                owner.stdin.close()
            except (OSError, ValueError):
                pass
            deadline = time.monotonic() + preset["cleanup_timeout"]
            while owner.poll() is None and time.monotonic() < deadline:
                try:
                    drain()
                except Exception:
                    pass
                time.sleep(.1)
            if owner.poll() is None:
                log("host restoration was not confirmed; inspect the helper logs before another launch")
                try:
                    stop_process(owner, 5)
                except (OSError, subprocess.TimeoutExpired) as error:
                    log(f"SSH cleanup was not confirmed: {error}")
                result = 1
            owner_code = owner.returncode
            if owner_code != 0:
                result = result or 1
            try:
                drain()
            except Exception:
                pass
        transcript.close()
        (evidence / "exit.json").write_text(json.dumps({"client_exit": None if child is None else child.returncode,
            "owner_exit": owner_code, "exit": result}) + "\n")
        for sig, handler in previous.items():
            signal.signal(sig, handler)
    return result


def main(args):
    report = args[:1] == ["--status"]
    if report:
        args = args[1:]
    config = json.loads((PACKAGE / "presets.json").read_text())
    if config.get("schema_version") != 1:
        raise RuntimeError("unsupported preset configuration")
    if not args or args[0] not in config["profiles"]:
        raise RuntimeError("use run-strata.py [--status] " + "|".join(config["profiles"]) + " [CHAT ARGS]")
    profile, extra = args[0], args[1:]
    if report and extra:
        raise RuntimeError("status accepts only a profile")
    preset = config["profiles"][profile]
    return status(preset, profile) if report else session(preset, profile, extra)


if __name__ == "__main__":
    os.umask(0o077)
    try:
        sys.exit(main(sys.argv[1:]))
    except Exception as error:
        log(str(error))
        sys.exit(1)
