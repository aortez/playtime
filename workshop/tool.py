"""The fixed tool bridge, executed as an unprivileged user inside the workshop."""
import codecs
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


def text_sample(data, *, starts_mid_file=False, final=False):
    # A byte-offset read may begin in the middle of a UTF-8 character.
    if starts_mid_file:
        skipped = 0
        while skipped < min(3, len(data)) and 0x80 <= data[skipped] <= 0xBF:
            skipped += 1
        data = data[skipped:]
    if any((byte < 32 and byte not in b"\t\n\r\f") or byte == 127 for byte in data):
        return False
    try:
        # A bounded sample may also end in the middle of a character. At EOF,
        # however, an incomplete character is invalid UTF-8.
        codecs.getincrementaldecoder("utf-8")().decode(data, final=final)
    except UnicodeDecodeError:
        return False
    return True


def read_file(args, limits):
    path = Path(args["path"])
    if not path.is_absolute():
        path = WORK / path
    info = path.stat()
    if not stat.S_ISREG(info.st_mode):
        raise ValueError("read_file requires a regular file")
    offset = int(args.get("offset_bytes", 0))
    if offset < 0:
        raise ValueError("offset_bytes cannot be negative")
    with path.open("rb") as source:
        # Inspect the header even for offset reads, including reads past EOF.
        prefix = source.read(4096)
        start = max(0, offset - 3)
        source.seek(start)
        sample = source.read(offset - start + limits["output_bytes"] + 1)
    if not text_sample(prefix, final=len(prefix) >= info.st_size) or not text_sample(
        sample, starts_mid_file=start > 0, final=start + len(sample) >= info.st_size
    ):
        return {
            "error": "read_file only supports UTF-8 text; this file appears binary or is not "
                     "UTF-8. Use exec to inspect metadata or compute a text summary "
                     "(for images, use Pillow). This interface cannot view images.",
            "size_bytes": info.st_size,
        }
    data = sample[offset - start:]
    return {"content": data[:limits["output_bytes"]].decode("utf-8", errors="replace"),
            "offset_bytes": offset, "truncated": len(data) > limits["output_bytes"]}


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
        return read_file(args, limits)
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
