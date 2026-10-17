import base64
import http.client
import io
import json
import struct
import sys
import tarfile
import tempfile
import threading
import unittest
import zlib
from contextlib import closing
from html.parser import HTMLParser
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from playtime.report import CSP, load_timeline, publish, report
from playtime.report_previews import IMAGE_BYTES, PreviewBudget, image_preview, text_preview
from playtime.server import ReportServer


def fixture(runs, name="20261017-test"):
    directory = runs / name
    (directory / "inputs").mkdir(parents=True)
    (directory / "inputs/prompt.txt").write_text("Explore <script>window.injected = true</script>")
    (directory / "manifest.json").write_text(
        json.dumps(
            {
                "id": name,
                "kind": "fixture",
                "recipe": {"name": "Report fixture"},
            }
        )
    )
    return directory


def events(directory, records):
    lines = []
    for sequence, event in enumerate(records):
        lines.append(json.dumps({"sequence": sequence, "time": "2026-10-17T06:10:10Z", **event}))
    (directory / "events.jsonl").write_text("\n".join(lines) + "\n")


def png(rgb):
    def chunk(kind, data):
        return (
            struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))
        )

    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", 1, 1, 8, 2, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(b"\0" + bytes(rgb)))
        + chunk(b"IEND", b"")
    )


def snapshot(directory, number, files):
    folder = directory / "sessions" / f"{number:03d}"
    folder.mkdir(parents=True)
    with tarfile.open(folder / "workspace.tar", "w") as archive:
        for name, data in files.items():
            member = tarfile.TarInfo(name)
            member.size = len(data)
            archive.addfile(member, io.BytesIO(data))


class Elements(HTMLParser):
    def __init__(self, document):
        super().__init__()
        self.tags = []
        self.feed(document)

    def handle_starttag(self, tag, attrs):
        self.tags.append((tag, dict(attrs)))


class ReportTests(unittest.TestCase):
    def test_images_show_the_actual_checkpoint_versions_even_after_overwrite_or_removal(self):
        with tempfile.TemporaryDirectory() as folder:
            directory = fixture(Path(folder))
            red, blue, green = png((255, 0, 0)), png((0, 0, 255)), png((0, 255, 0))
            snapshot(directory, 1, {"./plot.png": red, "./removed.png": green})
            snapshot(directory, 2, {"./plot.png": blue})
            workspace = directory / "final-workspace"
            workspace.mkdir()
            (workspace / "plot.png").write_bytes(blue)
            events(
                directory,
                [
                    {
                        "kind": "checkpoint",
                        "session": 1,
                        "changes": {"added": ["plot.png", "removed.png"]},
                    },
                    {
                        "kind": "checkpoint",
                        "session": 2,
                        "changes": {"modified": ["plot.png"], "removed": ["removed.png"]},
                    },
                ],
            )
            document = report(directory).read_text()
            sources = [attrs["src"] for tag, attrs in Elements(document).tags if tag == "img"]
            for data, count in [(red, 1), (green, 1), (blue, 2)]:
                source = "data:image/png;base64," + base64.b64encode(data).decode()
                self.assertEqual(sources.count(source), count)
            self.assertLess(document.index('id="visual-outputs"'), document.index('id="session-1"'))
            self.assertIn("Session 1 checkpoint", document)
            self.assertIn("Last changed in session 2", document)
            self.assertIn("img-src data:", CSP)

    def test_file_previews_escape_markup_bound_text_and_skip_binary_and_links(self):
        with tempfile.TemporaryDirectory() as folder:
            directory = fixture(Path(folder))
            workspace = directory / "final-workspace"
            workspace.mkdir()
            markup = '<script>window.injected = true</script><svg onload="bad()"></svg>'
            (workspace / "page.html").write_text(markup)
            (workspace / "fake.png").write_text(markup)
            (workspace / "drawing.svg").write_text(markup)
            (workspace / "binary.dat").write_bytes(b"\x00\xff\x01")
            (workspace / "long.py").write_text("print('example')\n" * 120)
            name = "<img src=x onerror=alert(1)>.png"
            (workspace / name).write_bytes(png((0, 0, 0)))
            (workspace / "outside.png").symlink_to(workspace / name)
            (workspace / "outside-dir").symlink_to(directory / "inputs", target_is_directory=True)
            document = report(directory).read_text()
            elements = Elements(document)
            self.assertEqual(sum(tag == "script" for tag, _ in elements.tags), 1)
            self.assertEqual(sum(tag == "img" for tag, _ in elements.tags), 1)
            self.assertFalse(any(tag == "svg" for tag, _ in elements.tags))
            self.assertFalse(
                any(key.startswith("on") for _, attrs in elements.tags for key in attrs)
            )
            self.assertIn("&lt;script&gt;window.injected", document)
            self.assertIn("Excerpt shown. Download the file", document)
            self.assertIn("does not have a recognized image header", document)
            self.assertIn("binary.dat", document)
            self.assertNotIn("outside.png", document)
            self.assertNotIn("outside-dir", document)

    def test_snapshot_previews_ignore_unsafe_members_and_survive_damaged_archives(self):
        with tempfile.TemporaryDirectory() as folder:
            directory = fixture(Path(folder))
            snapshot(directory, 1, {"../escape.png": png((0, 0, 0))})
            archive_path = directory / "sessions/001/workspace.tar"
            with tarfile.open(archive_path, "a") as archive:
                for name, kind in [
                    ("symlink.png", tarfile.SYMTYPE),
                    ("hardlink.png", tarfile.LNKTYPE),
                ]:
                    member = tarfile.TarInfo(name)
                    member.type = kind
                    member.linkname = "../escape.png"
                    archive.addfile(member)
            events(
                directory,
                [
                    {
                        "kind": "checkpoint",
                        "session": 1,
                        "changes": {"added": ["../escape.png", "symlink.png", "hardlink.png"]},
                    }
                ],
            )
            document = report(directory).read_text()
            self.assertFalse(any(tag == "img" for tag, _ in Elements(document).tags))
            archive_path.write_bytes(b"broken archive")
            self.assertIn("saved snapshot could not be read", report(directory).read_text())

    def test_preview_limits_preserve_download_fallback_and_utf8_boundaries(self):
        data = png((1, 2, 3))
        budget = PreviewBudget()
        budget.images = len(data) - 1
        self.assertIn("notice", image_preview(io.BytesIO(data), "small.png", len(data), budget))
        self.assertIn(
            "notice", image_preview(io.BytesIO(data), "large.png", IMAGE_BYTES + 1, PreviewBudget())
        )
        text = "é".encode() * 10
        budget.text = 3
        preview = text_preview(io.BytesIO(text), len(text), budget)
        self.assertEqual(preview, {"text": "é", "truncated": True})
        with tempfile.TemporaryDirectory() as folder:
            directory = fixture(Path(folder))
            workspace = directory / "final-workspace"
            workspace.mkdir()
            with (workspace / "large.png").open("wb") as source:
                source.write(data)
                source.truncate(IMAGE_BYTES + 1)
            document = report(directory).read_text()
            self.assertIn("4 MiB inline preview limit", document)
            self.assertIn('href="./final-workspace/large.png" download', document)

    def test_turns_join_actions_to_results_and_preserve_reset_memories(self):
        with tempfile.TemporaryDirectory() as folder:
            directory = fixture(Path(folder))
            events(
                directory,
                [
                    {
                        "kind": "session_start",
                        "session": 1,
                        "memory": {"content": "", "status": "loaded"},
                    },
                    {
                        "kind": "model_request",
                        "session": 1,
                        "call": 1,
                        "payload": {"messages": [{}, {}]},
                    },
                    {
                        "kind": "model_response",
                        "session": 1,
                        "call": 1,
                        "response": {"eval_count": 5, "message": {"thinking": "Check the result."}},
                    },
                    {
                        "kind": "tool_request",
                        "session": 1,
                        "name": "exec",
                        "arguments": {"command": "echo first"},
                    },
                    {
                        "kind": "tool_result",
                        "session": 1,
                        "name": "exec",
                        "result": {"exit_code": 0, "stdout": "first"},
                    },
                    {
                        "kind": "tool_request",
                        "session": 1,
                        "name": "exec",
                        "arguments": {"command": "false"},
                    },
                    {
                        "kind": "tool_result",
                        "session": 1,
                        "name": "exec",
                        "result": {"exit_code": 1, "stderr": "failed"},
                    },
                    {
                        "kind": "checkpoint",
                        "session": 1,
                        "generated_tokens_total": 5,
                        "changes": {"added": ["result.txt"]},
                    },
                    {
                        "kind": "session_start",
                        "session": 2,
                        "memory": {"content": "Remember result.txt", "status": "loaded"},
                    },
                    {
                        "kind": "model_request",
                        "session": 2,
                        "call": 2,
                        "payload": {"messages": [{}, {}]},
                    },
                    {
                        "kind": "model_response",
                        "session": 2,
                        "call": 2,
                        "response": {"eval_count": 7, "message": {"content": "Continuing."}},
                    },
                    {
                        "kind": "checkpoint",
                        "session": 2,
                        "generated_tokens_total": 12,
                        "changes": {},
                    },
                ],
            )
            for number, text in [(1, "Remember result.txt"), (2, "Finished checking")]:
                path = directory / "sessions" / f"{number:03d}"
                path.mkdir(parents=True)
                (path / "memento.txt").write_text(text)
            sessions, _ = load_timeline(directory)
            tools = sessions[1]["turns"][0]["tools"]
            self.assertEqual([tool["result"]["result"]["exit_code"] for tool in tools], [0, 1])
            self.assertEqual(len(sessions[2]["turns"]), 1)
            document = publish(directory).read_text()
            self.assertIn("7 tokens this session", document)
            self.assertIn("Remember result.txt", document)
            self.assertIn("Finished checking", document)
            self.assertIn("Tool error", document)
            self.assertIn("Check the result.", document)
            self.assertTrue((directory.parent / "index.html").is_file())

    def test_partial_streams_and_output_limits_stay_visible(self):
        with tempfile.TemporaryDirectory() as folder:
            directory = fixture(Path(folder))
            events(
                directory,
                [
                    {"kind": "model_request", "session": 1, "call": 1},
                    {
                        "kind": "model_response",
                        "session": 1,
                        "call": 1,
                        "response": {"eval_count": 12, "message": {}},
                    },
                    {
                        "kind": "tool_request",
                        "session": 1,
                        "name": "exec",
                        "arguments": {"command": "slow"},
                    },
                    {
                        "kind": "tool_result",
                        "session": 1,
                        "name": "exec",
                        "result": {
                            "timed_out": True,
                            "output_truncated": True,
                            "stdout": "line\n" * 30,
                        },
                    },
                    {"kind": "model_request", "session": 1, "call": 2},
                    {
                        "kind": "model_chunk",
                        "session": 1,
                        "call": 2,
                        "chunk": {"message": {"thinking": "Still deciding"}},
                    },
                    {
                        "kind": "model_chunk",
                        "session": 1,
                        "call": 2,
                        "chunk": {"message": {"content": "Unfinished thought"}},
                    },
                ],
            )
            with (directory / "events.jsonl").open("a") as stream:
                stream.write('{"partial":')
            (directory / "result.json").write_text(
                json.dumps(
                    {
                        "stop_reason": "wall_limit",
                        "generated_tokens": 12,
                        "token_usage_complete": False,
                    }
                )
            )
            document = report(directory).read_text()
            for text in [
                "12 + unknown",
                "Still deciding",
                "Unfinished thought",
                "Incomplete response",
                "Timed out",
                "truncated its output",
                "Excerpt shown",
                "final log record is incomplete",
            ]:
                self.assertIn(text, document)

    def test_untrusted_markup_is_text_and_only_report_controls_can_execute(self):
        with tempfile.TemporaryDirectory() as folder:
            directory = fixture(Path(folder))
            malicious = "</pre><script>window.injected = true</script><img src=x onerror=alert(1)>"
            events(
                directory,
                [
                    {
                        "kind": "model_response",
                        "session": 1,
                        "call": 1,
                        "response": {"eval_count": 2, "message": {"content": malicious * 30}},
                    },
                ],
            )
            workspace = directory / "final-workspace"
            workspace.mkdir()
            (workspace / "<img src=x onerror=alert(1)>.html").write_text(malicious)
            document = report(directory).read_text()
            elements = Elements(document)
            self.assertEqual(sum(tag == "script" for tag, _ in elements.tags), 1)
            self.assertFalse(any(tag == "img" for tag, _ in elements.tags))
            self.assertFalse(
                any(key.startswith("on") for _, attrs in elements.tags for key in attrs)
            )
            self.assertIn(
                ("meta", {"http-equiv": "Content-Security-Policy", "content": CSP}), elements.tags
            )
            self.assertIn("&lt;script&gt;", document)
            self.assertIn("%3Cimg", document)


class ServerTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.runs = self.root / "runs"
        self.directory = fixture(self.runs)
        publish(self.directory)
        self.server = ReportServer(self.runs, port=0)
        self.server.RequestHandlerClass.log_message = lambda *args: None
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join()
        self.temporary.cleanup()

    def request(self, path, method="GET", headers=None):
        with closing(
            http.client.HTTPConnection("127.0.0.1", self.server.server_port, timeout=2)
        ) as connection:
            connection.request(method, path, headers=headers or {})
            response = connection.getresponse()
            return response.status, dict(response.getheaders()), response.read()

    def test_gallery_discovers_new_reports_and_latest_redirects_without_restart(self):
        status, headers, body = self.request("/")
        self.assertEqual(status, 200)
        self.assertIn(self.directory.name.encode(), body)
        self.assertIn("no-store", headers["Cache-Control"])
        status, headers, head = self.request("/", "HEAD")
        self.assertEqual(head, b"")
        self.assertEqual(int(headers["Content-Length"]), len(body))
        later = fixture(self.runs, "20261018-later")
        report(later)
        (self.runs / "broken").mkdir()
        (self.runs / "broken/report.html").write_text("partial")
        status, _, body = self.request("/")
        self.assertEqual(status, 200)
        self.assertIn(later.name.encode(), body)
        status, headers, _ = self.request("/latest/")
        self.assertEqual(status, 302)
        self.assertEqual(headers["Location"], f"/{later.name}/report.html")
        status, headers, _ = self.request(headers["Location"])
        self.assertEqual(status, 200)
        self.assertEqual(headers["Content-Type"], "text/html; charset=utf-8")

    def test_artifacts_are_downloaded_and_other_host_files_are_inaccessible(self):
        workspace = self.directory / "final-workspace"
        workspace.mkdir()
        (workspace / "report.html").write_text("<script>bad()</script>")
        outside = self.root / "secret.txt"
        outside.write_text("private")
        (workspace / "link.txt").symlink_to(outside)
        (self.runs / "outside").symlink_to(self.root, target_is_directory=True)
        prefix = "/" + self.directory.name
        status, headers, body = self.request(prefix + "/final-workspace/report.html")
        self.assertEqual(status, 200)
        self.assertTrue(headers["Content-Disposition"].startswith("attachment;"))
        self.assertEqual(headers["Content-Type"], "application/octet-stream")
        self.assertEqual(headers["X-Content-Type-Options"], "nosniff")
        for path in [
            "/../secret.txt",
            "/%2e%2e/secret.txt",
            prefix + "/../../secret.txt",
            prefix + "/final-workspace/link.txt",
            "/outside/secret.txt",
            "/.playtime/images",
            prefix + "/inputs/",
        ]:
            self.assertEqual(self.request(path)[0], 404, path)
        self.assertEqual(self.request("/", headers={"Host": "example.test"})[0], 403)
        self.assertEqual(self.request(prefix + "/report.html", method="POST")[0], 501)


if __name__ == "__main__":
    unittest.main()
