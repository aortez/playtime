#!/usr/bin/env python3
"""Read a held inference session and hash its actual process and artifacts.

Sent on stdin over SSH to a Strata or Basalt helper's host, or run beside a local
llama-server owner. Unchanged model artifacts may reuse private cached hashes;
engines, serving sources, server releases and build records are always read.
Both ends check the reviewed host identity before handoff. This script never
starts or changes the model.
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


def verify_basalt(root, expected_host, full=False):
    """Attest the API process, its CUDA child, complete pack and serving code."""
    session = json.loads((root / "session.json").read_text())
    config_path = Path(session["config_path"])
    config = json.loads(config_path.read_text())
    model = config["models"][0]
    release = Path.home() / ".local/share/basalt/0.4.1/basalt-v0.4.1-linux-x64"
    proc = Path("/proc") / str(session["launcher_pid"])
    def argv(path):
        return [s.decode() for s in (path / "cmdline").read_bytes().split(b"\0") if s]
    expected_argv = [str(release / ".venv/bin/python"), "-m", "serve.server", "--engine-bin", str(release / "bin/basalt"), "--config", str(config_path)]
    actual = argv(proc)
    if actual != expected_argv or proc.joinpath("cwd").resolve() != release:
        raise RuntimeError("owned Basalt API process differs from its recorded configuration")
    started = proc.joinpath("stat").read_text().rsplit(")", 1)[1].split()[19]
    children = []
    for item in Path("/proc").glob("[0-9]*"):
        try:
            command = argv(item)
            if command[:2] == [str(release / "bin/basalt"), "--serve"]:
                parent = int(item.joinpath("stat").read_text().rsplit(")", 1)[1].split()[1])
                if parent == session["launcher_pid"]:
                    children.append((item, command))
        except (OSError, ValueError, UnicodeError):
            continue
    if len(children) != 1:
        raise RuntimeError("expected one CUDA child of the owned Basalt API process")
    engine, command = children[0]
    if command[command.index("--model") + 1] != model["model"]:
        raise RuntimeError("Basalt CUDA child selected a different model")
    environment = dict(v.split("=", 1) for v in proc.joinpath("environ").read_bytes().decode().split("\0")
                       if "=" in v and v.startswith(("CUDA_", "LD_LIBRARY_PATH=")))
    if environment.get("CUDA_VISIBLE_DEVICES") != session["gpu_uuid"]:
        raise RuntimeError("Basalt selected a different GPU")
    hashes = ArtifactHashes(Path.home() / ".cache/playtime-chat", expected_host or {}, full or expected_host is None)
    sources = [Path(model["model"]), release / "requirements.txt"]
    sources += sorted(p for directory in (release / "serve", release / "tools") for p in directory.rglob("*")
                      if p.is_file() and p.suffix in (".py", ".jinja"))
    artifacts = [hashes.hash_file(p, reusable=p.suffix == ".basalt") for p in sorted(sources)]
    launch = {"config": config, "argv": actual, "engine_argv": command, "environment": environment}
    quant = json.loads((config_path.parent.parent / "profiles.json").read_text())["profiles"][session["profile"]]["quantization"]
    host = {"backend": "cuda", "quantization": quant,
            "engine_sha256": hashes.hash_file(engine / "exe")["sha256"],
            "responses_source_sha256": hashes.hash_file(release / "serve/api/responses.py")["sha256"],
            "artifacts_manifest_sha256": digest_json(artifacts),
            "launch_configuration_sha256": digest_json(launch)}
    if (argv(proc) != actual or argv(engine) != command
            or proc.joinpath("stat").read_text().rsplit(")", 1)[1].split()[19] != started
            or json.loads(config_path.read_text()) != config):
        raise RuntimeError("Basalt process/configuration changed during verification")
    if expected_host is not None:
        hashes.finish(host)
    else:
        for path, before in hashes.observed.items():
            if identity(path.stat()) != before:
                raise RuntimeError("artifact changed during discovery")
    return {"host": host, "artifacts": artifacts, "launch": launch,
            "process": {"pid": session["launcher_pid"], "engine_pid": int(engine.name), "start_ticks": started},
            "verification": hashes.stats}


def release_manifest(release, hashes):
    """Every file of the Strata server release, by its path inside the release.

    Symlinks are recorded as links, not followed. Python writes its bytecode
    caches at runtime, so `__pycache__` directories are left out, and a stale or
    altered cache there isn't detected.
    """
    entries = []
    for directory, subdirectories, files in os.walk(release):
        base = Path(directory)
        subdirectories[:] = [name for name in subdirectories if name != "__pycache__"]
        for name in [*subdirectories, *files]:
            path = base / name
            info = path.lstat()
            relative = str(path.relative_to(release))
            if stat.S_ISLNK(info.st_mode):
                entries.append({"path": relative, "link": os.readlink(path)})
            elif stat.S_ISREG(info.st_mode):
                record = hashes.hash_file(path)
                # hash_file follows links; the bytes read must be this regular file's.
                if hashes.observed[path] != identity(info):
                    raise RuntimeError(f"server release changed while verifying: {relative}")
                entries.append({"path": relative, "size": record["size"], "sha256": record["sha256"]})
            elif not stat.S_ISDIR(info.st_mode):
                raise RuntimeError(f"unexpected file type in the server release: {relative}")
    return sorted(entries, key=lambda entry: entry["path"])


# Reviewed `general.file_type` values from llama.cpp's llama.h.
GGUF_FILE_TYPES = {0: "F32", 1: "F16", 2: "Q4_0", 7: "Q8_0", 14: "Q4_K_S", 15: "Q4_K_M",
                   17: "Q5_K_M", 18: "Q6_K", 32: "BF16"}
# Byte widths of GGUF's fixed-size value types; 8 is a string and 9 an array.
GGUF_WIDTHS = {0: 1, 1: 1, 2: 2, 3: 2, 4: 4, 5: 4, 6: 4, 7: 1, 10: 8, 11: 8, 12: 8}
GGUF_HEADER_LIMIT = 256 << 20


def gguf_quantization(path):
    """The quantization that a GGUF v3 file's `general.file_type` header names."""
    with path.open("rb") as source:
        def number(size):
            data = source.read(size)
            if len(data) != size or source.tell() > GGUF_HEADER_LIMIT:
                raise RuntimeError(f"unreadable GGUF header: {path}")
            return int.from_bytes(data, "little")

        def skip(kind):
            if kind == 8:
                source.seek(number(8), os.SEEK_CUR)
            elif kind == 9:
                inner, count = number(4), number(8)
                if inner in GGUF_WIDTHS:
                    source.seek(GGUF_WIDTHS[inner] * count, os.SEEK_CUR)
                else:
                    for _ in range(count):
                        skip(inner)
            elif kind in GGUF_WIDTHS:
                source.seek(GGUF_WIDTHS[kind], os.SEEK_CUR)
            else:
                raise RuntimeError(f"unreadable GGUF header: {path}")

        if source.read(4) != b"GGUF" or number(4) != 3:
            raise RuntimeError(f"not a GGUF v3 file: {path}")
        number(8)
        for _ in range(number(8)):
            length = number(8)
            if length > 1024:
                raise RuntimeError(f"unreadable GGUF header: {path}")
            key, kind = source.read(length), number(4)
            if key == b"general.file_type":
                file_type = number(4) if kind == 4 else None
                if file_type not in GGUF_FILE_TYPES:
                    raise RuntimeError(f"unreviewed GGUF file type {file_type}: {path}")
                return GGUF_FILE_TYPES[file_type]
            skip(kind)
    raise RuntimeError(f"GGUF header names no file type: {path}")


def serves(pid, port):
    """Whether the process holds the socket that listens on 127.0.0.1:port."""
    rows = (line.split() for line in Path("/proc/net/tcp").read_text().splitlines()[1:])
    sockets = {f"socket:[{row[9]}]" for row in rows if row[1] == f"0100007F:{port:04X}" and row[3] == "0A"}
    for descriptor in (Path("/proc") / str(pid) / "fd").iterdir():
        try:
            if os.readlink(descriptor) in sockets:
                return True
        except OSError:
            continue
    return False


def verify_llama(root, expected_host, full=False):
    """Attest the owner's llama-server process, its build record and its model file."""
    record = json.loads((root / "llama-session.json").read_text())
    runtime = Path(record["runtime"])
    proc = Path("/proc") / str(record["pid"])

    def observe():
        argv = [s.decode() for s in (proc / "cmdline").read_bytes().split(b"\0") if s]
        return argv, (proc / "stat").read_text().rsplit(")", 1)[1].split()[19]

    argv, started = observe()
    environment = dict(value.split("=", 1) for value in (proc / "environ").read_bytes().decode().split("\0") if value)
    if (argv != record["argv"] or started != record["start_ticks"] or environment != record["environment"]
            or (proc / "cwd").resolve() != runtime or argv[0] != str(runtime / "bin/llama-server")):
        raise RuntimeError("owned llama-server process differs from its session record")
    port, model = argv[argv.index("--port") + 1], Path(argv[argv.index("-m") + 1])
    if port != str(record["port"]) or model != Path(record["model"]) or not serves(record["pid"], int(port)):
        raise RuntimeError("owned llama-server does not serve its recorded port and model")
    build_bytes = (runtime / "BUILD.json").read_bytes()
    build = json.loads(build_bytes)
    # A GPU or RPC backend would need its own review, and a different label.
    if (build.get("schema_version") != 1 or build.get("target") != "llama-server"
            or build.get("binary", {}).get("path") != "bin/llama-server"
            or any(option.startswith("-DGGML_") and option.endswith("=ON") and option != "-DGGML_NATIVE=ON"
                   for option in build.get("cmake_options", []))):
        raise RuntimeError("unreviewed llama-server build record")
    hashes = ArtifactHashes(Path.home() / ".cache/playtime-chat", expected_host or {}, full or expected_host is None)
    engine = hashes.hash_file(proc / "exe")["sha256"]
    if engine != build["binary"]["sha256"]:
        raise RuntimeError("running llama-server differs from its build record")
    artifacts = [hashes.hash_file(model, reusable=True)]
    # A template file that replaces the model's own is served too. It is small,
    # so it's always hashed again.
    if "--chat-template-file" in argv:
        template = Path(argv[argv.index("--chat-template-file") + 1])
        if not template.is_absolute():
            raise RuntimeError("llama-server's chat template file must be an absolute path")
        artifacts.append(hashes.hash_file(template))
    # Each launch draws its own port; the rest of the command is reviewed.
    launch = {"argv": ["PORT" if previous == "--port" else value for previous, value in zip(["", *argv], argv)],
              "environment": environment, "cwd": str(runtime)}
    host = {"backend": "cpu", "quantization": gguf_quantization(model), "engine_sha256": engine,
            "artifacts_manifest_sha256": digest_json(artifacts),
            "launch_configuration_sha256": digest_json(launch), "source_commit": build["commit"],
            "build_manifest_sha256": hashlib.sha256(build_bytes).hexdigest()}
    if observe() != (argv, started) or (runtime / "BUILD.json").read_bytes() != build_bytes:
        raise RuntimeError("llama-server process or build record changed during verification")
    if expected_host is not None:
        hashes.finish(host)
    else:
        for path, before in hashes.observed.items():
            if identity(path.stat()) != before:
                raise RuntimeError("artifact changed during discovery")
    report = {"host": host, "artifacts": artifacts, "launch": launch,
              "process": {"pid": record["pid"], "start_ticks": started, "port": record["port"]},
              "verification": hashes.stats}
    # A new lock's reviewer reads the whole build record; launches pin its digest.
    if expected_host is None:
        report["build"] = build
    return report


def verify(root, expected_host, full=False):
    if (root / "llama-session.json").exists():
        return verify_llama(root, expected_host, full)
    if (root / "session.json").exists():
        return verify_basalt(root, expected_host, full)
    hashes = ArtifactHashes(Path.home() / ".cache/playtime-chat", expected_host or {}, full or expected_host is None)
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
    release = Path(config["cwd"])
    release_files = release_manifest(release, hashes)
    host = {
        "backend": backend,
        "quantization": "TQ2_0",
        "engine_sha256": hashes.hash_file(proc / "exe")["sha256"],
        "artifacts_manifest_sha256": digest_json(artifacts),
        "launch_configuration_sha256": digest_json(launch),
        "responses_source_sha256": hashes.hash_file(release / "serve/responses.py")["sha256"],
        "server_release_manifest_sha256": digest_json(release_files),
    }
    # Reject replacement or exit during hashing rather than handing off stale evidence.
    current = [v.decode() for v in proc.joinpath("cmdline").read_bytes().split(b"\0") if v]
    if current != argv:
        raise RuntimeError("engine changed while verifying")
    if expected_host is not None:
        hashes.finish(host)
    else:
        for path, before in hashes.observed.items():
            if identity(path.stat()) != before:
                raise RuntimeError("artifact changed during discovery")
    report = {"host": host, "artifacts": artifacts, "launch": launch, "process": process,
              "server_release": {"path": str(release),
                                 "files": sum("sha256" in entry for entry in release_files),
                                 "links": sum("link" in entry for entry in release_files),
                                 "bytes": sum(entry.get("size", 0) for entry in release_files)},
              "verification": hashes.stats}
    # A new lock's reviewer reads the whole manifest; launches report its digest.
    if expected_host is None:
        report["server_release"]["manifest"] = release_files
    return report


if __name__ == "__main__":
    os.umask(0o077)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("session", type=Path)
    identity_mode = parser.add_mutually_exclusive_group(required=True)
    identity_mode.add_argument("--expected-host", type=json.loads)
    identity_mode.add_argument("--discover", action="store_true", help="read a new identity without publishing a hash cache")
    parser.add_argument("--full-verify", action="store_true")
    args = parser.parse_args()
    print(json.dumps(verify(args.session, args.expected_host, args.full_verify), indent=2))
