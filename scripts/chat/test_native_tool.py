"""Real same-UID namespace fixtures; dedicated-account qualification is separate."""
import importlib.util
import json
import os
from pathlib import Path
import shlex
import shutil
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
        for name in ("home", "cargo", "tmp"):
            (self.cache / name).mkdir()
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
        with self.assertRaises(ValueError):
            self.run_tool(self.request("write_file", path="x", content="x"))
        for command in ["touch x", f"touch {shlex.quote(str(self.cache / 'x'))}"]:
            result = self.run_tool(self.request(command=command))
            self.assertNotEqual(result["exit_code"], 0, result)
        self.assertEqual(self.run_tool(self.request(command="touch /tmp/scratch"))["exit_code"], 0)

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


if __name__ == "__main__":
    unittest.main()
