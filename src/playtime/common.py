import contextlib
import hashlib
import json
import os
import selectors
import signal
import subprocess
import tempfile
import time
from pathlib import Path


class PlaytimeError(Exception):
    pass


class StopRun(PlaytimeError):
    """A deliberate stop with a machine-readable reason."""


def timestamp():
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())


def digest_file(path):
    with Path(path).open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=True)


def digest_json(value):
    return hashlib.sha256(canonical(value).encode()).hexdigest()


def write_json(path, value):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")
    temporary.replace(path)


def tree_manifest(root):
    root = Path(root)
    result = {}
    for path in sorted(root.rglob("*")):
        if "__pycache__" in path.parts:
            continue
        if path.is_symlink():
            raise PlaytimeError(f"Input symlinks are not supported: {path}")
        if path.is_file():
            result[path.relative_to(root).as_posix()] = digest_file(path)
    return result


def controller_manifest(root):
    root = Path(root)
    result = {"src/" + name: sha for name, sha in tree_manifest(root / "src").items()}
    for name in ["playtime", "pyproject.toml"]:
        result[name] = digest_file(root / name)
    return result


def command(argv, *, timeout=30, input_bytes=None, output_path=None, max_bytes=4 * 1024 * 1024):
    """Bound subprocess duration and output; never interpret host shell text."""
    with contextlib.ExitStack() as stack:
        source = subprocess.DEVNULL
        if input_bytes is not None:
            source = stack.enter_context(tempfile.TemporaryFile())
            source.write(input_bytes)
            source.seek(0)
        destination = stack.enter_context(Path(output_path).open("wb")) if output_path else None
        process = subprocess.Popen(
            [str(arg) for arg in argv],
            stdin=source,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            start_new_session=True,
        )
        selector = selectors.DefaultSelector()
        for stream, name in [(process.stdout, "stdout"), (process.stderr, "stderr")]:
            os.set_blocking(stream.fileno(), False)
            selector.register(stream, selectors.EVENT_READ, name)
        captured = {"stdout": bytearray(), "stderr": bytearray()}
        total = 0
        deadline = time.monotonic() + timeout
        try:
            while selector.get_map():
                if time.monotonic() >= deadline:
                    raise PlaytimeError(f"Command timed out: {argv[0]}")
                for key, _ in selector.select(min(0.1, max(0, deadline - time.monotonic()))):
                    data = os.read(key.fd, 65536)
                    if not data:
                        selector.unregister(key.fileobj)
                        continue
                    total += len(data)
                    if total > max_bytes:
                        raise PlaytimeError(f"Command output exceeded {max_bytes} bytes: {argv[0]}")
                    if destination and key.data == "stdout":
                        destination.write(data)
                    else:
                        captured[key.data].extend(data)
            process.wait(timeout=max(0.01, deadline - time.monotonic()))
            if process.returncode:
                detail = captured["stderr"].decode(errors="replace")[-4000:]
                raise PlaytimeError(f"Command failed ({process.returncode}): {argv[0]}: {detail}")
            return bytes(captured["stdout"])
        finally:
            if process.poll() is None:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait()
            selector.close()
            process.stdout.close()
            process.stderr.close()


class EventLog:
    def __init__(self, path, max_bytes):
        self.file = Path(path).open("x", encoding="utf-8")
        self.max_bytes = max_bytes
        self.used = 0
        self.sequence = 0

    def emit(self, kind, *, final=False, **data):
        event = {"sequence": self.sequence, "time": timestamp(), "kind": kind, **data}
        encoded = canonical(event) + "\n"
        size = len(encoded.encode())
        # Leave room for the stop record even when a model exhausts the log budget.
        ceiling = self.max_bytes if final else self.max_bytes - 65536
        if self.used + size > ceiling:
            raise StopRun("log_limit")
        self.file.write(encoded)
        self.file.flush()
        self.used += size
        self.sequence += 1

    def close(self):
        self.file.close()
