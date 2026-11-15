#!/usr/bin/python3 -IS
"""Fixed privileged entry point; only an installed root-owned policy grants access.

The controller keeps stdin open after its single request. EOF is cancellation,
including controller death. No project code runs before setpriv and Bubblewrap.
"""
import hashlib
import json
import math
import os
from pathlib import Path
import selectors
import signal
import stat
import subprocess
import sys
import time

MAX_REQUEST = 262144
CAPABILITY = "project-native-v1"
STOP_REQUESTED = False
POLICY_FIELDS = {"schema_version", "controller_uid", "agent_uid", "agent_gid", "workspace",
                 "cache", "toolchain", "helper", "mode", "limits"}
LIMIT_FIELDS = {"command_seconds", "memory_bytes", "processes", "output_bytes"}
# Runs only inside the sandbox, with an isolated interpreter and JSON data on stdin.
FILE_TOOL = r'''
import json, os, pathlib, sys
r = json.load(sys.stdin)
a = r["arguments"]
p = pathlib.Path(a["path"])
try:
    if r["tool"] == "read_file":
        offset = a.get("offset_bytes", 0)
        with p.open("rb") as f:
            f.seek(offset)
            data = f.read(r["output_bytes"] + 1)
        result = {"content":data[:r["output_bytes"]].decode("utf-8"), "offset_bytes":offset,
                  "truncated":len(data) > r["output_bytes"]}
    else:
        data = a["content"].encode("utf-8")
        with p.open("ab" if a.get("append", False) else "wb") as f:
            f.write(data)
        result = {"path":str(p), "bytes_written":len(data)}
except (OSError, UnicodeError, ValueError) as e:
    result = {"error":str(e)[:2048]}
print(json.dumps(result))
'''


def install_signals():
    def stop(_number, _frame):
        global STOP_REQUESTED
        STOP_REQUESTED = True
    for number in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
        signal.signal(number, stop)


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError("duplicate JSON field")
        result[key] = value
    return result


def decode(data):
    return json.loads(data, object_pairs_hook=unique_object)


def trusted_path(path, directory=False):
    path = Path(path)
    if not path.is_absolute() or path != path.resolve(strict=True):
        raise ValueError("trusted path must be absolute and contain no links")
    for entry in (path, *path.parents):
        info = entry.lstat()
        if info.st_uid != 0 or info.st_mode & 0o022:
            raise ValueError("policy/runtime/toolchain paths must be root-owned and protected")
    info = path.stat()
    if not (stat.S_ISDIR(info.st_mode) if directory else stat.S_ISREG(info.st_mode)):
        raise ValueError("invalid trusted path type")
    return path


def validate_policy(policy):
    if set(policy) != POLICY_FIELDS or policy["schema_version"] != 1:
        raise ValueError("unsupported native policy")
    for field in ("controller_uid", "agent_uid", "agent_gid"):
        if type(policy[field]) is not int or not 0 < policy[field] < 2**32 - 1:
            raise ValueError("invalid native identity")
    if policy["controller_uid"] == policy["agent_uid"]:
        raise ValueError("native tools require a distinct dedicated account")
    if policy["mode"] not in ("edit", "read_only"):
        raise ValueError("unsupported native role")
    limits = policy["limits"]
    if set(limits) != LIMIT_FIELDS or any(type(v) is not int for v in limits.values()):
        raise ValueError("explicit native limits required")
    if not (1 <= limits["command_seconds"] <= 3600 and 1024**3 <= limits["memory_bytes"] <= 128 * 1024**3
            and 16 <= limits["processes"] <= 4096 and 1024 <= limits["output_bytes"] <= 65536):
        raise ValueError("native limits outside supported bounds")
    paths = [Path(policy[key]) for key in ("workspace", "cache", "toolchain")]
    for path in paths:
        if not path.is_absolute() or path != path.resolve(strict=True) or not path.is_dir() or path == Path("/"):
            raise ValueError("native roots must be canonical directories")
    if any(a.is_relative_to(b) for i, a in enumerate(paths) for j, b in enumerate(paths) if i != j):
        raise ValueError("native roots must not overlap")
    git = paths[0] / ".git"
    if not git.is_dir() or git.resolve(strict=True) != git:
        raise ValueError("native workspace must have a standalone .git directory")
    if any(paths[i].stat().st_mode & 0o002 for i in (0, 1)):
        raise ValueError("native writable roots cannot be world-writable")
    if paths[1].stat().st_uid != policy["agent_uid"]:
        raise ValueError("native cache must belong to agent")
    for subdirectory in ("home", "cargo", "tmp"):
        path = paths[1] / subdirectory
        if path.resolve(strict=True) != path or not path.is_dir() or path.stat().st_uid != policy["agent_uid"]:
            raise ValueError("cache home/cargo/tmp must be pre-provisioned agent directories")


def load_policy(path):
    path = trusted_path(path)
    with path.open("rb") as source:
        data = source.read(65537)
    if len(data) > 65536:
        raise ValueError("native policy too large")
    policy = decode(data)
    validate_policy(policy)
    trusted_path(policy["helper"])
    trusted_path(policy["toolchain"], directory=True)
    if Path(policy["helper"]) != Path(__file__).resolve():
        raise ValueError("policy names another helper installation")
    if os.geteuid() != 0 or os.environ.get("SUDO_UID") != str(policy["controller_uid"]):
        raise ValueError("native helper requires the policy's trusted sudo caller")
    return policy, hashlib.sha256(data).hexdigest()


def validate_request(request, policy):
    if set(request) != {"schema_version", "policy_sha256", "helper_sha256", "workspace_identity", "cache_identity", "git_identity", "tool", "arguments", "timeout_ms", "output_bytes"} or request["schema_version"] != 1:
        raise ValueError("unsupported native request")
    if type(request["timeout_ms"]) is not int or not 1 <= request["timeout_ms"] <= policy["limits"]["command_seconds"] * 1000:
        raise ValueError("invalid tool deadline")
    if type(request["output_bytes"]) is not int or not 1 <= request["output_bytes"] <= policy["limits"]["output_bytes"]:
        raise ValueError("invalid output bound")
    tool, args = request["tool"], request["arguments"]
    fields = {"exec": ({"command"}, {"cwd", "timeout_seconds"}),
              "read_file": ({"path"}, {"offset_bytes"}),
              "write_file": ({"path", "content"}, {"append"})}
    if tool not in fields or not isinstance(args, dict):
        raise ValueError("unsupported native tool")
    required, optional = fields[tool]
    if not required <= set(args) <= required | optional:
        raise ValueError("invalid native tool fields")
    for field in required | ({"cwd"} if "cwd" in args else set()):
        if not isinstance(args[field], str) or "\0" in args[field]:
            raise ValueError("invalid native tool text")
    if "offset_bytes" in args and (type(args["offset_bytes"]) is not int or args["offset_bytes"] < 0):
        raise ValueError("invalid read offset")
    if "append" in args and type(args["append"]) is not bool:
        raise ValueError("invalid append flag")
    if "timeout_seconds" in args and (type(args["timeout_seconds"]) not in (int, float) or not math.isfinite(args["timeout_seconds"]) or args["timeout_seconds"] <= 0):
        raise ValueError("invalid command timeout")
    if policy["mode"] == "read_only" and tool == "write_file":
        raise ValueError("write_file denied by read-only policy")


def sandbox_args(policy, request, root_fds):
    args = ["/usr/bin/bwrap", "--unshare-all", "--unshare-user", "--share-net", "--disable-userns",
            "--die-with-parent", "--new-session", "--cap-drop", "ALL", "--clearenv",
            "--ro-bind", "/usr", "/usr", "--tmpfs", "/usr/local", "--ro-bind", "/lib", "/lib",
            "--ro-bind", "/lib64", "/lib64", "--symlink", "usr/bin", "/bin",
            "--symlink", "usr/sbin", "/sbin", "--proc", "/proc", "--dev", "/dev",
            "--size", str(512 * 1024**2), "--tmpfs", "/tmp", "--dir", "/etc"]
    for path in ("/etc/alternatives", "/etc/ld.so.cache", "/etc/resolv.conf", "/etc/hosts", "/etc/nsswitch.conf", "/etc/ssl/certs"):
        if Path(path).exists():
            args += ["--ro-bind", path, path]
    binding = "--bind-fd" if policy["mode"] == "edit" else "--ro-bind-fd"
    for key, fd in zip(("workspace", "cache"), root_fds):
        args += [binding, str(fd), policy[key]]
    args += ["--ro-bind-fd", str(root_fds[2]), str(Path(policy["workspace"]) / ".git")]
    args += ["--ro-bind", policy["toolchain"], policy["toolchain"]]
    cache = Path(policy["cache"])
    environment = {"PATH":f'{policy["toolchain"]}/bin:/usr/bin:/bin', "HOME":str(cache / "home"),
                   "CARGO_HOME":str(cache / "cargo"), "TMPDIR":str(cache / "tmp") if policy["mode"] == "edit" else "/tmp",
                   "LANG":"C.UTF-8", "USER":"devbot-agent", "LOGNAME":"devbot-agent",
                   "GIT_CONFIG_SYSTEM":"/dev/null", "GIT_CONFIG_GLOBAL":"/dev/null",
                   "GIT_CONFIG_COUNT":"1", "GIT_CONFIG_KEY_0":"safe.directory", "GIT_CONFIG_VALUE_0":policy["workspace"]}
    for key, value in environment.items():
        args += ["--setenv", key, value]
    args += ["--chdir", request["arguments"].get("cwd", policy["workspace"]), "--"]
    if request["tool"] == "exec":
        args += ["/bin/bash", "--noprofile", "--norc", "-o", "pipefail", "-c", request["arguments"]["command"]]
    else:
        args += ["/usr/bin/python3", "-I", "-S", "-B", "-c", FILE_TOOL]
    return args


def run_tool(policy, request, control_fd, *, drop_identity=True):
    """One PID namespace per command; no background processes survive its exit."""
    validate_request(request, policy)
    cancelled = STOP_REQUESTED
    if control_fd is not None:
        with selectors.DefaultSelector() as selector:
            selector.register(control_fd, selectors.EVENT_READ)
            cancelled |= bool(selector.select(0))
    if cancelled:
        return {"schema_version":1, "cleanup":"complete", "result":{"error":"native tool cancelled"}}
    roots = [os.open(policy[key], os.O_PATH | os.O_DIRECTORY | os.O_NOFOLLOW) for key in ("workspace", "cache")]
    try:
        roots.append(os.open(".git", os.O_PATH | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=roots[0]))
        for field, fd in zip(("workspace_identity", "cache_identity", "git_identity"), roots):
            info = os.fstat(fd)
            if request[field] != {"device":info.st_dev, "inode":info.st_ino}:
                raise ValueError("native workspace/cache identity changed")
        args = sandbox_args(policy, request, roots)
        if drop_identity:
            limits = policy["limits"]
            args = ["/usr/bin/setpriv", "--reuid", str(policy["agent_uid"]), "--regid", str(policy["agent_gid"]),
                    "--clear-groups", "--bounding-set=-all", "--inh-caps=-all", "--ambient-caps=-all", "--no-new-privs",
                    "/usr/bin/prlimit", f'--as={limits["memory_bytes"]}', f'--nproc={limits["processes"]}', "--nofile=4096", "--", *args]
        process = subprocess.Popen(args, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                   env={}, cwd="/", pass_fds=roots, start_new_session=True)
    finally:
        for fd in roots:
            os.close(fd)
    started = time.monotonic()
    deadline = started + request["timeout_ms"] / 1000
    requested = request["arguments"].get("timeout_seconds")
    if requested is not None:
        deadline = min(deadline, started + requested)
    output = {"stdout":bytearray(), "stderr":bytearray()}
    truncated = False
    timed_out = cancelled = False
    pending = json.dumps(request).encode() if request["tool"] != "exec" else b""
    try:
        with selectors.DefaultSelector() as selector:
            if control_fd is not None:
                selector.register(control_fd, selectors.EVENT_READ, "control")
            for label in output:
                stream = getattr(process, label)
                os.set_blocking(stream.fileno(), False)
                selector.register(stream, selectors.EVENT_READ, label)
            if pending:
                os.set_blocking(process.stdin.fileno(), False)
                selector.register(process.stdin, selectors.EVENT_WRITE, "input")
            else:
                process.stdin.close()
            killed_at = None
            while process.poll() is None or any(key.data in output for key in selector.get_map().values()):
                now = time.monotonic()
                cancelled |= STOP_REQUESTED
                if not killed_at and (now >= deadline or cancelled):
                    timed_out = not cancelled
                    killed_at = now
                    try:
                        process.kill()
                    except ProcessLookupError:
                        pass
                if killed_at and now - killed_at > 5:
                    raise RuntimeError("native cleanup not confirmed within five seconds")
                for key, _ in selector.select(.02):
                    if key.data == "control":
                        # Any extra data is a protocol violation, EOF a lost owner.
                        os.read(control_fd, 1)
                        selector.unregister(control_fd)
                        cancelled = True
                    elif key.data == "input":
                        try:
                            written = os.write(process.stdin.fileno(), pending)
                            pending = pending[written:]
                        except BrokenPipeError:
                            pending = b""
                        if not pending:
                            selector.unregister(process.stdin)
                            process.stdin.close()
                    else:
                        data = os.read(key.fd, 8192)
                        if not data:
                            selector.unregister(key.fileobj)
                            key.fileobj.close()
                        else:
                            maximum = request["output_bytes"] if request["tool"] == "exec" else 6 * request["output_bytes"] + 8192
                            room = max(0, maximum - len(output[key.data]))
                            output[key.data] += data[:room]
                            truncated |= len(data) > room
        code = process.wait(timeout=5)
    finally:
        if process.poll() is None:
            process.kill()
            process.wait(timeout=5)
        for stream in (process.stdin, process.stdout, process.stderr):
            stream.close()
    if cancelled:
        result = {"error":"native tool cancelled"}
    elif request["tool"] == "exec":
        result = {"stdout":output["stdout"].decode("utf-8", "replace"), "stderr":output["stderr"].decode("utf-8", "replace"),
                  "exit_code":code, "timed_out":timed_out, "output_truncated":truncated,
                  "duration_seconds":time.monotonic() - started}
    elif timed_out or code != 0 or truncated:
        result = {"error":"native file tool failed or exceeded its bounds"}
    else:
        result = decode(output["stdout"])
    return {"schema_version":1, "cleanup":"complete", "result":result}


def read_request(fd):
    data = bytearray()
    deadline = time.monotonic() + 5
    with selectors.DefaultSelector() as selector:
        selector.register(fd, selectors.EVENT_READ)
        while time.monotonic() < deadline:
            if not selector.select(max(0, deadline - time.monotonic())):
                break
            part = os.read(fd, 4096)
            if not part:
                raise ValueError("controller closed before request")
            data += part
            if len(data) > MAX_REQUEST:
                raise ValueError("native request exceeds limit")
            if b"\n" in data:
                if not data.endswith(b"\n") or data.count(b"\n") != 1:
                    raise ValueError("one request per helper required")
                return decode(data)
    raise ValueError("native request deadline exceeded")


def main():
    if len(sys.argv) != 3 or sys.argv[1] != "--policy":
        raise ValueError("expected --policy /root-owned/policy.json")
    install_signals()
    policy, digest = load_policy(sys.argv[2])
    request = read_request(0)
    if request["policy_sha256"] != digest:
        raise ValueError("native policy changed")
    with Path(__file__).open("rb") as source:
        helper = source.read(MAX_REQUEST + 1)
    if len(helper) > MAX_REQUEST or request["helper_sha256"] != hashlib.sha256(helper).hexdigest():
        raise ValueError("native helper changed")
    print(json.dumps(run_tool(policy, request, 0)), flush=True)


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, RuntimeError, KeyError, subprocess.TimeoutExpired) as error:
        print(f"native helper refused or failed: {error}", file=sys.stderr)
        sys.exit(1)
