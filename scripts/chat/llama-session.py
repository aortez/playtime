#!/usr/bin/env python3
"""Own a local llama-server for one launcher session: start it, report it, stop it.

The launcher runs this beside itself with `python3 -I`, so it imports only the
standard library. It never builds the engine or fetches a model: a profile's
`llama` entry in presets.json names a reviewed runtime directory and model file.

    llama-session.py PROFILE --port PORT   start and hold, as the session owner
    llama-session.py --status PROFILE      read-only status as JSON
    llama-session.py --stop                stop servers whose owner is gone

The owner speaks the remote helpers' protocol. It reports its state directory on
stderr and `GYRO_READY {...}` on stdout, then holds the server while stdin brings
a heartbeat line at least every HEARTBEAT_TIMEOUT seconds. A release, EOF, a
missed heartbeat or a signal stops the server: a small local model loads quickly,
so it is never parked. The kernel stops the server if its owner dies, and the
verifier, not this owner, attests the process and its files before any handoff.
"""

import ctypes
import json
import os
from pathlib import Path
import select
import signal
import stat
import subprocess
import sys
import time
import urllib.request

PACKAGE = Path(__file__).resolve().parent
STATE = Path.home() / ".local/state/playtime-llama"
HEARTBEAT_TIMEOUT = 35
STOP_GRACE = 10
RELEASE = "GYRO_RELEASE"
PR_SET_PDEATHSIG = 1


def log(message):
    print(f"llama-server: {message}", file=sys.stderr, flush=True)


def preset_for(profile):
    preset = json.loads((PACKAGE / "presets.json").read_text())["profiles"].get(profile)
    config = preset.get("llama") if isinstance(preset, dict) else None
    if (not isinstance(config, dict) or set(config) != {"runtime", "model", "alias", "context", "threads"}
            or not all(isinstance(config[key], str) and os.path.isabs(config[key]) for key in ("runtime", "model"))
            or not isinstance(config["alias"], str) or not config["alias"]
            or not all(type(config[key]) is int and config[key] > 0 for key in ("context", "threads"))):
        raise RuntimeError(f"{profile} is not a local llama-server profile")
    return preset


def launch(config, port):
    """The whole configuration of a launch, which runs with an empty environment.

    llama-server reads LLAMA_ARG_* defaults, a pinned media marker and even its
    health route from the environment, so none of that is inherited.
    """
    return [str(Path(config["runtime"]) / "bin/llama-server"), "-m", config["model"],
            "--alias", config["alias"], "--host", "127.0.0.1", "--port", str(port),
            "-np", "1", "-c", str(config["context"]), "-t", str(config["threads"]),
            "-fa", "on", "--jinja", "--no-context-shift", "--reasoning-format", "deepseek",
            "--offline", "--load-mode", "none", "--no-ui"]


def stat_fields(pid):
    """/proc/PID/stat's fields after the command name, from the state onward."""
    return (Path("/proc") / str(pid) / "stat").read_text().rsplit(")", 1)[1].split()


def server_state(record):
    """None once the recorded server has exited, else "owned" or "orphaned"."""
    try:
        proc = Path("/proc") / str(record["pid"])
        argv = [part.decode() for part in (proc / "cmdline").read_bytes().split(b"\0") if part]
        fields = stat_fields(record["pid"])
        if fields[19] != record["start_ticks"] or argv != record["argv"]:
            return None
        # An owner's exit reparents its server.
        return "owned" if int(fields[1]) == record["owner_pid"] else "orphaned"
    except (OSError, IndexError, KeyError, TypeError, ValueError, UnicodeDecodeError):
        return None


def records():
    """Every readable session record with its directory, oldest first."""
    found = []
    for path in sorted(STATE.glob("*/*/llama-session.json")) if STATE.is_dir() else []:
        try:
            found.append((path.parent, json.loads(path.read_text())))
        except (OSError, ValueError):
            continue
    return found


def status(profile):
    """Busy while any recorded server runs, since every profile shares this CPU."""
    preset_for(profile)
    for directory, record in records():
        state = server_state(record)
        if state:
            holder = f"owner pid {record['owner_pid']}" if state == "owned" else "no owner; --stop-server stops it"
            return {"state": "busy", "message": f"llama-server for {record['profile']} is running "
                    f"(pid {record['pid']}, {holder}); logs: {directory}"}
    return {"state": "starts_on_launch"}


def stop_orphans():
    """Stop recorded servers whose owner is gone; a live owner stops its own."""
    result = 0
    for directory, record in records():
        try:
            descriptor = os.pidfd_open(record["pid"])
        except (OSError, KeyError, TypeError):
            continue
        try:
            # The descriptor pins this process, so a reused pid is never signalled.
            if server_state(record) != "orphaned":
                continue
            log(f"stopping pid {record['pid']}; logs: {directory}")
            for sig in (signal.SIGTERM, signal.SIGKILL):
                try:
                    signal.pidfd_send_signal(descriptor, sig)
                except ProcessLookupError:
                    break
                if select.select([descriptor], [], [], STOP_GRACE)[0]:
                    break
            else:
                log(f"pid {record['pid']} did not exit")
                result = 1
        finally:
            os.close(descriptor)
    return result


def private_directory(path):
    path.mkdir(mode=0o700, parents=True, exist_ok=True)
    info = path.lstat()
    if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
        raise RuntimeError(f"state must be a private, caller-owned directory: {path}")
    return path


def get_json(port, path):
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    with opener.open(f"http://127.0.0.1:{port}{path}", timeout=5) as response:
        return json.loads(response.read(4 << 20))


class Owner:
    def __init__(self, profile, port):
        self.profile, self.port = profile, port
        self.preset = preset_for(profile)
        self.config = self.preset["llama"]
        self.child = self.directory = None
        self.signals = []
        self.pending = b""

    def interrupted(self, signum, _frame):
        self.signals.append(signum)

    def line(self, timeout):
        """The next stdin line within timeout: None on timeout, "" at EOF."""
        deadline = time.monotonic() + timeout
        while b"\n" not in self.pending:
            remaining = deadline - time.monotonic()
            if remaining <= 0 or not select.select([0], [], [], remaining)[0]:
                return None
            data = os.read(0, 4096)
            if not data or len(self.pending) > 4096:
                return ""
            self.pending += data
        line, self.pending = self.pending.split(b"\n", 1)
        return line.decode(errors="replace") + "\n"

    def check(self):
        if self.signals:
            raise InterruptedError(f"received signal {self.signals[0]}")
        if self.child.poll() is not None:
            raise RuntimeError(f"llama-server exited with {self.child.returncode}; see {self.directory}")

    def start(self):
        busy = status(self.profile)
        if busy["state"] != "starts_on_launch":
            raise RuntimeError(busy["message"])
        parent = private_directory(STATE / self.profile)
        self.directory = parent / f"{time.strftime('%Y%m%dT%H%M%SZ', time.gmtime())}-{os.getpid()}"
        self.directory.mkdir(mode=0o700)
        log(f"starting {self.profile}; logs: {self.directory}")
        argv = launch(self.config, self.port)
        owner = os.getpid()
        prctl = ctypes.CDLL(None, use_errno=True).prctl

        def die_with_owner():
            # An owner that died before this call is no longer the parent.
            if prctl(PR_SET_PDEATHSIG, signal.SIGTERM, 0, 0, 0) != 0 or os.getppid() != owner:
                os._exit(127)

        with (self.directory / "server.log").open("xb") as output:
            self.child = subprocess.Popen(argv, env={}, cwd=self.config["runtime"], stdin=subprocess.DEVNULL,
                                          stdout=output, stderr=subprocess.STDOUT, start_new_session=True,
                                          preexec_fn=die_with_owner)
        record = {"schema_version": 1, "profile": self.profile, "owner_pid": owner, "pid": self.child.pid,
                  "start_ticks": stat_fields(self.child.pid)[19], "port": self.port, "argv": argv,
                  "environment": {}, "runtime": self.config["runtime"], "model": self.config["model"]}
        with (self.directory / "llama-session.json").open("x") as output:
            json.dump(record, output, indent=2)
            output.write("\n")
            output.flush()
            os.fsync(output.fileno())

    def ready(self):
        """The served model, context and slot count once llama-server is healthy."""
        deadline = time.monotonic() + self.preset["startup_timeout"]
        while True:
            self.check()
            try:
                if get_json(self.port, "/health").get("status") == "ok":
                    props = get_json(self.port, "/props")
                    return {"model": props.get("model_alias"), "parallel": props.get("total_slots"),
                            "context": props.get("default_generation_settings", {}).get("n_ctx")}
            except (OSError, ValueError, AttributeError):
                pass
            if time.monotonic() >= deadline:
                raise RuntimeError("llama-server did not become ready in time")
            if self.line(.2) == "":
                raise RuntimeError("the launcher closed the session during startup")

    def hold(self):
        """Serve until a release or EOF, and say which; anything else raises."""
        heard = time.monotonic()
        while True:
            self.check()
            line = self.line(1)
            if line is None:
                if time.monotonic() - heard > HEARTBEAT_TIMEOUT:
                    raise RuntimeError("heartbeats stopped")
            elif line == "":
                return "the launcher closed the session"
            elif line.strip() == RELEASE:
                return "released"
            elif line.strip() == ".":
                heard = time.monotonic()
            else:
                raise RuntimeError("unexpected launcher input")

    def stop(self):
        """Stop and reap the server; whether it has exited."""
        if self.child is None:
            return True
        for stop in (self.child.terminate, self.child.kill):
            if self.child.poll() is None:
                stop()
            try:
                self.child.wait(timeout=STOP_GRACE)
                return True
            except subprocess.TimeoutExpired:
                continue
        return False


def own(profile, port):
    owner = Owner(profile, port)
    for sig in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
        signal.signal(sig, owner.interrupted)
    result = 1
    try:
        owner.start()
        print("GYRO_READY " + json.dumps(owner.ready()), flush=True)
        log(f"stopping {profile}: {owner.hold()}")
        result = 0
    except Exception as error:
        log(f"stopping {profile}: {error}")
    finally:
        stopped = owner.stop()
        if owner.child is not None:
            log(f"llama-server exited with {owner.child.returncode}" if stopped else "llama-server exit is unconfirmed")
    return result if stopped else 1


def main(args):
    try:
        if len(args) == 2 and args[0] == "--status":
            print(json.dumps(status(args[1])))
            return 0
        if args == ["--stop"]:
            return stop_orphans()
        if len(args) == 3 and args[1] == "--port" and args[2].isdigit() and 0 < int(args[2]) < 65536:
            return own(args[0], int(args[2]))
    except (OSError, RuntimeError, ValueError) as error:
        log(str(error))
        return 1
    log("usage: llama-session.py PROFILE --port PORT | --status PROFILE | --stop")
    return 2


if __name__ == "__main__":
    os.umask(0o077)
    sys.exit(main(sys.argv[1:]))
