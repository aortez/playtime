"""Isolated uid-1000 bounded outgoing memory read. No workspace imports."""
import codecs
import json
import os
import stat
import sys

ROOT = "/work"


def main():
    limit = json.loads(sys.stdin.buffer.read(128))
    if type(limit) is not int or not 0 < limit <= 65536:
        raise ValueError("invalid memory limit")
    result = {"status": "unreadable", "content": ""}
    root = os.open(ROOT, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_NONBLOCK)
    try:
        try:
            before = os.stat("MEMENTO.md", dir_fd=root, follow_symlinks=False)
            if not stat.S_ISREG(before.st_mode):
                raise ValueError("not a regular memento")
            fd = os.open("MEMENTO.md", os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=root)
            with os.fdopen(fd, "rb") as source:
                actual = os.fstat(source.fileno())
                signature = lambda s: (s.st_dev, s.st_ino, s.st_mode, s.st_size, s.st_mtime_ns, s.st_ctime_ns)
                if signature(actual) != signature(before):
                    raise RuntimeError("memory changed")
                prefix = source.read(4096)
                source.seek(0)
                data = source.read(limit + 1)
                if signature(os.fstat(source.fileno())) != signature(before):
                    raise RuntimeError("memory changed")
            if signature(os.stat("MEMENTO.md", dir_fd=root, follow_symlinks=False)) != signature(before):
                raise RuntimeError("memory changed")
            def text_sample(sample):
                if any((c < 32 and c not in b"\t\n\r\f") or c == 127 for c in sample):
                    return False
                try:
                    codecs.getincrementaldecoder("utf-8")().decode(sample, final=len(sample) >= before.st_size)
                    return True
                except UnicodeError:
                    return False
            # Match the existing bridge: binary header/sample takes precedence
            # over truncation; a partial UTF-8 suffix at a sample edge is valid.
            if not text_sample(prefix) or not text_sample(data):
                result["status"] = "binary"
            elif len(data) > limit:
                result["status"] = "oversized"
            else:
                result = {"status": "loaded", "content": data.decode("utf-8", errors="strict")}
        except FileNotFoundError:
            result["status"] = "missing"
        except (PermissionError, ValueError):
            result["status"] = "unreadable"
        print(json.dumps(result))
    finally:
        os.close(root)


if __name__ == "__main__":
    try:
        main()
    except Exception:
        sys.stderr.write("outgoing memory read failed\n")
        sys.exit(1)
