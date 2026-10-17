"""Read-only, loopback-only publishing for the local run archive."""

import shutil
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path, PurePosixPath
from urllib.parse import quote, unquote, urlsplit

from .report import CSP, available_runs, gallery, write_page


class ReportServer(ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self, runs, port=8765):
        self.runs = Path(runs).resolve()
        self.runs.mkdir(parents=True, exist_ok=True)
        super().__init__(("127.0.0.1", port), ReportHandler)


class ReportHandler(BaseHTTPRequestHandler):
    server_version = "PlaytimeReports/1"

    def do_GET(self):
        self.respond()

    def do_HEAD(self):
        self.respond(head=True)

    def send_headers(self, status, content_type, length, *, attachment=None, location=None):
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(length))
        self.send_header("Cache-Control", "no-store")
        self.send_header("X-Content-Type-Options", "nosniff")
        self.send_header("Content-Security-Policy", CSP + "; frame-ancestors 'none'")
        self.send_header("Referrer-Policy", "no-referrer")
        if attachment:
            self.send_header(
                "Content-Disposition", "attachment; filename*=UTF-8''" + quote(attachment, safe="")
            )
        if location:
            self.send_header("Location", location)
        self.end_headers()

    def respond(self, *, head=False):
        try:
            host = self.headers.get("Host", "").split(":", 1)[0].lower()
            if host not in {"localhost", "127.0.0.1"}:
                self.send_error(403, "This gallery is local to this machine")
                return
            path = unquote(urlsplit(self.path).path, errors="strict")
            parts = PurePosixPath(path).parts[1:]
            if ".." in parts or "\x00" in path or "\\" in path:
                self.send_error(404)
                return
            if path in {"/", "/index.html"}:
                body = gallery(self.server.runs).encode("utf-8")
                self.send_headers(200, "text/html; charset=utf-8", len(body))
                if not head:
                    self.wfile.write(body)
                return
            if path in {"/latest", "/latest/"}:
                records = available_runs(self.server.runs)
                if not records:
                    self.send_error(404, "No reports yet")
                    return
                self.send_headers(
                    302,
                    "text/plain",
                    0,
                    location="/" + quote(records[0][0].name, safe="") + "/report.html",
                )
                return
            if not parts or not (self.server.runs / parts[0] / "manifest.json").is_file():
                self.send_error(404)
                return
            if len(parts) == 1:
                self.send_headers(
                    302, "text/plain", 0, location="/" + quote(parts[0], safe="") + "/report.html"
                )
                return
            target = self.server.runs
            for part in parts:
                target = target / part
                if target.is_symlink():
                    self.send_error(404)
                    return
            if not target.resolve().is_relative_to(self.server.runs) or not target.is_file():
                self.send_error(404)
                return
            trusted_report = len(parts) == 2 and parts[1] == "report.html"
            download = "final-workspace" in parts or target.suffix not in {
                ".json",
                ".jsonl",
                ".txt",
                ".md",
                ".py",
                ".toml",
            }
            content_type = (
                "text/html; charset=utf-8"
                if trusted_report
                else ("application/octet-stream" if download else "text/plain; charset=utf-8")
            )
            # Only controller-generated reports are rendered as HTML. Artifacts
            # always download, even when a caller bypasses the report's links.
            with target.open("rb") as source:
                self.send_headers(
                    200,
                    content_type,
                    target.stat().st_size,
                    attachment=target.name if download and not trusted_report else None,
                )
                if not head:
                    shutil.copyfileobj(source, self.wfile)
        except (BrokenPipeError, ConnectionResetError):
            pass
        except (OSError, UnicodeError, ValueError):
            self.send_error(404)


def serve(runs, *, port=8765):
    with ReportServer(runs, port) as server:
        write_page(server.runs / "index.html", gallery(server.runs))
        actual_port = server.server_port
        print(f"Report gallery: http://127.0.0.1:{actual_port}/", flush=True)
        print(f"Latest report: http://127.0.0.1:{actual_port}/latest/", flush=True)
        print("New completed reports appear automatically. Ctrl-C stops the server.", flush=True)
        server.serve_forever()
