import http.client
import json
import socket
import threading
from urllib.parse import urlsplit

from .common import PlaytimeError, StopRun, canonical, digest_json

MAX_CHUNK_BYTES = 512 * 1024


class ToolCallParseError(PlaytimeError):
    """Ollama explicitly rejected a tool call before returning a complete response."""


def is_tool_parse_error(error):
    return isinstance(error, str) and error.startswith("error parsing tool call:")


class Ollama:
    def __init__(self, endpoint):
        self.url = urlsplit(endpoint)
        if (
            self.url.scheme not in {"http", "https"}
            or not self.url.hostname
            or self.url.username
            or self.url.password
            or self.url.query
            or self.url.fragment
        ):
            raise PlaytimeError(
                "Ollama endpoint must be an HTTP(S) URL without credentials or query"
            )

    def connection(self, timeout):
        cls = (
            http.client.HTTPSConnection
            if self.url.scheme == "https"
            else http.client.HTTPConnection
        )
        return cls(self.url.hostname, self.url.port, timeout=timeout)

    def request(self, path, payload=None, timeout=15):
        connection = self.connection(timeout)
        try:
            body = canonical(payload).encode() if payload is not None else None
            connection.request(
                "POST" if body is not None else "GET",
                self.url.path.rstrip("/") + path,
                body=body,
                headers={"Content-Type": "application/json"},
            )
            response = connection.getresponse()
            data = response.read(16 * 1024 * 1024 + 1)
            if response.status != 200 or len(data) > 16 * 1024 * 1024:
                raise PlaytimeError(f"Ollama {path}: HTTP {response.status}: {data[:400]!r}")
            return json.loads(data)
        except (OSError, ValueError, http.client.HTTPException) as error:
            raise PlaytimeError(f"Ollama {path}: {error}") from error
        finally:
            connection.close()

    def inspect(self, name):
        version = self.request("/api/version")["version"]
        models = self.request("/api/tags")["models"]
        model = next((model for model in models if model["name"] == name), None)
        if model is None:
            raise PlaytimeError(f"Model is not installed on this server: {name}")
        show = self.request("/api/show", {"model": name})
        if "tools" not in show.get("capabilities", []):
            raise PlaytimeError(f"Model does not advertise tool calling: {name}")
        definition = {
            key: show.get(key)
            for key in ["template", "parameters", "model_info", "details", "capabilities"]
        }
        return {
            "name": name,
            "digest": model["digest"],
            "ollama_version": version,
            "definition_sha256": digest_json(definition),
            "definition": definition,
            "modelfile": show.get("modelfile", ""),
        }

    def check(self, locked):
        current = self.inspect(locked["name"])
        for key in ["digest", "ollama_version", "definition_sha256"]:
            if current[key] != locked[key]:
                raise PlaytimeError(
                    f"Model lock mismatch for {key}; prepare a new lock deliberately"
                )
        return current

    def chat(self, payload, timeout, on_chunk):
        connection = self.connection(min(timeout, 30))
        expired = threading.Event()
        active_socket = None

        def abort():
            expired.set()
            if active_socket is not None:
                try:
                    active_socket.shutdown(socket.SHUT_RDWR)
                except OSError:
                    pass

        timer = threading.Timer(timeout, abort)
        timer.daemon = True
        timer.start()
        message = {"role": "assistant", "content": "", "thinking": "", "tool_calls": []}
        consumed = 0
        try:
            connection.connect()
            # Keep the socket reference even if HTTPConnection hands ownership
            # to a Connection: close response. The absolute deadline must also
            # interrupt a slowly arriving partial JSON line.
            active_socket = connection.sock
            if expired.is_set():
                raise StopRun("model_timeout")
            # The 30-second timeout bounds connection setup only. Tool-call
            # arguments may be buffered by Ollama while generation continues,
            # so a quiet stream must use the absolute request deadline above.
            active_socket.settimeout(None)
            connection.request(
                "POST",
                self.url.path.rstrip("/") + "/api/chat",
                body=canonical(payload).encode(),
                headers={"Content-Type": "application/json"},
            )
            response = connection.getresponse()
            if response.status != 200:
                # Ollama can reject a tool call before starting the stream.
                # Preserve the HTTP response separately from native stream chunks.
                raw = response.read(MAX_CHUNK_BYTES + 1)
                if expired.is_set():
                    raise StopRun("model_timeout")
                truncated = len(raw) > MAX_CHUNK_BYTES
                on_chunk(
                    {
                        "http_error": {
                            "status": response.status,
                            "body": raw[:MAX_CHUNK_BYTES].decode("utf-8", errors="replace"),
                            "truncated": truncated,
                        }
                    }
                )
                if truncated:
                    raise StopRun("model_response_byte_limit")
                try:
                    detail = json.loads(raw)
                except ValueError:
                    detail = None
                if isinstance(detail, dict) and is_tool_parse_error(detail.get("error")):
                    raise ToolCallParseError(detail["error"])
                raise PlaytimeError(f"Ollama chat HTTP {response.status}: {raw[:4096]!r}")
            while True:
                raw = response.readline(MAX_CHUNK_BYTES + 1)
                consumed += len(raw)
                if expired.is_set():
                    raise StopRun("model_timeout")
                if not raw:
                    raise StopRun("incomplete_model_response")
                if len(raw) > MAX_CHUNK_BYTES or consumed > 4 * 1024 * 1024:
                    raise StopRun("model_response_byte_limit")
                chunk = json.loads(raw)
                on_chunk(chunk)
                if "error" in chunk:
                    if is_tool_parse_error(chunk["error"]):
                        raise ToolCallParseError(chunk["error"])
                    raise PlaytimeError(f"Ollama stream error: {chunk['error']}")
                part = chunk.get("message", {})
                message["content"] += part.get("content", "")
                message["thinking"] += part.get("thinking", "")
                message["tool_calls"].extend(part.get("tool_calls", []))
                if chunk.get("done"):
                    if type(chunk.get("eval_count")) is not int or chunk["eval_count"] < 0:
                        raise StopRun("missing_token_usage")
                    return {**chunk, "message": message}
        except (OSError, ValueError, http.client.HTTPException) as error:
            raise StopRun(
                "model_timeout" if expired.is_set() else "model_connection_error"
            ) from error
        finally:
            timer.cancel()
            connection.close()
