"""Offline tests of the seed authoring helper; never run an experiment."""
import importlib.util
import json
import os
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("seed_bundle", Path(__file__).with_name("seed-bundle.py"))
bundles = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bundles)


class SeedBundles(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        bundles.create(self.root, "original", "Original")

    def test_clone_has_independent_bytes_and_source_digest(self):
        manifest = self.root / "seed-bundles/original.json"
        value = json.loads(manifest.read_text())
        value["assessment"] = "assessments/original"
        (self.root / "assessments/original").mkdir(parents=True)
        manifest.write_text(json.dumps(value))
        source = bundles.validate(self.root, "original")[2]
        bundles.create(self.root, "copy", "Copy", "original")
        copied, files, before = bundles.validate(self.root, "copy")
        self.assertIn(source["bundle_sha256"], copied["provenance"])
        self.assertIsNone(copied["assessment"])
        (self.root / "seeds/original/ABOUT.txt").write_text("changed")
        self.assertEqual(before, bundles.validate(self.root, "copy")[2])
        self.assertNotEqual(files, bundles.validate(self.root, "original")[1])

    def test_digest_changes_with_content_and_metadata(self):
        before = bundles.validate(self.root, "original")[2]["bundle_sha256"]
        seed = self.root / "seeds/original/ABOUT.txt"
        seed.write_text("different")
        after = bundles.validate(self.root, "original")[2]["bundle_sha256"]
        self.assertNotEqual(before, after)
        manifest = self.root / "seed-bundles/original.json"
        value = json.loads(manifest.read_text())
        value["revision"] += 1
        manifest.write_text(json.dumps(value))
        self.assertNotEqual(after, bundles.validate(self.root, "original")[2]["bundle_sha256"])

    def test_link_and_binary_inputs_are_refused(self):
        extra = self.root / "seeds/original/extra"
        extra.symlink_to(self.root / "seeds/original/ABOUT.txt")
        with self.assertRaises(ValueError):
            bundles.validate(self.root, "original")
        extra.unlink()
        extra.write_bytes(b"\xff")
        with self.assertRaises(UnicodeError):
            bundles.validate(self.root, "original")

    def test_raw_encoded_and_file_count_bounds(self):
        seed = self.root / "seeds/original/ABOUT.txt"
        for content in ("a" * 131073, "\x01" * 40000):
            seed.write_text(content)
            with self.assertRaises(ValueError):
                bundles.validate(self.root, "original")
        seed.write_text("ok")
        for i in range(16):
            (seed.parent / str(i)).write_text("x")
        with self.assertRaises(ValueError):
            bundles.validate(self.root, "original")

    def test_create_refuses_overwrite_and_traversal(self):
        before = bundles.validate(self.root, "original")[2]
        for bundle_id in ("original", "../escape", "absolute/path", "bad id"):
            with self.assertRaises(ValueError):
                bundles.create(self.root, bundle_id, "Bad")
        self.assertEqual(before, bundles.validate(self.root, "original")[2])

    @unittest.skipIf(os.geteuid() == 0, "root bypasses directory read permissions")
    def test_unreadable_subtree_is_not_silently_dropped(self):
        directory = self.root / "seeds/original/private"
        directory.mkdir()
        (directory / "important.txt").write_text("must be copied or refused")
        directory.chmod(0)
        try:
            with self.assertRaises(PermissionError):
                bundles.validate(self.root, "original")
            with self.assertRaises(PermissionError):
                bundles.create(self.root, "copy", "Copy", "original")
            self.assertFalse((self.root / "seeds/copy").exists())
        finally:
            directory.chmod(0o700)

    def test_oversized_manifest_is_refused_before_writing(self):
        with self.assertRaises(ValueError):
            bundles.create(self.root, "oversized", "x" * 17000)
        self.assertFalse((self.root / "seeds/oversized").exists())
        self.assertFalse((self.root / "seed-bundles/oversized.json").exists())

    def test_clone_keeps_valid_unicode_metadata_within_byte_bound(self):
        path = self.root / "seed-bundles/original.json"
        manifest = json.loads(path.read_text())
        manifest["description"] = "é" * 4000
        path.write_text(json.dumps(manifest, ensure_ascii=False), encoding="utf-8")
        bundles.validate(self.root, "original")
        bundles.create(self.root, "copy", "Copy", "original")
        self.assertEqual(bundles.validate(self.root, "copy")[0]["description"], manifest["description"])


if __name__ == "__main__":
    unittest.main()
