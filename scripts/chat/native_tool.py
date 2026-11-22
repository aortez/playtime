#!/usr/bin/python3 -IS
"""Fixed privileged entry point; only an installed root-owned policy grants access.

The controller keeps stdin open after its single request. EOF is cancellation,
including controller death. No project code runs before setpriv and Bubblewrap.
A refusal before dispatch is a complete receipt; a nonzero exit means the helper
cannot vouch that no tool process remains.
"""
import ctypes
import fcntl
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
PR_SET_CHILD_SUBREAPER = 36
TMP_BYTES = 512 * 1024**2
SHM_BYTES = 64 * 1024**2
NOLOGIN_SHELLS = {"/usr/sbin/nologin", "/sbin/nologin", "/usr/bin/false", "/bin/false"}
SLIRP4NETNS = "/usr/bin/slirp4netns"
NS_GET_USERNS = 0xB701
# slirp4netns's fixed addresses: the sandbox is 10.0.2.100, the gateway 10.0.2.2
# and the DNS forwarder 10.0.2.3.
NETWORK_DNS = b"nameserver 10.0.2.3\n"
POLICY_FIELDS = {"schema_version", "controller_uid", "agent_uid", "agent_gid", "workspace",
                 "cache", "toolchain", "helper", "mode", "limits"}
LIMIT_FIELDS = {"command_seconds", "memory_bytes", "processes", "output_bytes"}
# Runs only inside the sandbox, with an isolated interpreter and JSON data on stdin.
# Its text checks match the Docker bridge's (workshop/tool.py). Relative paths use
# the sandbox working directory, which is the checkout.
FILE_TOOL = r'''
import codecs, json, pathlib, stat, sys
r = json.load(sys.stdin)
a = r["arguments"]
limit = r["output_bytes"]
p = pathlib.Path(a["path"])
def text(data, mid_file=False, final=False):
    if mid_file:
        skipped = 0
        while skipped < min(3, len(data)) and 0x80 <= data[skipped] <= 0xBF:
            skipped += 1
        data = data[skipped:]
    if any((b < 32 and b not in b"\t\n\r\f") or b == 127 for b in data):
        return False
    try:
        # A bounded sample may end inside a character; at EOF that is invalid.
        codecs.getincrementaldecoder("utf-8")().decode(data, final=final)
    except UnicodeDecodeError:
        return False
    return True
try:
    if r["tool"] == "read_file":
        info = p.stat()
        if not stat.S_ISREG(info.st_mode):
            raise ValueError("read_file requires a regular file")
        offset = a.get("offset_bytes", 0)
        with p.open("rb") as f:
            prefix = f.read(4096)
            start = max(0, offset - 3)
            f.seek(start)
            sample = f.read(offset - start + limit + 1)
        if not text(prefix, final=len(prefix) >= info.st_size) or not text(
                sample, start > 0, start + len(sample) >= info.st_size):
            result = {"error":"read_file only supports UTF-8 text; this file appears binary or is not UTF-8. "
                      "Use exec to inspect metadata or compute a text summary. This interface cannot view images.",
                      "size_bytes":info.st_size}
        else:
            data = sample[offset - start:]
            result = {"content":data[:limit].decode("utf-8", errors="replace"), "offset_bytes":offset,
                      "truncated":len(data) > limit}
    else:
        data = a["content"].encode("utf-8")
        p.parent.mkdir(parents=True, exist_ok=True)
        with p.open("ab" if a.get("append", False) else "wb") as f:
            f.write(data)
        result = {"path":str(p.absolute()), "bytes_written":len(data)}
except (OSError, OverflowError, UnicodeError, ValueError) as e:
    result = {"error":str(e)[:2048]}
print(json.dumps(result))
'''


class Unconfirmed(RuntimeError):
    """Cleanup could not be confirmed, so the caller must keep its fence."""


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
    if paths[1].stat().st_uid != policy["agent_uid"]:
        raise ValueError("native cache must belong to agent")
    # Admission checks root modes and the cache's cargo/, cargo/git/,
    # cargo/registry/ and tmp/ directories. Tools can change those, so checking
    # them here would fence the workspace.


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


def account_error(policy, passwd="/etc/passwd", shadow="/etc/shadow"):
    """A tool must not be able to start processes outside its sandbox by logging in."""
    def entries(path):
        with open(path, "rb") as source:
            data = source.read(4 * 1024**2 + 1)
        if len(data) > 4 * 1024**2:
            raise ValueError("account database too large")
        return [line.split(":") for line in data.decode("utf-8", "replace").splitlines()]
    users = [entry for entry in entries(passwd) if len(entry) == 7 and entry[2] == str(policy["agent_uid"])]
    if len(users) != 1:
        return "agent account needs exactly one local passwd entry"
    name, home, shell = users[0][0], users[0][5], users[0][6]
    if shell not in NOLOGIN_SHELLS:
        return "agent account must not have a login shell"
    passwords = [entry[1] for entry in entries(shadow) if len(entry) > 1 and entry[0] == name]
    if len(passwords) != 1 or not passwords[0].startswith(("!", "*")):
        return "agent account password must be locked"
    roots = [Path(policy[key]) for key in ("workspace", "cache", "toolchain")]
    for path in (home, os.path.join(home, ".ssh")):
        path = Path(os.path.realpath(path))
        if any(path.is_relative_to(root) for root in roots):
            return "agent home and SSH keys must lie outside the tool mounts"
    return None


def validate_request(request, policy):
    """Controller-chosen fields; a failure here is a protocol error."""
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


def argument_error(request, policy):
    """Model-chosen argument values. A refusal here dispatches nothing."""
    args = request["arguments"]
    for field in ("command", "cwd", "path"):
        if field in args and (not isinstance(args[field], str) or "\0" in args[field]):
            return f"{field} must be text without NUL characters"
    if "content" in args and not isinstance(args["content"], str):
        return "content must be text"
    if "offset_bytes" in args and (type(args["offset_bytes"]) is not int or args["offset_bytes"] < 0):
        return "offset_bytes must be a non-negative integer"
    if "append" in args and type(args["append"]) is not bool:
        return "append must be a boolean"
    if "timeout_seconds" in args and (type(args["timeout_seconds"]) not in (int, float) or not math.isfinite(args["timeout_seconds"]) or args["timeout_seconds"] <= 0):
        return "timeout_seconds must be a positive number"
    if policy["mode"] == "read_only" and request["tool"] == "write_file":
        return "write_file denied by read-only policy"
    return None


def receipt(result):
    return {"schema_version":1, "cleanup":"complete", "result":result}


def as_agent(policy):
    """setpriv to the agent's identity with no groups, capabilities or new privileges."""
    return ["/usr/bin/setpriv", "--reuid", str(policy["agent_uid"]), "--regid", str(policy["agent_gid"]),
            "--clear-groups", "--bounding-set=-all", "--inh-caps=-all", "--ambient-caps=-all", "--no-new-privs"]


def sandbox_args(policy, request, fds):
    """`fds` are the pinned workspace, cache and .git and the info pipe, then for
    a networked command its gate's two pipes and DNS configuration."""
    workspace, cache, git, info, *network = fds
    # Each command has its own network namespace. Only /tmp and /dev/shm are
    # writable outside the bound roots, and both are capped.
    args = ["/usr/bin/bwrap", "--unshare-all", "--unshare-user", "--disable-userns",
            "--die-with-parent", "--new-session", "--cap-drop", "ALL", "--clearenv", "--info-fd", str(info),
            "--ro-bind", "/usr", "/usr", "--tmpfs", "/usr/local", "--remount-ro", "/usr/local",
            "--ro-bind", "/lib", "/lib", "--ro-bind-try", "/lib64", "/lib64", "--symlink", "usr/bin", "/bin",
            "--symlink", "usr/sbin", "/sbin", "--proc", "/proc", "--dev", "/dev",
            "--size", str(SHM_BYTES), "--tmpfs", "/dev/shm", "--remount-ro", "/dev",
            "--size", str(TMP_BYTES), "--tmpfs", "/tmp", "--dir", "/tmp/home", "--dir", "/tmp/cargo"]
    # HOME and Cargo's configuration, credentials and installed tools start empty
    # for every command. Only Cargo's downloads, and its locks on them, persist.
    for name in ("registry", "git", ".package-cache", ".package-cache-mutate"):
        args += ["--symlink", f'{policy["cache"]}/cargo/{name}', f"/tmp/cargo/{name}"]
    args += ["--dir", "/etc"]
    for path in ("/etc/alternatives", "/etc/ld.so.cache", "/etc/hosts", "/etc/nsswitch.conf", "/etc/ssl/certs"):
        if Path(path).exists():
            args += ["--ro-bind", path, path]
    if network:
        args += ["--ro-bind-data", str(network[2]), "/etc/resolv.conf"]
    binding = "--bind-fd" if policy["mode"] == "edit" else "--ro-bind-fd"
    args += [binding, str(workspace), policy["workspace"], binding, str(cache), policy["cache"]]
    args += ["--ro-bind-fd", str(git), str(Path(policy["workspace"]) / ".git")]
    args += ["--ro-bind", policy["toolchain"], policy["toolchain"], "--remount-ro", "/"]
    environment = {"PATH":f'{policy["toolchain"]}/bin:/usr/bin:/bin', "HOME":"/tmp/home", "CARGO_HOME":"/tmp/cargo",
                   # Each command's cache tracker is new, so it must not clean shared downloads.
                   "CARGO_CACHE_AUTO_CLEAN_FREQUENCY":"never",
                   "TMPDIR":f'{policy["cache"]}/tmp' if policy["mode"] == "edit" else "/tmp",
                   "LANG":"C.UTF-8", "USER":"devbot-agent", "LOGNAME":"devbot-agent",
                   "GIT_CONFIG_SYSTEM":"/dev/null", "GIT_CONFIG_GLOBAL":"/dev/null",
                   "GIT_CONFIG_COUNT":"1", "GIT_CONFIG_KEY_0":"safe.directory", "GIT_CONFIG_VALUE_0":policy["workspace"]}
    for key, value in environment.items():
        args += ["--setenv", key, value]
    # Like the Docker bridge, a relative cwd resolves against the checkout.
    args += ["--chdir", os.path.join(policy["workspace"], request["arguments"].get("cwd", "")), "--"]
    if request["tool"] != "exec":
        return args + ["/usr/bin/python3", "-I", "-S", "-B", "-c", FILE_TOOL]
    shell = ["/bin/bash", "--noprofile", "--norc"]
    if not network:
        return args + [*shell, "-o", "pipefail", "-c", request["arguments"]["command"]]
    # Once running, the gate reports that setup is complete, then holds the
    # command until its network is up. If the helper dies first, the gate reads
    # EOF and the command never starts.
    ready, gate = network[:2]
    script = (f'printf 1 >&{ready} && exec {ready}>&- && IFS= read -r -n 1 -u {gate} go && [ "$go" = 1 ] && '
              f'exec {gate}<&- && exec {" ".join(shell)} -o pipefail -c "$1"')
    return args + [*shell, "-c", script, "gate", request["arguments"]["command"]]


def become_subreaper():
    """Bubblewrap's monitor exits as soon as its command does, while its PID 1
    may still be stopping other processes. That init is then re-parented here."""
    libc = ctypes.CDLL(None, use_errno=True)
    if libc.prctl(PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) != 0:
        raise OSError(ctypes.get_errno(), "cannot adopt the sandbox's init")


def sandbox_init(info):
    try:
        pid = decode(info)["child-pid"]
    except (ValueError, KeyError, TypeError):
        return None
    return pid if type(pid) is int and pid > 0 else None


def reap_init(pid, stop_by):
    """The kernel lets a PID namespace's init exit only after every other process
    in that namespace has, so reaping it confirms that no tool process remains."""
    if pid is not None:
        try:
            os.waitid(os.P_PID, pid, os.WEXITED | os.WNOHANG | os.WNOWAIT)
        except ChildProcessError:
            return  # The monitor already reaped it.
        # Only the monitor's own child can have been re-parented here.
        os.kill(pid, signal.SIGKILL)
        while os.waitpid(pid, os.WNOHANG)[0] != pid:
            if time.monotonic() > stop_by:
                raise Unconfirmed("native cleanup not confirmed within five seconds")
            time.sleep(.005)
        return
    # Without the init's PID, a remaining child can be neither identified nor excluded.
    try:
        os.waitid(os.P_ALL, 0, os.WEXITED | os.WNOHANG | os.WNOWAIT)
    except ChildProcessError:
        return
    raise Unconfirmed("native sandbox init unidentified; cleanup not confirmed")


def start_sandbox(policy, request, drop_identity, networked):
    """Returns the monitor process, Bubblewrap's info pipe and, for a networked
    command, its gate: the pipe that reports setup and the one that releases it."""
    passed, kept = [], []
    try:
        for key in ("workspace", "cache"):
            passed.append(os.open(policy[key], os.O_PATH | os.O_DIRECTORY | os.O_NOFOLLOW))
        passed.append(os.open(".git", os.O_PATH | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=passed[0]))
        for field, fd in zip(("workspace_identity", "cache_identity", "git_identity"), passed):
            info_stat = os.fstat(fd)
            if request[field] != {"device":info_stat.st_dev, "inode":info_stat.st_ino}:
                raise ValueError("native workspace/cache identity changed")
        info, write = os.pipe()
        kept.append(info)
        passed.append(write)
        if networked:
            ready, write = os.pipe()
            kept.append(ready)
            passed.append(write)
            gate, release = os.pipe()
            kept.append(release)
            passed.append(gate)
            dns, write = os.pipe()
            passed.append(dns)
            try:
                os.write(write, NETWORK_DNS)
            finally:
                os.close(write)
        args = sandbox_args(policy, request, passed)
        if drop_identity:
            limits = policy["limits"]
            args = [*as_agent(policy), "/usr/bin/prlimit", f'--as={limits["memory_bytes"]}',
                    f'--nproc={limits["processes"]}', "--nofile=4096", "--", *args]
        process = subprocess.Popen(args, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                   env={}, cwd="/", pass_fds=passed, start_new_session=True)
    except BaseException:
        for fd in kept:
            os.close(fd)
        raise
    finally:
        for fd in passed:
            os.close(fd)
    return process, os.fdopen(kept[0], "rb", buffering=0), tuple(kept[1:]) or None


def read_info(info, stop_by):
    """Bubblewrap reports its init, then closes this pipe, before releasing it."""
    data = bytearray()
    with selectors.DefaultSelector() as selector:
        selector.register(info, selectors.EVENT_READ)
        while selector.select(max(0, stop_by - time.monotonic())):
            part = os.read(info.fileno(), 4096)
            if not part:
                return sandbox_init(data)
            data += part[:4096 - len(data)]
    raise Unconfirmed("Bubblewrap did not report its sandbox within five seconds")


def gate_ready(fd, stop_by):
    """True once the gate runs, so Bubblewrap's setup is complete; False if the
    sandbox ended first."""
    with selectors.DefaultSelector() as selector:
        selector.register(fd, selectors.EVENT_READ)
        if not selector.select(max(0, stop_by - time.monotonic())):
            raise RuntimeError("sandbox setup did not finish in time")
    return os.read(fd, 1) == b"1"


def network_namespace(process, init):
    """Descriptors for the sandbox's network namespace and the user namespace
    that owns it. The init still had the monitor as its parent after the first
    was opened, so a reused PID cannot substitute another process."""
    task = os.open(f"/proc/{init}", os.O_PATH | os.O_DIRECTORY)
    try:
        net = os.open("ns/net", os.O_RDONLY | os.O_CLOEXEC, dir_fd=task)
        try:
            status = os.open("stat", os.O_RDONLY | os.O_CLOEXEC, dir_fd=task)
            try:
                fields = os.read(status, 4096).rpartition(b")")[2].split()
            finally:
                os.close(status)
            if len(fields) < 2 or fields[1] != str(process.pid).encode():
                raise RuntimeError("sandbox init is no longer Bubblewrap's child")
            host = os.stat("/proc/self/ns/net")
            if (os.fstat(net).st_dev, os.fstat(net).st_ino) == (host.st_dev, host.st_ino):
                raise RuntimeError("sandbox shares the host network namespace")
            return net, fcntl.ioctl(net, NS_GET_USERNS)
        except BaseException:
            os.close(net)
            raise
    finally:
        os.close(task)


class Network:
    """slirp4netns serving one sandbox, in its own process group. Only this
    helper holds its exit pipe, so it also exits if the helper dies."""

    def __init__(self, process, exit_fd):
        self.process, self.exit_fd = process, exit_fd

    def close(self):
        if self.exit_fd is not None:
            os.close(self.exit_fd)
            self.exit_fd = None
        # Until it is reaped, the group's leader keeps the group's ID from reuse.
        if self.process.returncode is None:
            try:
                os.killpg(self.process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass

    def stop(self, stop_by):
        """Also reaps slirp4netns's setup child, which is re-parented here if
        slirp4netns dies first."""
        self.close()
        while True:
            try:
                if self.process.poll() is not None and os.waitpid(-self.process.pid, os.WNOHANG)[0]:
                    continue
            except ChildProcessError:
                return
            if time.monotonic() > stop_by:
                raise Unconfirmed("native cleanup not confirmed within five seconds")
            time.sleep(.005)


def start_network(policy, process, init, drop_identity, stop_by):
    """User-mode NAT into the sandbox's own network namespace. slirp4netns runs
    as the agent in its own sandbox and never forwards to the host's loopback,
    so host services, including the model tunnel, stay unreachable.

    It starts inside the user namespace that owns that network namespace, where
    Bubblewrap maps the agent to uid 0, so it keeps the capabilities it needs to
    configure the network and sandbox itself. slirp4netns 1.3.3 would otherwise
    enter the init's current namespace, a nested one with no uid 0."""
    if init is None:
        raise RuntimeError("sandbox init unidentified")
    trusted_path(SLIRP4NETNS)
    passed = list(network_namespace(process, init))
    try:
        net, owner = passed
        ready, write = os.pipe()
        passed.append(write)
        exit_read, exit_fd = os.pipe()
        passed.append(exit_read)
        args = ["/usr/bin/nsenter", "--preserve-credentials", f"--user=/proc/self/fd/{owner}", "--",
                SLIRP4NETNS, "--configure", "--mtu=65520", "--disable-host-loopback", "--enable-sandbox",
                "--enable-seccomp", f"--ready-fd={write}", f"--exit-fd={exit_read}", "--netns-type=path",
                f"/proc/self/fd/{net}", "tap0"]
        if drop_identity:
            args = [*as_agent(policy), *args]
        try:
            slirp = subprocess.Popen(args, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                                     stderr=subprocess.DEVNULL, env={}, cwd="/", pass_fds=passed,
                                     start_new_session=True)
        except BaseException:
            os.close(ready)
            os.close(exit_fd)
            raise
    finally:
        for fd in passed:
            os.close(fd)
    network = Network(slirp, exit_fd)
    with os.fdopen(ready, "rb", buffering=0) as ready, selectors.DefaultSelector() as selector:
        selector.register(ready, selectors.EVENT_READ)
        configured = bool(selector.select(max(0, stop_by - time.monotonic()))) and ready.read(1) == b"1"
    if not configured:
        network.stop(time.monotonic() + 5)
        raise RuntimeError(f"slirp4netns did not configure the network (status {slirp.returncode})")
    return network


def file_result(tool, stdout, truncated):
    # The file tool reports only after it has finished, so a complete report
    # stands even if the deadline stopped the sandbox afterwards.
    try:
        result = None if truncated else decode(stdout)
    except ValueError:
        result = None
    if isinstance(result, dict):
        return result
    if tool == "write_file":
        return {"error":"write_file stopped before confirming its result; the file may have changed"}
    return {"error":"read_file failed or exceeded its bounds"}


def run_tool(policy, request, control_fd, *, drop_identity=True):
    """One PID and network namespace per command. The receipt says cleanup is
    complete only after that namespace's init has been reaped."""
    validate_request(request, policy)
    refusal = argument_error(request, policy)
    cancelled = STOP_REQUESTED
    if control_fd is not None:
        with selectors.DefaultSelector() as selector:
            selector.register(control_fd, selectors.EVENT_READ)
            cancelled |= bool(selector.select(0))
    if refusal or cancelled:
        return receipt({"error":refusal or "native tool cancelled"})
    become_subreaper()
    exec_tool = request["tool"] == "exec"
    # Only an editing role's commands get a network; file tools never need one.
    networked = exec_tool and policy["mode"] == "edit"
    process, info, gate = start_sandbox(policy, request, drop_identity, networked)
    started = time.monotonic()
    deadline = started + request["timeout_ms"] / 1000
    requested = request["arguments"].get("timeout_seconds")
    if requested is not None:
        deadline = min(deadline, started + requested)
    maximum = request["output_bytes"] if exec_tool else 6 * request["output_bytes"] + 8192
    output = {"stdout":bytearray(), "stderr":bytearray()}
    truncated = timed_out = False
    network = setup_error = None
    pending = b"" if exec_tool else json.dumps(request).encode()
    try:
        init = read_info(info, time.monotonic() + 5)
        if gate is not None:
            try:
                setup_by = min(time.monotonic() + 5, deadline)
                # If Bubblewrap's setup failed, its exit and stderr are the result.
                if gate_ready(gate[0], setup_by):
                    network = start_network(policy, process, init, drop_identity, setup_by)
                    os.write(gate[1], b"1")
            except Unconfirmed:
                raise
            except (OSError, ValueError, RuntimeError, subprocess.SubprocessError) as error:
                # The gate stayed closed, so no tool code ran.
                if time.monotonic() >= deadline:
                    timed_out = True
                else:
                    setup_error = f"native network setup failed: {error}"[:2048]
        with selectors.DefaultSelector() as selector:
            if control_fd is not None:
                selector.register(control_fd, selectors.EVENT_READ, "control")
            for label, stream in (("stdout", process.stdout), ("stderr", process.stderr)):
                os.set_blocking(stream.fileno(), False)
                selector.register(stream, selectors.EVENT_READ, label)
            if pending:
                os.set_blocking(process.stdin.fileno(), False)
                selector.register(process.stdin, selectors.EVENT_WRITE, "input")
            else:
                process.stdin.close()

            def service():
                nonlocal cancelled, pending, truncated
                for key, _ in selector.select(.02):
                    if key.data == "control":
                        # Any extra data is a protocol violation, EOF a lost owner.
                        os.read(control_fd, 1)
                        selector.unregister(control_fd)
                        cancelled = True
                    elif key.data == "input":
                        try:
                            pending = pending[os.write(process.stdin.fileno(), pending):]
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
                            room = max(0, maximum - len(output[key.data]))
                            output[key.data] += data[:room]
                            truncated |= len(data) > room

            def open_streams():
                return any(key.data in ("stdout", "stderr") for key in selector.get_map().values())

            while not (setup_error or timed_out) and process.poll() is None:
                cancelled |= STOP_REQUESTED
                if cancelled or time.monotonic() >= deadline:
                    timed_out = not cancelled
                    break
                service()
            stop_by = time.monotonic() + 5
            if process.poll() is None:
                process.kill()
            while process.poll() is None:
                if time.monotonic() > stop_by:
                    raise Unconfirmed("native cleanup not confirmed within five seconds")
                service()
            reap_init(init, stop_by)
            if network is not None:
                network.stop(stop_by)
            # No sandbox process remains to hold the output pipes open.
            while open_streams():
                if time.monotonic() > stop_by:
                    raise Unconfirmed("native cleanup not confirmed within five seconds")
                service()
        code = process.returncode
    finally:
        if process.poll() is None:
            process.kill()
            process.wait(timeout=5)
        if network is not None:
            network.close()
        for fd in gate or ():
            os.close(fd)
        for stream in (process.stdin, process.stdout, process.stderr, info):
            stream.close()
    if setup_error:
        result = {"error":setup_error}
    elif cancelled:
        result = {"error":"native tool cancelled"}
    elif exec_tool:
        result = {"stdout":output["stdout"].decode("utf-8", "replace"), "stderr":output["stderr"].decode("utf-8", "replace"),
                  "exit_code":code, "timed_out":timed_out, "output_truncated":truncated,
                  "duration_seconds":time.monotonic() - started}
    else:
        result = file_result(request["tool"], output["stdout"], truncated)
    return receipt(result)


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
    validate_request(request, policy)
    refusal = account_error(policy)
    print(json.dumps(receipt({"error":refusal}) if refusal else run_tool(policy, request, 0)), flush=True)


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, RuntimeError, KeyError, subprocess.TimeoutExpired) as error:
        print(f"native helper refused or failed: {error}", file=sys.stderr)
        sys.exit(1)
