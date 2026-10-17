"""The fixed tool bridge, executed as an unprivileged user inside the workshop."""
import json
import os
import selectors
import signal
import stat
import subprocess
import sys
import time
from pathlib import Path

WORK = Path("/work")


def work_path(value):
    path = Path(value)
    if not path.is_absolute():
        path = WORK / path
    path = path.resolve()
    if path != WORK and WORK not in path.parents:
        raise ValueError("Writable paths and working directories must be inside /work")
    return path


def execute(args, limits):
    command = args["command"]
    if not isinstance(command, str) or len(command.encode()) > 65536:
        raise ValueError("command must be a string of at most 65536 bytes")
    cwd = work_path(args.get("cwd", "/work"))
    seconds = min(float(args.get("timeout_seconds", limits["command_seconds"])),
                  limits["command_seconds"])
    if seconds <= 0:
        raise ValueError("timeout_seconds must be positive")
    start = time.monotonic()
    process = subprocess.Popen(["/bin/bash", "--noprofile", "--norc", "-c", command],
                               cwd=cwd, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                               stdin=subprocess.DEVNULL, start_new_session=True)
    chunks = {"stdout": bytearray(), "stderr": bytearray()}
    kept = 0
    truncated = False
    timed_out = False
    selector = selectors.DefaultSelector()
    for stream, name in [(process.stdout, "stdout"), (process.stderr, "stderr")]:
        os.set_blocking(stream.fileno(), False)
        selector.register(stream, selectors.EVENT_READ, name)
    try:
        while selector.get_map():
            if time.monotonic() - start >= seconds:
                timed_out = True
                break
            for key, _ in selector.select(min(0.1, seconds)):
                data = os.read(key.fd, 65536)
                if not data:
                    selector.unregister(key.fileobj)
                    continue
                available = max(0, limits["output_bytes"] - kept)
                chunks[key.data].extend(data[:available])
                kept += min(available, len(data))
                truncated |= len(data) > available
        if timed_out:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        remaining = max(0.01, seconds - (time.monotonic() - start))
        try:
            process.wait(timeout=remaining)
        except subprocess.TimeoutExpired:
            timed_out = True
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            process.wait(timeout=2)
    finally:
        selector.close()
        process.stdout.close()
        process.stderr.close()
    return {name: bytes(value).decode("utf-8", errors="replace") for name, value in chunks.items()} | {
        "exit_code": process.returncode,
        "timed_out": timed_out,
        "output_truncated": truncated,
        "duration_seconds": round(time.monotonic() - start, 3),
    }


def dispatch(request):
    args = request["arguments"]
    limits = request["limits"]
    operation = request["name"]
    if operation == "exec":
        return execute(args, limits)
    if operation == "read_file":
        path = Path(args["path"])
        if not path.is_absolute():
            path = WORK / path
        if not stat.S_ISREG(path.stat().st_mode):
            raise ValueError("read_file requires a regular file")
        offset = int(args.get("offset_bytes", 0))
        if offset < 0:
            raise ValueError("offset_bytes cannot be negative")
        with path.open("rb") as source:
            source.seek(offset)
            data = source.read(limits["output_bytes"] + 1)
        return {"content": data[:limits["output_bytes"]].decode("utf-8", errors="replace"),
                "offset_bytes": offset, "truncated": len(data) > limits["output_bytes"]}
    if operation == "write_file":
        path = work_path(args["path"])
        content = args["content"].encode("utf-8")
        if len(content) > 131072:
            raise ValueError("write_file is limited to 131072 UTF-8 bytes per call")
        path.parent.mkdir(parents=True, exist_ok=True)
        # Resolve again after mkdir; symlinks cannot redirect writes outside /work.
        path = work_path(str(path))
        with path.open("ab" if args.get("append", False) else "wb") as dest:
            dest.write(content)
        return {"path": str(path), "bytes_written": len(content)}
    raise ValueError("Unknown tool: " + operation)


if __name__ == "__main__":
    try:
        request = json.loads(sys.stdin.buffer.read(262145))
        result = dispatch(request)
    except Exception as error:
        result = {"error": f"{type(error).__name__}: {error}"}
    print(json.dumps(result, ensure_ascii=True))
