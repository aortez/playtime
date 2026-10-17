"""Text-reader checks against files only; never execute workshop commands here."""

import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from workshop.tool import dispatch


class TextReaderTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.path = Path(temporary.name) / "artifact"

    def read(self, *, offset=0, limit=16384):
        return dispatch(
            {
                "name": "read_file",
                "arguments": {"path": str(self.path), "offset_bytes": offset},
                "limits": {"output_bytes": limit},
            }
        )

    def test_binary_and_non_utf8_files_return_small_metadata_only_errors(self):
        fixtures = [
            b"\x89PNG\r\n\x1a\n" + bytes(range(256)) * 256,
            b"GIF89a\x01\x00\x01\x00" + b"x" * 20000,
            b"\xff\xd8\xff\xe0" + b"x" * 20000,
            b"PK\x03\x04" + b"x" * 20000,
            b"looks like text\x00but is binary",
            b"caf\xe9",
            "UTF-16 text".encode("utf-16"),
        ]
        for data in fixtures:
            self.path.write_bytes(data)
            for offset in (0, 8, len(data) + 100):
                with self.subTest(header=data[:12], offset=offset):
                    result = self.read(offset=offset)
                    self.assertIn("UTF-8", result["error"])
                    self.assertIn("exec", result["error"])
                    self.assertEqual(result["size_bytes"], len(data))
                    self.assertNotIn("content", result)
                    self.assertLess(len(json.dumps(result).encode()), 512)

    def test_text_reads_preserve_byte_offsets_and_truncation(self):
        for text in ("", "one\ttwo\nthree\r\nfour\f", "Résumé — 日本語 🌱\n" * 400):
            data = text.encode()
            self.path.write_bytes(data)
            for offset in (0, 1, 15, 4095, 4096, 4097, len(data), len(data) + 1):
                with self.subTest(text=text[:20], offset=offset):
                    result = self.read(offset=offset, limit=17)
                    self.assertEqual(
                        result,
                        {
                            "content": data[offset : offset + 17].decode("utf-8", errors="replace"),
                            "offset_bytes": offset,
                            "truncated": len(data[offset:]) > 17,
                        },
                    )

    def test_utf8_characters_can_cross_probe_and_slice_boundaries(self):
        self.path = self.path.with_suffix(".png")  # Content, not extension, decides.
        for prefix in (4093, 4094, 4095, 4096):
            data = ("a" * prefix + "🌱" * 20).encode()
            self.path.write_bytes(data)
            for offset in range(prefix - 3, prefix + 8):
                with self.subTest(prefix=prefix, offset=offset):
                    result = self.read(offset=offset, limit=5)
                    self.assertNotIn("error", result)
                    self.assertEqual(
                        result["content"],
                        data[offset : offset + 5].decode("utf-8", errors="replace"),
                    )

    def test_binary_data_after_a_text_header_is_rejected_when_read(self):
        for suffix in (b"\x00" * 20000, b"\xff" * 20000, b"\xf0\x9f"):
            self.path.write_bytes(b"a" * 8192 + suffix)
            with self.subTest(suffix=suffix[:4]):
                self.assertIn("error", self.read(offset=8192))
                self.assertIn("error", self.read())


if __name__ == "__main__":
    unittest.main()
