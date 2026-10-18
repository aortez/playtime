"""Independent CPU probes for embedded Rust capture helpers; no extraction.

ROOT replacement is test-only. Production source hard-codes /work and accepts
limits only. Frozen reference bytes were reviewed in #54, not regenerated here.
"""
import hashlib
import io
import json
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
HELPERS = ROOT / "crates/playtime-docker/src/capture"
GOLDEN = ROOT / "crates/playtime-docker/tests/fixtures/capture/reference.tar"
LIMITS = {"entries": 4096, "tar_bytes": 64 << 20,
          "file_bytes": 8 << 20, "retained_bytes": 8 << 20}


class RustCaptureHelpers(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="playtime-capture-helpers-")
        self.addCleanup(self.temp.cleanup)
        self.work = Path(self.temp.name) / "work"
        self.work.mkdir()

    def run_helper(self, name="exporter.py", data=None, transform=None):
        source = (HELPERS / name).read_text()
        source = source.replace('ROOT = "/work"', "ROOT = " + repr(str(self.work)))
        if transform:
            source = transform(source)
        env = {**os.environ, "PYTHONPATH": str(self.work), "PYTHONHOME": str(self.work),
               "HOME": str(self.work), "PYTHONUSERBASE": str(self.work / ".local")}
        return subprocess.run(["python3", "-I", "-S", "-B", "-c", source],
                              input=json.dumps(data if data is not None else LIMITS).encode(),
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                              cwd=self.work, env=env, timeout=10, check=False)

    def test_reference_independent_checksum_and_hash(self):
        raw = GOLDEN.read_bytes()
        self.assertEqual(len(raw), 10240)
        self.assertEqual(hashlib.sha256(raw).hexdigest(),
                         "e465b5958f9892de45a47cfe25802ce2330d1753fc8eb116a4e77dd1ae3b092e")
        pos, physical = 0, 0
        while raw[pos:pos + 512] != bytes(512):
            header = raw[pos:pos + 512]
            recorded = int(header[148:156].strip(b"\x00 "), 8)
            self.assertEqual(recorded, sum(header[:148]) + 8 * 32 + sum(header[156:]))
            size = int(header[124:136].strip(b"\x00 "), 8)
            pos += 512 + ((size + 511) // 512) * 512
            physical += 1
        self.assertEqual(physical, 7)
        self.assertGreaterEqual(len(raw) - pos, 1024)
        self.assertFalse(any(raw[pos:]))
        with tarfile.open(fileobj=io.BytesIO(raw), mode="r|") as archive:
            members = list(archive)
        self.assertEqual([m.name for m in members],
                         ["a.txt", "alias", "dir", "dir/é.txt", "outside", "run.sh"])
        self.assertEqual([m.mode for m in members], [0o640, 0o640, 0o750, 0o644, 0o777, 0o755])

    def test_export_matches_reviewed_reference_exactly(self):
        (self.work / "a.txt").write_bytes(b"alpha\n")
        (self.work / "a.txt").chmod(0o640)
        os.link(self.work / "a.txt", self.work / "alias")
        (self.work / "dir").mkdir(mode=0o750)
        (self.work / "dir/é.txt").write_bytes(b"beta\n")
        (self.work / "dir/é.txt").chmod(0o644)
        (self.work / "outside").symlink_to("../../outside")
        (self.work / "run.sh").write_bytes(b"#!/bin/sh\nexit 0\n")
        (self.work / "run.sh").chmod(0o755)
        result = self.run_helper()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, GOLDEN.read_bytes())

    def test_isolated_imports_and_owner_only_files(self):
        # Every explicit import in both helpers, their important dependencies,
        # and interpreter startup hooks. Poison cwd, PYTHONPATH and user site.
        names = ("codecs", "json", "os", "stat", "sys", "tarfile", "io", "struct", "copy", "re",
                 "time", "pwd", "grp", "shutil", "sitecustomize", "usercustomize")
        poison = "raise RuntimeError('workspace code executed')\n"
        for name in names:
            (self.work / (name + ".py")).write_text(poison)
        site = self.work / ".local/lib/python3.14/site-packages"
        site.mkdir(parents=True)
        (site / "sitecustomize.py").write_text(poison)
        (self.work / "MEMENTO.md").write_text("actual memory\n")
        (self.work / "MEMENTO.md").chmod(0o600)
        result = self.run_helper()
        self.assertEqual(result.returncode, 0, result.stderr)
        with tarfile.open(fileobj=io.BytesIO(result.stdout), mode="r:") as archive:
            self.assertEqual(archive.extractfile("MEMENTO.md").read(), b"actual memory\n")
            self.assertEqual(archive.getmember("MEMENTO.md").mode, 0o600)
        result = self.run_helper("memory.py", 32)
        self.assertEqual(json.loads(result.stdout), {"status": "loaded", "content": "actual memory\n"})

    def test_memory_statuses_and_exact_bytes(self):
        file = self.work / "MEMENTO.md"
        def check(status, content=""):
            result = self.run_helper("memory.py", 8)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(json.loads(result.stdout), {"status": status, "content": content})
        check("missing")
        file.write_bytes(b""); check("loaded")
        file.write_bytes(b"hi\n"); check("loaded", "hi\n")
        file.write_bytes(b"ninebytes"); check("oversized")
        file.write_bytes(b"\x00" + b"x" * 32); check("binary")
        file.write_bytes(b"abcdefg" + "é".encode() * 10); check("oversized")
        file.write_bytes(b"x" * 4095 + "é".encode()); check("oversized")
        file.write_bytes(b"\x00"); check("binary")
        file.write_bytes(b"\xff"); check("binary")
        file.unlink(); file.symlink_to("/etc/passwd"); check("unreadable")
        file.unlink(); os.mkfifo(file); check("unreadable")
        file.unlink(); file.write_text("private"); file.chmod(0)
        if os.geteuid() != 0:
            check("unreadable")
            self.assertNotEqual(self.run_helper().returncode, 0)
        file.chmod(0o600)

    def test_unsupported_types_sparse_bounds_and_mutation_fail(self):
        file = self.work / "fifo"
        os.mkfifo(file)
        self.assertNotEqual(self.run_helper().returncode, 0)
        file.unlink()
        file = self.work / "sparse"
        with file.open("wb") as stream:
            stream.truncate((8 << 20) + 1)
        self.assertNotEqual(self.run_helper().returncode, 0)
        file.unlink()
        (self.work / "regular").write_text("initial")
        # Deterministic race injection after inventory; source mutation is only
        # in this test copy, and expected output is independently checked.
        mutate = lambda source: source.replace('walk(root, "")',
            'walk(root, "")\n        open(ROOT + "/regular", "w").write("changed")')
        self.assertNotEqual(self.run_helper(transform=mutate).returncode, 0)
        # Mutation after reading is also rejected by the second fstat.
        mutate = lambda source: source.replace('archive.addfile(header, body)',
            'archive.addfile(header, body)\n                                open(ROOT + "/regular", "w").write("changed again")')
        self.assertNotEqual(self.run_helper(transform=mutate).returncode, 0)
        result = self.run_helper(data={**LIMITS, "entries": 1})
        self.assertEqual(result.returncode, 0, result.stderr)
        (self.work / "second").write_text("x")
        self.assertNotEqual(self.run_helper(data={**LIMITS, "entries": 1}).returncode, 0)
        self.assertNotEqual(self.run_helper(data={**LIMITS, "tar_bytes": 1024}).returncode, 0)


if __name__ == "__main__":
    unittest.main()
