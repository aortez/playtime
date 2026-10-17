"""Loopback dashboard and publishing for the local run archive."""

import json
import secrets
import shutil
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path, PurePosixPath
from urllib.parse import parse_qs, quote, unquote, urlsplit

from . import dashboard
from .common import PlaytimeError
from .jobs import RunManager, lock_held
from .live import checkpoint, events_since, model_health
from .report import CSP, available_runs, gallery, write_page


class ReportServer(ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self, runs, port=8765, *, root=None, manager=None):
        self.runs = Path(runs).resolve()
        self.runs.mkdir(parents=True, exist_ok=True)
        self.manager = manager or (RunManager(root) if root is not None else None)
        self.token = secrets.token_urlsafe(32)
        super().__init__(("127.0.0.1", port), ReportHandler)


class ReportHandler(BaseHTTPRequestHandler):
    server_version = "PlaytimeReports/1"

    def do_GET(self):
        self.respond()

    def do_HEAD(self):
        self.respond(head=True)

    def valid_host(self):
        return self.headers.get("Host", "") in {
            f"127.0.0.1:{self.server.server_port}",
            f"localhost:{self.server.server_port}",
        }

    def json_response(self, value, status=200, *, head=False):
        body = json.dumps(value, ensure_ascii=True).encode()
        self.send_headers(status, "application/json; charset=utf-8", len(body))
        if not head:
            self.wfile.write(body)

    def do_POST(self):
        if self.server.manager is None:
            self.send_error(501)
            return
        try:
            if (
                not self.valid_host()
                or self.headers.get("Origin") != "http://" + self.headers.get("Host", "")
                or not secrets.compare_digest(
                    self.headers.get("X-Playtime-Token", ""), self.server.token
                )
            ):
                self.json_response(
                    {"error": "Launch and stop require this dashboard's local origin and token"},
                    403,
                )
                return
            size = int(self.headers.get("Content-Length", "0"))
            if (
                not 0 < size <= 4096
                or self.headers.get("Transfer-Encoding")
                or self.headers.get_content_type() != "application/json"
            ):
                self.json_response({"error": "Expected a small JSON request"}, 400)
                return
            self.connection.settimeout(10)
            data = json.loads(self.rfile.read(size))
            parts = urlsplit(self.path).path.strip("/").split("/")
            manager = self.server.manager
            if (
                parts == ["api", "launch"]
                and isinstance(data, dict)
                and set(data) == {"recipe", "mode", "revision"}
            ):
                if not all(isinstance(value, str) for value in data.values()):
                    raise PlaytimeError("Invalid launch request")
                self.json_response(
                    manager.start(data["recipe"], data["mode"], data["revision"]), 201
                )
            elif (
                len(parts) == 4
                and parts[:2] == ["api", "run"]
                and parts[3] == "stop"
                and data == {}
            ):
                self.json_response(manager.stop(parts[2]), 202)
            else:
                self.json_response({"error": "Unknown action"}, 404)
        except (BrokenPipeError, ConnectionResetError):
            pass
        except (PlaytimeError, OSError, ValueError, KeyError, TypeError) as error:
            self.json_response(
                {"error": str(error)}, 409 if isinstance(error, PlaytimeError) else 400
            )

    def dashboard_response(self, path, *, head=False):
        manager = self.server.manager
        if manager is None:
            return False
        parts = path.strip("/").split("/")
        if path in {"/", "/index.html"} or (len(parts) == 2 and parts[0] == "watch"):
            run_id = parts[1] if parts[0] == "watch" else ""
            if run_id:
                manager.directory(run_id)
            body = dashboard.document(self.server.token, run_id).encode()
            self.send_headers(200, "text/html; charset=utf-8", len(body), csp=dashboard.CSP)
            if not head:
                self.wfile.write(body)
            return True
        if not path.startswith("/api/"):
            return False
        query = parse_qs(urlsplit(self.path).query)
        if path == "/api/bootstrap":
            data = {"token": self.server.token, "recipes": manager.recipes()}
        elif path == "/api/preview":
            data = manager.preview(
                query.get("recipe", [""])[0], query.get("mode", ["experiment"])[0]
            )
        elif path == "/api/runs":
            records = manager.listing()
            data = {
                "runs": records,
                "busy": any(record["active"] for record in records) or lock_held(manager.root),
            }
        elif len(parts) >= 4 and parts[:2] == ["api", "run"]:
            directory = manager.directory(parts[2])
            if len(parts) == 4 and parts[3] == "events":
                data = events_since(directory, int(query.get("cursor", ["0"])[0]))
                data["status"] = manager.status(directory)
            elif len(parts) == 4 and parts[3] == "health":
                status = manager.status(directory)
                data = (
                    {"state": "scripted", "label": "Scripted model"}
                    if status["kind"] == "scripted-smoke"
                    else model_health(status["recipe"])
                )
            elif len(parts) == 5 and parts[3] == "checkpoint":
                data = checkpoint(directory, int(parts[4]))
            else:
                self.json_response({"error": "Not found"}, 404, head=head)
                return True
        else:
            self.json_response({"error": "Not found"}, 404, head=head)
            return True
        self.json_response(data, head=head)
        return True

    def send_headers(
        self, status, content_type, length, *, attachment=None, location=None, csp=CSP
    ):
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(length))
        self.send_header("Cache-Control", "no-store")
        self.send_header("X-Content-Type-Options", "nosniff")
        self.send_header("Content-Security-Policy", csp + "; frame-ancestors 'none'")
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
            if not self.valid_host():
                self.json_response(
                    {"error": "This dashboard is local to this machine"}, 403, head=head
                )
                return
            path = unquote(urlsplit(self.path).path, errors="strict")
            parts = PurePosixPath(path).parts[1:]
            if ".." in parts or "\x00" in path or "\\" in path:
                self.send_error(404)
                return
            if self.dashboard_response(path, head=head):
                return
            if path in {"/", "/index.html", "/archive"}:
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
        except (OSError, UnicodeError, ValueError, PlaytimeError, KeyError, TypeError) as error:
            if urlsplit(self.path).path.startswith("/api/"):
                self.json_response({"error": str(error)}, 400, head=head)
            else:
                self.send_error(404)


def serve(runs, *, port=8765, root=None):
    with ReportServer(runs, port, root=root) as server:
        write_page(server.runs / "index.html", gallery(server.runs))
        actual_port = server.server_port
        print(f"Playtime dashboard: http://127.0.0.1:{actual_port}/", flush=True)
        print(f"Latest report: http://127.0.0.1:{actual_port}/latest/", flush=True)
        print("Ctrl-C stops the dashboard. Detached experiments keep running.", flush=True)
        server.serve_forever()
