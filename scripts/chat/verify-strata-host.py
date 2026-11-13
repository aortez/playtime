#!/usr/bin/env python3
"""Read a held Strata helper session and hash its actual process and artifacts.

Sent over SSH on stdin. No model startup, settings changes, or persistent cache.
The local owner compares this report with a separately reviewed model lock.
"""

import hashlib
import json
from pathlib import Path
import sys


def digest_json(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(",", ":")).encode()).hexdigest()


def hash_file(path):
    before = path.stat()
    with path.open("rb") as source:
        digest = hashlib.file_digest(source, "sha256").hexdigest()
    after = path.stat()
    identity = lambda s: (s.st_dev, s.st_ino, s.st_size, s.st_mtime_ns, s.st_ctime_ns)
    if identity(before) != identity(after):
        raise RuntimeError(f"artifact changed while hashing: {path}")
    return {"path": str(path), "size": after.st_size, "sha256": digest}


def verify(root):
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
    artifacts = [hash_file(path) for path in sorted(sources)]
    proc = Path("/proc") / str(process["pid"])
    environment = proc.joinpath("environ").read_bytes().decode().split("\0")
    environment = dict(value.split("=", 1) for value in environment
                       if "=" in value and value.startswith(("STRATA_", "CUDA_", "HIP_", "HSA_", "ROCR_")))
    launch = {"config": config, "argv": argv, "environment": environment}
    host = {
        "backend": backend,
        "quantization": "TQ2_0",
        "engine_sha256": hash_file(proc / "exe")["sha256"],
        "artifacts_manifest_sha256": digest_json(artifacts),
        "launch_configuration_sha256": digest_json(launch),
        "responses_source_sha256": hash_file(Path(config["cwd"]) / "serve/responses.py")["sha256"],
    }
    # Reject replacement or exit during hashing rather than handing off stale evidence.
    current = [v.decode() for v in proc.joinpath("cmdline").read_bytes().split(b"\0") if v]
    if current != argv:
        raise RuntimeError("engine changed while verifying")
    return {"host": host, "artifacts": artifacts, "launch": launch, "process": process}


if __name__ == "__main__":
    print(json.dumps(verify(Path(sys.argv[1])), indent=2))
