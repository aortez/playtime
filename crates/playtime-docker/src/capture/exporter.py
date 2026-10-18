"""Fixed capture v1. Invoked as uid/gid 1000 with Python -I -S -B.

Only limits arrive on stdin. Never import or execute workspace contents.
"""
import json
import os
import stat
import sys
import tarfile

ROOT = "/work"


def signature(info):
    return (info.st_dev, info.st_ino, info.st_mode, info.st_size,
            info.st_mtime_ns, info.st_ctime_ns, info.st_nlink)


def unchanged(before, after):
    if signature(before) != signature(after):
        raise ValueError("workspace changed during capture")


def utf8(value, path=False):
    encoded = value.encode("utf-8", errors="strict")
    if not encoded or len(encoded) > 4096 or "\x00" in value:
        raise ValueError("unsupported name or link encoding/length")
    if path and (value.startswith("/") or len(value.split("/")) > 32
                 or any(p in ("", ".", "..") for p in value.split("/"))):
        raise ValueError("invalid logical path")
    return encoded


def main():
    limits = json.loads(sys.stdin.buffer.read(4097))
    if set(limits) != {"entries", "file_bytes", "retained_bytes", "tar_bytes"}:
        raise ValueError("invalid capture limits")
    maximum = {"entries": 4096, "file_bytes": 67108864,
               "retained_bytes": 67108864, "tar_bytes": 67108864}
    if any(type(v) is not int or v <= 0 or v > maximum[k] for k, v in limits.items()):
        raise ValueError("invalid capture limit")
    directory_flags = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_NONBLOCK
    root = os.open(ROOT, directory_flags)
    entries = {}
    discovered = 0
    root_info = os.fstat(root)

    def walk(fd, prefix):
        nonlocal discovered
        before = os.fstat(fd)
        names = []
        # scandir consumes incrementally; do not allocate an unbounded listdir.
        with os.scandir(fd) as scan:
            for entry in scan:
                name = prefix + entry.name
                utf8(name, path=True)
                discovered += 1
                if discovered > limits["entries"]:
                    raise ValueError("entry limit")
                names.append((name, entry.name))
        for name, leaf in sorted(names, key=lambda pair: pair[0].encode("utf-8")):
            info = os.stat(leaf, dir_fd=fd, follow_symlinks=False)
            if not (stat.S_ISREG(info.st_mode) or stat.S_ISDIR(info.st_mode)
                    or stat.S_ISLNK(info.st_mode)):
                raise ValueError("unsupported workspace entry type")
            if len(entries) >= limits["entries"]:
                raise ValueError("entry limit")
            entries[name] = info
            if stat.S_ISDIR(info.st_mode):
                child = os.open(leaf, directory_flags, dir_fd=fd)
                try:
                    unchanged(info, os.fstat(child))
                    walk(child, name + "/")
                    unchanged(info, os.fstat(child))
                finally:
                    os.close(child)
        unchanged(before, os.fstat(fd))

    def parent(name):
        parts = name.split("/")
        fd = os.dup(root)
        prefix = ""
        try:
            for part in parts[:-1]:
                prefix += part
                child = os.open(part, directory_flags, dir_fd=fd)
                os.close(fd)
                fd = child
                unchanged(entries[prefix], os.fstat(fd))
                prefix += "/"
            return fd, parts[-1]
        except BaseException:
            os.close(fd)
            raise

    class Output:
        total = 0

        def write(self, data):
            self.total += len(data)
            if self.total > limits["tar_bytes"]:
                raise ValueError("tar byte limit")
            sys.stdout.buffer.write(data)
            return len(data)

    try:
        walk(root, "")
        unchanged(root_info, os.fstat(root))
        inodes = {}
        retained = 0
        with tarfile.open(fileobj=Output(), mode="w|", format=tarfile.PAX_FORMAT) as archive:
            for name, before in sorted(entries.items(), key=lambda pair: pair[0].encode("utf-8")):
                fd, leaf = parent(name)
                try:
                    unchanged(before, os.stat(leaf, dir_fd=fd, follow_symlinks=False))
                    header = tarfile.TarInfo(name)
                    header.uid = header.gid = 1000
                    header.uname = header.gname = ""
                    header.mtime = 0
                    header.mode = stat.S_IMODE(before.st_mode)
                    if stat.S_ISDIR(before.st_mode):
                        header.type = tarfile.DIRTYPE
                        archive.addfile(header)
                    elif stat.S_ISLNK(before.st_mode):
                        header.type = tarfile.SYMTYPE
                        header.linkname = os.readlink(leaf, dir_fd=fd)
                        utf8(header.linkname)
                        archive.addfile(header)
                    else:
                        source = os.open(leaf, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=fd)
                        with os.fdopen(source, "rb") as body:
                            unchanged(before, os.fstat(body.fileno()))
                            key = (before.st_dev, before.st_ino)
                            if key in inodes:
                                header.type = tarfile.LNKTYPE
                                header.linkname = inodes[key]
                                archive.addfile(header)
                            else:
                                retained += before.st_size
                                if before.st_size > limits["file_bytes"] or retained > limits["retained_bytes"]:
                                    raise ValueError("logical byte limit")
                                inodes[key] = name
                                header.type = tarfile.REGTYPE
                                header.size = before.st_size
                                archive.addfile(header, body)
                            unchanged(before, os.fstat(body.fileno()))
                    unchanged(before, os.stat(leaf, dir_fd=fd, follow_symlinks=False))
                finally:
                    os.close(fd)
            # Detect mutations to entries already emitted, including additions
            # and removals via their parent directory's metadata.
            for name, before in entries.items():
                fd, leaf = parent(name)
                try:
                    unchanged(before, os.stat(leaf, dir_fd=fd, follow_symlinks=False))
                finally:
                    os.close(fd)
            unchanged(root_info, os.fstat(root))
        sys.stdout.buffer.flush()
    finally:
        os.close(root)


if __name__ == "__main__":
    try:
        main()
    except Exception:
        # Bounded diagnostics never interpolate hostile names or contents.
        sys.stderr.write("workspace capture failed (unreadable, unsupported, changed or over limit)\n")
        sys.exit(1)
