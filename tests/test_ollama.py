import json
import sys
import threading
import time
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from playtime.common import PlaytimeError, StopRun
from playtime.ollama import Ollama, ToolCallParseError


class StreamingTests(unittest.TestCase):
    def serve(self, responder):
        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def do_POST(self):
                self.rfile.read(int(self.headers["Content-Length"]))
                self.send_response(200)
                self.send_header("Content-Type", "application/x-ndjson")
                self.send_header("Connection", "close")
                self.end_headers()
                try:
                    responder(self.wfile)
                except (BrokenPipeError, ConnectionResetError):
                    pass
                self.close_connection = True

            def log_message(self, *args):
                pass

        server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        server.daemon_threads = True
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        return Ollama(f"http://127.0.0.1:{server.server_port}")

    def test_stream_preserves_thinking_tools_and_usage(self):
        chunks = [
            {"message": {"thinking": "considering"}},
            {
                "message": {
                    "tool_calls": [{"function": {"name": "exec", "arguments": {"command": "pwd"}}}]
                }
            },
            {"message": {"content": "done"}, "done": True, "eval_count": 12},
        ]

        def respond(output):
            for chunk in chunks:
                output.write((json.dumps(chunk) + "\n").encode())
                output.flush()

        captured = []
        result = self.serve(respond).chat({"messages": []}, 2, captured.append)
        self.assertEqual(result["message"]["thinking"], "considering")
        self.assertEqual(result["message"]["content"], "done")
        self.assertEqual(result["message"]["tool_calls"][0]["function"]["name"], "exec")
        self.assertEqual(result["eval_count"], 12)
        self.assertEqual(captured, chunks)

    def test_incomplete_stream_does_not_report_zero_usage_as_success(self):
        def respond(output):
            output.write(b'{"message":{"thinking":"partial"}}\n')
            output.flush()

        with self.assertRaisesRegex(StopRun, "incomplete_model_response"):
            self.serve(respond).chat({}, 2, lambda chunk: None)

    def test_only_explicit_tool_parse_errors_are_recoverable_and_chunks_are_preserved(self):
        for error in [
            "error parsing tool call: invalid character ']'",
            "model runner out of memory",
        ]:
            with self.subTest(error=error):
                chunks = [{"message": {"thinking": "partial"}}, {"error": error}]

                def respond(output, chunks=chunks):
                    for chunk in chunks:
                        output.write((json.dumps(chunk) + "\n").encode())
                        output.flush()

                captured = []
                with self.assertRaises(PlaytimeError) as raised:
                    self.serve(respond).chat({}, 2, captured.append)
                self.assertEqual(
                    isinstance(raised.exception, ToolCallParseError),
                    error.startswith("error parsing tool call:"),
                )
                self.assertEqual(captured, chunks)

    def test_wall_deadline_interrupts_a_stalled_stream(self):
        def respond(output):
            output.write(b'{"message":{"thinking":"partial"}}\n')
            output.flush()
            time.sleep(2)

        client = self.serve(respond)
        started = time.monotonic()
        with self.assertRaisesRegex(StopRun, "model_timeout"):
            client.chat({}, 0.2, lambda chunk: None)
        self.assertLess(time.monotonic() - started, 1)

    def test_deadline_interrupts_a_dribbling_partial_line(self):
        def respond(output):
            for _ in range(50):
                output.write(b" ")
                output.flush()
                time.sleep(0.04)

        client = self.serve(respond)
        started = time.monotonic()
        with self.assertRaisesRegex(StopRun, "model_timeout"):
            client.chat({}, 0.2, lambda chunk: None)
        self.assertLess(time.monotonic() - started, 1)


if __name__ == "__main__":
    unittest.main()
