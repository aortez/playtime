#!/usr/bin/env python3
"""Read a held Strata helper session and hash its actual process and artifacts.

Sent over SSH on stdin. Unchanged model artifacts may reuse private cached hashes;
the engine and Responses source are always hashed. Both ends check the reviewed
host identity before handoff. This script never starts or changes the model.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import tempfile


def digest_json(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(",", ":")).encode()).hexdigest()


def identity(info):
    return [info.st_dev, info.st_ino, info.st_size, info.st_mtime_ns, info.st_ctime_ns]


class ArtifactHashes:
    """Metadata cache for trusted local files, not a fresh content-integrity proof.

    Only a report matching the reviewed lock can publish entries. Cache metadata
    is separate from the artifact manifest so existing model locks stay valid.
    """

    def __init__(self, directory, expected_host, full=False):
        self.expected_host = expected_host
        self.full = full
        self.path = directory / f"strata-hashes-v1-{digest_json(expected_host)}.json"
        self.cached = {}
        self.entries = {}
        self.observed = {}
        self.stats = {"mode": "full" if full else "reuse_unchanged", "reused_files": 0,
                      "reused_bytes": 0, "hashed_files": 0, "hashed_bytes": 0,
                      "cache_saved": False}
        try:
            directory.mkdir(mode=0o700, parents=True, exist_ok=True)
            info = directory.lstat()
            if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
                raise ValueError("cache directory must be private and caller-owned")
        except (OSError, ValueError) as error:
            self.stats["cache_note"] = str(error)
            self.path = None
        if full or self.path is None:
            return
        try:
            fd = os.open(self.path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
            with os.fdopen(fd, "r") as source:
                info = os.fstat(source.fileno())
                if (not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid()
                        or info.st_mode & 0o077 or info.st_size > 1024 * 1024):
                    raise ValueError("cache file must be private, caller-owned and bounded")
                cache = json.loads(source.read(1024 * 1024 + 1))
            if (cache["schema_version"] != 1
                    or cache["host_digest"] != digest_json(expected_host)
                    or not isinstance(cache["entries"], dict)):
                raise ValueError("cache format or reviewed identity changed")
            self.cached = cache["entries"]
        except FileNotFoundError:
            pass
        except (OSError, ValueError, KeyError, TypeError) as error:
            self.stats["cache_note"] = f"ignoring cache: {error}"

    def hash_file(self, path, reusable=False):
        # Compare the opened file with the name, not just two pathname stats.
        # ctime detects a same-size rewrite even if mtime was restored.
        before = identity(path.stat())
        with path.open("rb") as source:
            info = os.fstat(source.fileno())
            if not stat.S_ISREG(info.st_mode) or identity(info) != before:
                raise RuntimeError(f"artifact changed before hashing: {path}")
            previous = self.cached.get(str(path)) if reusable else None
            if (isinstance(previous, dict) and previous.get("identity") == before
                    and isinstance(previous.get("sha256"), str)
                    and re.fullmatch(r"[0-9a-f]{64}", previous["sha256"])):
                digest = previous["sha256"]
                method = "reused"
            else:
                digest = hashlib.file_digest(source, "sha256").hexdigest()
                method = "hashed"
            if identity(os.fstat(source.fileno())) != before or identity(path.stat()) != before:
                raise RuntimeError(f"artifact changed while verifying: {path}")
        self.observed[path] = before
        self.stats[f"{method}_files"] += 1
        self.stats[f"{method}_bytes"] += info.st_size
        if reusable:
            self.entries[str(path)] = {"identity": before, "sha256": digest}
        return {"path": str(path), "size": info.st_size, "sha256": digest}

    def finish(self, host):
        if host != self.expected_host:
            raise RuntimeError("running host artifacts/settings differ from the reviewed model lock")
        # Also catch a change to an early file while later files were checked.
        for path, before in self.observed.items():
            if identity(path.stat()) != before:
                raise RuntimeError(f"artifact changed during verification: {path}")
        if self.path is None:
            return
        temporary = None
        try:
            with tempfile.NamedTemporaryFile(mode="w", dir=self.path.parent,
                                             prefix=".strata-hashes-", delete=False) as output:
                temporary = Path(output.name)
                json.dump({"schema_version": 1, "host_digest": digest_json(host),
                           "entries": self.entries}, output)
                output.flush()
                os.fsync(output.fileno())
            os.replace(temporary, self.path)
            self.stats["cache_saved"] = True
        except OSError as error:
            self.stats["cache_note"] = f"cache not saved: {error}"
        finally:
            if temporary is not None:
                temporary.unlink(missing_ok=True)


def verify(root, expected_host, full=False):
    hashes = ArtifactHashes(Path.home() / ".cache/playtime-chat", expected_host, full)
    config = json.loads((root / "engine-config.json").read_text())
    native = config["args"][config["args"].index("--native") + 1]
    processes = []
    for path in Path("/proc").glob("[0-9]*/cmdline"):
        try:
            argv = [v.decode() for v in path.read_bytes().split(b"\0") if v]
            if (argv and argv[0].startswith(str(Path.home() / ".local/share/strata/components/engine/"))
                    and "--serve" in argv and "--native" in argv
                    and argv[argv.index("--native") + 1] == native):
                processes.append({"pid": int(path.parent.name), "argv": argv})
        except (OSError, UnicodeDecodeError):
            continue
    if len(processes) != 1:
        raise RuntimeError("expected exactly one matching owned Strata engine")
    process = processes[0]
    argv = process["argv"]
    backend = "cuda" if "-cuda-" in argv[0] else "hip" if "-hip-" in argv[0] else None
    if backend is None or config.get("backend", backend) != backend:
        raise RuntimeError("unrecognized Strata backend")
    if (config["drafting"] != "dflash2" or argv[argv.index("--draft") + 1] != "dflash2"
            or argv[argv.index("--draft-fallback") + 1] != "none" or "--mtp" in argv):
        raise RuntimeError("engine does not use the reviewed fast drafting settings")
    sources = set()
    for flag in ("--native", "--ple-gguf", "--pack", "--expert-profile", "--draft-model", "--mtp-draft-vocab"):
        if flag in argv:
            path = Path(argv[argv.index(flag) + 1])
            if not path.exists():
                raise RuntimeError(f"missing artifact: {path}")
            sources.update([path] if path.is_file() else (p for p in path.rglob("*") if p.is_file()))
    artifacts = [hashes.hash_file(path, reusable=True) for path in sorted(sources)]
    proc = Path("/proc") / str(process["pid"])
    environment = proc.joinpath("environ").read_bytes().decode().split("\0")
    environment = dict(value.split("=", 1) for value in environment
                       if "=" in value and value.startswith(("STRATA_", "CUDA_", "HIP_", "HSA_", "ROCR_")))
    launch = {"config": config, "argv": argv, "environment": environment}
    host = {
        "backend": backend,
        "quantization": "TQ2_0",
        "engine_sha256": hashes.hash_file(proc / "exe")["sha256"],
        "artifacts_manifest_sha256": digest_json(artifacts),
        "launch_configuration_sha256": digest_json(launch),
        "responses_source_sha256": hashes.hash_file(Path(config["cwd"]) / "serve/responses.py")["sha256"],
    }
    # Reject replacement or exit during hashing rather than handing off stale evidence.
    current = [v.decode() for v in proc.joinpath("cmdline").read_bytes().split(b"\0") if v]
    if current != argv:
        raise RuntimeError("engine changed while verifying")
    hashes.finish(host)
    return {"host": host, "artifacts": artifacts, "launch": launch, "process": process,
            "verification": hashes.stats}


if __name__ == "__main__":
    os.umask(0o077)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("session", type=Path)
    parser.add_argument("--expected-host", type=json.loads, required=True)
    parser.add_argument("--full-verify", action="store_true")
    args = parser.parse_args()
    print(json.dumps(verify(args.session, args.expected_host, args.full_verify), indent=2))
