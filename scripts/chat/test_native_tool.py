"""Real same-UID namespace fixtures; dedicated-account qualification is separate."""
import importlib.util
import json
import os
from pathlib import Path
import shlex
import shutil
import signal
import socket
import subprocess
import threading
import time
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("native_tool", Path(__file__).with_name("native_tool.py"))
native = importlib.util.module_from_spec(spec)
spec.loader.exec_module(native)


@unittest.skipUnless(os.environ.get("PLAYTIME_NATIVE_TESTS") == "1", "explicit native namespace gate")
class NativeSandboxTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="playtime-native-")
        self.root = Path(self.temp.name)
        self.workspace = self.root / "workspace"
        self.cache = self.root / "cache"
        self.workspace.mkdir()
        subprocess.run(["/usr/bin/git", "-c", "init.templateDir=", "init", "--quiet", "--initial-branch=main", str(self.workspace)], env={"PATH":"/usr/bin:/bin", "HOME":str(self.root)}, check=True)
        self.cache.mkdir()
        for name in ("cargo/git", "cargo/registry", "tmp"):
            (self.cache / name).mkdir(parents=True)
        # Tests share only this selected toolchain, never the operator home.
        rustc = shutil.which("rustc")
        if not rustc:
            self.fail("native gate requires Rust")
        toolchain = subprocess.check_output([rustc, "--print", "sysroot"], text=True).strip()
        self.policy = {"schema_version":1, "controller_uid":os.getuid() + 1,
                       "agent_uid":os.getuid(), "agent_gid":os.getgid(),
                       "workspace":str(self.workspace), "cache":str(self.cache), "toolchain":toolchain,
                       "helper":str(Path(native.__file__).resolve()), "mode":"edit",
                       "limits":{"command_seconds":30, "memory_bytes":16 * 1024**3, "processes":512, "output_bytes":4096}}
        self.read_fd, self.write_fd = os.pipe()
        self.addCleanup(os.close, self.write_fd)
        self.addCleanup(os.close, self.read_fd)
        self.addCleanup(self.temp.cleanup)

    def request(self, tool="exec", **arguments):
        info = self.workspace.stat()
        return {"schema_version":1, "policy_sha256":"fixture", "helper_sha256":"fixture", "workspace_identity":{"device":info.st_dev, "inode":info.st_ino},
                "cache_identity":{"device":self.cache.stat().st_dev,"inode":self.cache.stat().st_ino},
                "git_identity":{"device":(self.workspace / ".git").stat().st_dev,"inode":(self.workspace / ".git").stat().st_ino}, "tool":tool, "arguments":arguments, "timeout_ms":30000, "output_bytes":4096}

    def run_tool(self, request):
        reply = native.run_tool(self.policy, request, self.read_fd, drop_identity=False)
        self.assertEqual(reply["cleanup"], "complete")
        return reply["result"]

    def test_actual_rust_write_build_and_test_at_host_path(self):
        (self.workspace / "src").mkdir()
        self.run_tool(self.request("write_file", path="Cargo.toml", content='[package]\nname="native-fixture"\nversion="0.1.0"\nedition="2024"\n'))
        self.run_tool(self.request("write_file", path="src/lib.rs", content='pub fn add(a:u32,b:u32)->u32 { a+b }\n#[test] fn works(){assert_eq!(add(2,3),5);}\n'))
        result = self.run_tool(self.request(command="pwd && cargo test --offline"))
        self.assertEqual(result["exit_code"], 0, result)
        self.assertIn(str(self.workspace), result["stdout"])
        self.assertIn("1 passed", result["stdout"])
        self.assertTrue((self.workspace / "target").is_dir())
        read = self.run_tool(self.request("read_file", path="src/lib.rs", offset_bytes=0))
        self.assertIn("pub fn add", read["content"])

    def test_errors_bounds_and_pipeline_status(self):
        result = self.run_tool(self.request(command="printf error >&2; false | cat"))
        self.assertEqual(result["exit_code"], 1)
        self.assertEqual(result["stderr"], "error")
        result = self.run_tool(self.request(command="head -c 100000 /dev/zero"))
        self.assertEqual(len(result["stdout"]), 4096)
        self.assertTrue(result["output_truncated"])
        self.assertEqual(result["exit_code"], 0)

    def test_timeout_and_normal_exit_kill_detached_descendants(self):
        marker = self.workspace / "late"
        command = "setsid sh -c 'sleep .4; echo escaped > late' </dev/null >/dev/null 2>&1 & "
        result = self.run_tool(self.request(command=command + "sleep 10", timeout_seconds=.1))
        self.assertTrue(result["timed_out"])
        self.assertNotEqual(result["exit_code"], 0)
        time.sleep(.5)
        self.assertFalse(marker.exists())
        result = self.run_tool(self.request(command=command + "true"))
        self.assertEqual(result["exit_code"], 0)
        time.sleep(.5)
        self.assertFalse(marker.exists())

    def test_control_pipe_cancellation_kills_descendants(self):
        # A protocol byte is also a cancellation signal; keep fd ownership local.
        timer = threading.Timer(.15, lambda: os.write(self.write_fd, b"!"))
        timer.start()
        self.addCleanup(timer.join)
        started = time.monotonic()
        result = self.run_tool(self.request(command="setsid sh -c 'sleep .4; touch late' & wait"))
        self.assertEqual(result["error"], "native tool cancelled")
        self.assertLess(time.monotonic() - started, 2)
        time.sleep(.5)
        self.assertFalse((self.workspace / "late").exists())

    def test_helper_signal_confirms_namespace_cleanup(self):
        program = """
import importlib.util,json,sys
spec=importlib.util.spec_from_file_location('native',sys.argv[1]);native=importlib.util.module_from_spec(spec);spec.loader.exec_module(native)
native.install_signals()
print(json.dumps(native.run_tool(json.loads(sys.argv[2]),json.loads(sys.argv[3]),0,drop_identity=False)),flush=True)
"""
        request = self.request(command="touch started; setsid sh -c 'sleep .5; touch escaped' & wait")
        with subprocess.Popen(['/usr/bin/python3','-IS','-c',program,str(Path(native.__file__).resolve()),json.dumps(self.policy),json.dumps(request)],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE) as process:
            deadline = time.monotonic() + 3
            while not (self.workspace / 'started').exists() and time.monotonic() < deadline:
                time.sleep(.01)
            self.assertTrue((self.workspace / 'started').exists())
            process.terminate()
            # Keep the control pipe open: SIGTERM itself must drive cleanup.
            process.wait(timeout=3)
            stdout, stderr = process.communicate()
            self.assertEqual(process.returncode, 0, stderr)
            result = json.loads(stdout)
            self.assertEqual(result['cleanup'], 'complete')
            self.assertEqual(result['result']['error'], 'native tool cancelled')
        time.sleep(.6)
        self.assertFalse((self.workspace / 'escaped').exists())

    def test_private_files_credentials_sockets_and_parent_processes_hidden(self):
        secret = self.root / "private-evidence"
        secret.write_text("do not expose")
        (self.workspace / "escaped-link").symlink_to(secret)
        parent_pid = os.getpid()
        denied = [str(secret), str(Path.home() / ".ssh"), "/var/run/docker.sock", "/run/docker.sock", "/proc/1/root" + str(secret), "escaped-link"]
        command = "\n".join(f"test ! -r {shlex.quote(path)} || exit 61" for path in denied)
        command += f"\ntest ! -d /proc/{parent_pid} || exit 62\ntest -z \"${{SSH_AUTH_SOCK:-}}\"\n"
        result = self.run_tool(self.request(command=command))
        self.assertEqual(result["exit_code"], 0, result)
        result = self.run_tool(self.request(command="ls -l /proc/self/fd"))
        self.assertNotIn(str(self.root), result["stdout"])

    def test_read_only_policy_blocks_exec_and_file_writes(self):
        self.policy["mode"] = "read_only"
        result = self.run_tool(self.request("write_file", path="x", content="x"))
        self.assertIn("read-only", result["error"])
        self.assertFalse((self.workspace / "x").exists())
        for command in ["touch x", f"touch {shlex.quote(str(self.cache / 'x'))}"]:
            result = self.run_tool(self.request(command=command))
            self.assertNotEqual(result["exit_code"], 0, result)
        self.assertEqual(self.run_tool(self.request(command="touch /tmp/scratch"))["exit_code"], 0)

    def reachable(self):
        """What a command can connect to: its own loopback, host services bound
        to loopback (as the model tunnel is), a host abstract socket, and a host
        service on a non-loopback address when the host has one."""
        listeners = []

        def listen(family, address):
            server = socket.socket(family, socket.SOCK_STREAM)
            listeners.append(server)
            server.bind(address)
            server.listen()
            return server.getsockname()

        loopback = listen(socket.AF_INET, ("127.0.0.1", 0))[1]
        abstract = f"playtime-native-test-{os.getpid()}-{time.monotonic_ns()}"
        listen(socket.AF_UNIX, "\0" + abstract)
        try:
            with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as probe:
                probe.connect(("192.0.2.1", 9))  # Sends nothing; selects the default route's address.
                host = listen(socket.AF_INET, (probe.getsockname()[0], 0))
        except OSError:
            host = ("", 0)
        script = """
import json, socket, sys
port, abstract, host, host_port = sys.argv[1:]
def reach(family, address):
    with socket.socket(family, socket.SOCK_STREAM) as client:
        client.settimeout(3)
        try:
            client.connect(address)
            return True
        except OSError:
            return False
own = socket.socket()
own.bind(("127.0.0.1", 0))
own.listen()
print(json.dumps({"own loopback":reach(socket.AF_INET, own.getsockname()),
                  "host loopback":reach(socket.AF_INET, ("127.0.0.1", int(port))),
                  "gateway":reach(socket.AF_INET, ("10.0.2.2", int(port))),
                  "DNS forwarder":reach(socket.AF_INET, ("10.0.2.3", int(port))),
                  "abstract socket":reach(socket.AF_UNIX, "\\0" + abstract),
                  "host address":reach(socket.AF_INET, (host, int(host_port))) if host else None}))
"""
        try:
            command = shlex.join(["python3", "-I", "-c", script, str(loopback), abstract, host[0], str(host[1])])
            result = self.run_tool(self.request(command=command))
        finally:
            for server in listeners:
                server.close()
        self.assertEqual(result["exit_code"], 0, result)
        return json.loads(result["stdout"]), host[0]

    def test_edit_commands_have_a_private_network_and_read_only_commands_none(self):
        reached, host = self.reachable()
        self.assertEqual(reached, {"own loopback":True, "host loopback":False, "gateway":False, "DNS forwarder":False,
                                   "abstract socket":False, "host address":True if host else None})
        self.policy["mode"] = "read_only"
        reached, host = self.reachable()
        self.assertEqual(reached, {"own loopback":True, "host loopback":False, "gateway":False, "DNS forwarder":False,
                                   "abstract socket":False, "host address":False if host else None})
        result = self.run_tool(self.request(command="test ! -e /etc/resolv.conf"))
        self.assertEqual(result["exit_code"], 0, result)

    def test_edit_commands_resolve_names_through_the_forwarder(self):
        # Any reply proves the path; an offline host's resolver still answers.
        script = """
import socket, struct
configuration = open("/etc/resolv.conf").read()
assert configuration == "nameserver 10.0.2.3\\n", configuration
query = struct.pack(">HHHHHH", 0x5054, 0x0100, 1, 0, 0, 0) + b"\\x09localhost\\x00" + struct.pack(">HH", 1, 1)
with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as client:
    client.settimeout(5)
    client.sendto(query, ("10.0.2.3", 53))
    reply = client.recv(512)
assert reply[:2] == query[:2] and reply[2] & 0x80, reply
"""
        result = self.run_tool(self.request(command=shlex.join(["python3", "-I", "-c", script])))
        self.assertEqual(result["exit_code"], 0, result)

    def test_home_and_cargo_configuration_are_private_to_each_command(self):
        command = ('printf "%s %s" "$HOME" "$CARGO_HOME" && touch "$HOME/marker" && mkdir "$CARGO_HOME/bin" && '
                   'echo "[net]" > "$CARGO_HOME/config.toml" && touch "$CARGO_HOME/registry/kept" "$CARGO_HOME/git/kept"')
        result = self.run_tool(self.request(command=command))
        self.assertEqual((result["exit_code"], result["stdout"]), (0, "/tmp/home /tmp/cargo"), result)
        command = ('test ! -e "$HOME/marker" && test ! -e "$CARGO_HOME/config.toml" && test ! -e "$CARGO_HOME/bin" && '
                   'test -e "$CARGO_HOME/registry/kept" && test -e "$CARGO_HOME/git/kept"')
        self.assertEqual(self.run_tool(self.request(command=command))["exit_code"], 0)
        self.assertEqual(sorted(path.relative_to(self.cache).as_posix() for path in self.cache.rglob("*")),
                         ["cargo", "cargo/git", "cargo/git/kept", "cargo/registry", "cargo/registry/kept", "tmp"])

    def test_network_setup_failure_runs_nothing(self):
        self.addCleanup(setattr, native, "SLIRP4NETNS", native.SLIRP4NETNS)
        self.addCleanup(setattr, native, "trusted_path", native.trusted_path)
        stand_in = self.root / "slirp4netns"
        stand_in.write_text("#!/bin/sh\nexit 1\n")
        stand_in.chmod(0o755)
        native.SLIRP4NETNS = str(stand_in)

        def refused(**arguments):
            result = self.run_tool(self.request(command="touch should-not-run", **arguments))
            self.assertFalse((self.workspace / "should-not-run").exists())
            with self.assertRaises(ChildProcessError):
                os.waitid(os.P_ALL, 0, os.WEXITED | os.WNOHANG | os.WNOWAIT)
            return result

        self.assertIn("root-owned", refused()["error"])
        # A same-UID test cannot install root-owned stand-ins.
        native.trusted_path = lambda path, directory=False: Path(path)
        self.assertEqual(refused()["error"], "native network setup failed: slirp4netns did not configure the network (status 1)")
        # It never reports ready, and its child would outlive it without the group kill.
        stand_in.write_text("#!/bin/sh\nsleep 30 &\nexec sleep 30\n")
        self.assertTrue(refused(timeout_seconds=.5)["timed_out"])

    def test_helper_death_before_release_runs_nothing_and_stops_the_network(self):
        program = """
import importlib.util,json,os,signal,sys
spec=importlib.util.spec_from_file_location('native',sys.argv[1]);native=importlib.util.module_from_spec(spec);spec.loader.exec_module(native)
start = native.start_network
def started(*arguments):
    print(start(*arguments).process.pid, flush=True)
    os.kill(os.getpid(), signal.SIGKILL)
native.start_network = started
native.run_tool(json.loads(sys.argv[2]),json.loads(sys.argv[3]),0,drop_identity=False)
"""
        # The killed helper's sandbox and network processes are re-parented here.
        native.become_subreaper()
        with subprocess.Popen(['/usr/bin/python3','-IS','-c',program,str(Path(native.__file__).resolve()),json.dumps(self.policy),json.dumps(self.request(command="touch should-not-run"))],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE) as process:
            # The control pipe stays open, so only the helper's death ends the tool.
            process.wait(timeout=10)
            stdout, stderr = process.stdout.read(), process.stderr.read()
        self.assertEqual(process.returncode, -signal.SIGKILL, stderr)
        reaped, deadline = set(), time.monotonic() + 3
        while True:
            try:
                pid = os.waitpid(-1, os.WNOHANG)[0]
            except ChildProcessError:
                break
            reaped.add(pid)
            if not pid:
                self.assertLess(time.monotonic(), deadline, "the killed helper's processes kept running")
                time.sleep(.01)
        self.assertIn(int(stdout), reaped)
        self.assertFalse((self.workspace / "should-not-run").exists())

    def test_git_metadata_cannot_be_written_renamed_or_replaced(self):
        head = (self.workspace / '.git/HEAD').read_bytes()
        for command in ["echo changed > .git/HEAD", "git config forbidden.setting yes", "mv .git .git-old", "rm -rf .git", "ln -sfn /tmp .git"]:
            result = self.run_tool(self.request(command=command))
            self.assertNotEqual(result['exit_code'], 0, (command, result))
        self.assertEqual((self.workspace / '.git/HEAD').read_bytes(), head)
        self.assertFalse((self.workspace / '.git-old').exists())
        self.assertFalse((self.workspace / '.git').is_symlink())
        git = self.workspace / '.git'
        git.rename(self.workspace / 'metadata')
        git.write_text('gitdir: metadata')
        with self.assertRaises(ValueError):
            native.validate_policy(self.policy)
        git.unlink()
        git.symlink_to(self.workspace / 'metadata')
        with self.assertRaises(ValueError):
            native.validate_policy(self.policy)

    def host_processes(self, marker):
        found = []
        for entry in Path("/proc").iterdir():
            try:
                if entry.name.isdigit() and marker in (entry / "cmdline").read_bytes():
                    found.append(entry.name)
            except OSError:
                pass
        return found

    def test_receipt_waits_until_the_sandbox_namespace_is_empty(self):
        inits = []
        identify = native.sandbox_init
        self.addCleanup(setattr, native, "sandbox_init", identify)
        native.sandbox_init = lambda info: inits.append(identify(info)) or inits[-1]
        marker = f"native-marker-{os.getpid()}-{time.monotonic_ns()}"
        daemon = f"(exec -a {marker} sleep 30) </dev/null >/dev/null 2>&1 & "
        for arguments in ({"command":daemon + "true"}, {"command":daemon + "sleep 10", "timeout_seconds":.2}):
            result = self.run_tool(self.request(**arguments))
            # Checked at once: the receipt itself must mean no tool process remains.
            self.assertEqual(self.host_processes(marker.encode()), [], arguments)
            self.assertEqual(result["timed_out"], "timeout_seconds" in arguments)
            # Bubblewrap reported its init, and the helper reaped it.
            self.assertIsInstance(inits[-1], int)
            self.assertFalse(Path(f"/proc/{inits[-1]}").exists())
            with self.assertRaises(ChildProcessError):
                os.waitid(os.P_ALL, 0, os.WEXITED | os.WNOHANG | os.WNOWAIT)

    def test_model_argument_refusals_are_complete_receipts_without_dispatch(self):
        marker = self.workspace / "should-not-run"
        for tool, arguments in [("exec", {"command":"touch should-not-run", "timeout_seconds":0}),
                                ("exec", {"command":"touch should-not-run", "timeout_seconds":-1}),
                                ("exec", {"command":"touch should-not-run\0"}),
                                ("exec", {"command":"touch should-not-run", "cwd":"src\0"}),
                                ("read_file", {"path":"x", "offset_bytes":-1}),
                                ("read_file", {"path":"x\0"}),
                                ("write_file", {"path":"should-not-run", "content":"x", "append":"yes"})]:
            reply = native.run_tool(self.policy, self.request(tool, **arguments), self.read_fd, drop_identity=False)
            self.assertEqual(reply["cleanup"], "complete")
            self.assertIn("error", reply["result"], arguments)
            self.assertFalse(marker.exists(), arguments)

    def test_sandbox_root_is_read_only_and_temporary_space_is_bounded(self):
        command = "\n".join(f"( : > {path}/probe ) 2>/dev/null && exit 61" for path in ("", "/etc", "/usr/local", "/dev"))
        command += "\nhead -c 70000000 /dev/zero > /dev/shm/big 2>/dev/null && exit 62"
        command += "\nrm /dev/shm/big; echo ok > /dev/null && : > /tmp/probe && python3 -c 'import multiprocessing; multiprocessing.Lock()'"
        result = self.run_tool(self.request(command=command))
        self.assertEqual(result["exit_code"], 0, result)

    def test_file_tools_match_the_docker_bridge(self):
        (self.workspace / "src").mkdir()
        result = self.run_tool(self.request(command="pwd", cwd="src"))
        self.assertEqual(result["stdout"], f"{self.workspace}/src\n", result)
        written = self.run_tool(self.request("write_file", path="new/dir/file.txt", content="a\0b"))
        self.assertEqual(written, {"path":str(self.workspace / "new/dir/file.txt"), "bytes_written":3})
        # 4095 ASCII bytes and a three-byte character split by the 4096-byte limit.
        (self.workspace / "split.txt").write_text("a" * 4095 + "\u20ac")
        read = self.run_tool(self.request("read_file", path="split.txt"))
        self.assertTrue(read["truncated"])
        self.assertEqual(read["content"], "a" * 4095 + "\ufffd")
        self.assertEqual(self.run_tool(self.request("read_file", path="split.txt", offset_bytes=4095))["content"], "\u20ac")
        (self.workspace / "binary").write_bytes(b"\x89PNG\r\n\x1a\n\x00")
        binary = self.run_tool(self.request("read_file", path="binary"))
        self.assertIn("only supports UTF-8 text", binary["error"])
        self.assertEqual(binary["size_bytes"], 9)

    def test_workspace_identity_and_policy_validation(self):
        native.validate_policy(self.policy)
        request = self.request(command="touch should-not-run")
        request["workspace_identity"]["inode"] += 1
        with self.assertRaises(ValueError):
            self.run_tool(request)
        self.assertFalse((self.workspace / "should-not-run").exists())
        request = self.request(command="touch should-not-run")
        request["cache_identity"]["inode"] += 1
        with self.assertRaises(ValueError):
            self.run_tool(request)
        self.policy["agent_uid"] = self.policy["controller_uid"]
        with self.assertRaises(ValueError):
            native.validate_policy(self.policy)


class NativePolicyTests(unittest.TestCase):
    def test_unprivileged_or_writable_helper_policy_refused(self):
        with self.assertRaises(ValueError):
            native.load_policy(Path(__file__))

    def test_duplicate_fields_are_not_last_write_wins(self):
        with self.assertRaises(ValueError):
            native.decode('{"tool":"read_file","tool":"exec"}')

    def test_agent_account_cannot_log_in_or_reach_its_keys(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            policy = {"agent_uid":998, "workspace":"/srv/checkout", "cache":"/var/lib/agent/projects/one",
                      "toolchain":"/opt/toolchain"}

            def check(home="/var/lib/agent", shell="/usr/sbin/nologin", password="!", extra=""):
                (root / "passwd").write_text(f"root:x:0:0::/root:/bin/bash\nagent:x:998:991::{home}:{shell}\n{extra}")
                (root / "shadow").write_text(f"root:*:1::::::\nagent:{password}:1::::::\n")
                return native.account_error(policy, root / "passwd", root / "shadow")
            self.assertIsNone(check())
            self.assertIsNone(check(shell="/bin/false", password="*"))
            self.assertIn("login shell", check(shell="/bin/bash"))
            self.assertIn("locked", check(password="$6$salt$hash"))
            self.assertIn("locked", check(password=""))
            self.assertIn("outside the tool mounts", check(home="/var/lib/agent/projects/one/home"))
            self.assertIn("outside the tool mounts", check(home="/srv/checkout"))
            # A mount below the home leaves the home and its keys unexposed.
            self.assertIsNone(check(home="/srv"))
            self.assertIn("exactly one", check(extra="again:x:998:991::/x:/usr/sbin/nologin\n"))

    def test_only_a_complete_file_tool_report_is_trusted(self):
        report = b'{"path":"/w/x","bytes_written":1}\n'
        self.assertEqual(native.file_result("write_file", report, False), {"path":"/w/x", "bytes_written":1})
        self.assertIn("may have changed", native.file_result("write_file", report[:10], False)["error"])
        self.assertIn("may have changed", native.file_result("write_file", report, True)["error"])
        self.assertIn("failed", native.file_result("read_file", b"", False)["error"])


if __name__ == "__main__":
    unittest.main()
