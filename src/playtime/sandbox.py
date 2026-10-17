import io
import json
import math
import tarfile
import time
from pathlib import Path, PurePosixPath

from .common import PlaytimeError, StopRun, command, digest_file, write_json


def initial_archive(seed_path, destination):
    with tarfile.open(destination, "w") as archive:
        if seed_path is None:
            return
        if not Path(seed_path).is_dir():
            raise PlaytimeError(f"Seed directory does not exist: {seed_path}")
        directories = [
            Path(seed_path),
            *sorted(p for p in Path(seed_path).rglob("*") if p.is_dir()),
        ]
        for source in directories:
            if source.is_symlink():
                raise PlaytimeError(f"Seed symlinks are not supported: {source}")
            name = "seeds/" + source.relative_to(seed_path).as_posix()
            info = tarfile.TarInfo(name)
            info.type = tarfile.DIRTYPE
            info.uid = info.gid = 1000
            info.mode = 0o755
            archive.addfile(info)
        for source in sorted(Path(seed_path).rglob("*")):
            if source.is_symlink():
                raise PlaytimeError(f"Seed symlinks are not supported: {source}")
            if not source.is_file():
                continue
            data = source.read_bytes()
            info = tarfile.TarInfo("seeds/" + source.relative_to(seed_path).as_posix())
            info.uid = info.gid = 1000
            info.mode = 0o755 if source.stat().st_mode & 0o111 else 0o644
            info.size = len(data)
            info.mtime = 0
            archive.addfile(info, io.BytesIO(data))


def safe_member(name):
    path = PurePosixPath(name)
    if path.is_absolute() or ".." in path.parts:
        raise PlaytimeError(f"Unsafe archive path: {name}")
    return path


def archive_index(path):
    import hashlib

    result = {}
    with tarfile.open(path) as archive:
        for member in archive:
            relative = safe_member(member.name).as_posix()
            if member.isdir():
                continue
            entry = {
                "size": member.size,
                "mode": member.mode,
                "type": member.type.decode(errors="replace"),
            }
            if member.isfile():
                with archive.extractfile(member) as source:
                    entry["sha256"] = hashlib.file_digest(source, "sha256").hexdigest()
            elif member.issym() or member.islnk():
                entry["link"] = member.linkname
            result[relative] = entry
    return result


def extract_regular_files(path, destination):
    """Never create links, devices, or paths outside the new artifact directory."""
    destination = Path(destination)
    destination.mkdir(parents=True, exist_ok=False)
    with tarfile.open(path) as archive:
        for member in archive:
            relative = safe_member(member.name)
            if not member.isfile():
                continue
            target = destination.joinpath(*relative.parts)
            target.parent.mkdir(parents=True, exist_ok=True)
            with archive.extractfile(member) as source, target.open("wb") as output:
                while data := source.read(65536):
                    output.write(data)
            target.chmod(member.mode & 0o777)


class Sandbox:
    def __init__(self, image, recipe, run_id, deadline):
        self.image = image
        self.recipe = recipe
        self.run_id = run_id
        self.deadline = deadline
        self.volume = "playtime-work-" + run_id
        self.holder = "playtime-store-" + run_id
        self.active = None
        self.created_volume = False
        self.created_holder = False

    def remaining(self):
        remaining = self.deadline - time.monotonic()
        if remaining <= 0:
            raise StopRun("wall_limit")
        return remaining

    def create(self, initial):
        workshop = self.recipe["workshop"]
        command(
            [
                "docker",
                "volume",
                "create",
                "--label",
                "playtime.run=" + self.run_id,
                "--driver",
                "local",
                "--opt",
                "type=tmpfs",
                "--opt",
                "device=tmpfs",
                "--opt",
                f"o=size={workshop['workspace_mb']}m,uid=1000,gid=1000,mode=0700,nr_inodes=16384",
                self.volume,
            ]
        )
        self.created_volume = True
        self.created_holder = True
        command(
            [
                "docker",
                "run",
                "-d",
                "--name",
                self.holder,
                "--label",
                "playtime.run=" + self.run_id,
                "--label",
                "playtime.role=store",
                "--network",
                "none",
                "--read-only",
                "--cap-drop",
                "ALL",
                "--security-opt",
                "no-new-privileges",
                "--user",
                "0:0",
                "--log-driver",
                "none",
                "--memory",
                "64m",
                "--memory-swap",
                "64m",
                "--pids-limit",
                "64",
                "--mount",
                f"type=volume,src={self.volume},dst=/work",
                self.image,
                "timeout",
                "--signal=KILL",
                str(math.ceil(self.remaining()) + 120),
                "sleep",
                "infinity",
            ]
        )
        if Path(initial).stat().st_size > workshop["workspace_mb"] * 1024**2:
            raise PlaytimeError("Seed archive exceeds the workspace size")
        command(
            ["docker", "cp", "-a", "-", self.holder + ":/work"],
            input_bytes=Path(initial).read_bytes(),
        )

    def start_session(self, number):
        self.remaining()
        workshop = self.recipe["workshop"]
        self.active = f"playtime-agent-{self.run_id}-{number}"
        command(
            [
                "docker",
                "run",
                "-d",
                "--name",
                self.active,
                "--hostname",
                "playtime-workshop",
                "--label",
                "playtime.run=" + self.run_id,
                "--label",
                "playtime.role=agent",
                "--network",
                "none",
                "--read-only",
                "--cap-drop",
                "ALL",
                "--user",
                "0:0",
                "--security-opt",
                "no-new-privileges",
                "--log-driver",
                "none",
                "--cpus",
                str(workshop["cpus"]),
                "--memory",
                f"{workshop['memory_mb']}m",
                "--memory-swap",
                f"{workshop['memory_mb']}m",
                "--pids-limit",
                str(workshop["pids"]),
                "--tmpfs",
                f"/tmp:rw,nosuid,nodev,size={workshop['tmp_mb']}m,mode=1777",
                "--shm-size",
                "64m",
                "--mount",
                f"type=volume,src={self.volume},dst=/work",
                "--env",
                "PLAYTIME_SEED=" + str(self.recipe["sampling_seed"]),
                "--env",
                "PYTHONHASHSEED=" + str(self.recipe["sampling_seed"]),
                self.image,
                "timeout",
                "--signal=KILL",
                str(math.ceil(self.remaining())),
                "sleep",
                "infinity",
            ]
        )

    def tool(self, name, arguments):
        if self.active is None:
            raise PlaytimeError("No active workshop session")
        limits = dict(self.recipe["limits"])
        limits["command_seconds"] = min(limits["command_seconds"], self.remaining())
        payload = {"name": name, "arguments": arguments, "limits": limits}
        output = command(
            [
                "docker",
                "exec",
                "--user",
                "1000:1000",
                "-i",
                self.active,
                "python",
                "/opt/playtime/tool.py",
            ],
            input_bytes=json.dumps(payload).encode(),
            timeout=min(limits["command_seconds"] + 5, self.remaining() + 2),
            max_bytes=524288,
        )
        return json.loads(output)

    def memory(self):
        limit = self.recipe["limits"]["memento_bytes"]
        payload = {
            "name": "read_file",
            "arguments": {"path": "/work/MEMENTO.md"},
            "limits": {"output_bytes": limit},
        }
        result = json.loads(
            command(
                [
                    "docker",
                    "exec",
                    "--user",
                    "1000:1000",
                    "-i",
                    self.holder,
                    "python",
                    "/opt/playtime/tool.py",
                ],
                input_bytes=json.dumps(payload).encode(),
            )
        )
        if result.get("truncated"):
            return {
                "content": "",
                "status": "oversized; inspect MEMENTO.md using tools and shorten it",
            }
        if "error" in result:
            return {"content": "", "status": result["error"]}
        return {"content": result["content"], "status": "loaded"}

    def stop_session(self):
        if self.active:
            name, self.active = self.active, None
            command(["docker", "rm", "--force", name], timeout=20)

    def snapshot(self, destination, previous=None):
        # Called with the agent stopped. The store container keeps the bounded
        # tmpfs volume mounted, so snapshots cannot race with workshop writers.
        destination = Path(destination)
        destination.parent.mkdir(parents=True, exist_ok=True)
        temporary = destination.with_suffix(".partial")
        maximum = self.recipe["workshop"]["workspace_mb"] * 1024**2 + 64 * 1024**2
        command(
            ["docker", "cp", self.holder + ":/work/.", "-"],
            output_path=temporary,
            max_bytes=maximum,
            timeout=60,
        )
        temporary.replace(destination)
        index = archive_index(destination)
        previous = previous or {}
        changes = {
            "added": sorted(index.keys() - previous.keys()),
            "removed": sorted(previous.keys() - index.keys()),
            "modified": sorted(
                key for key in index.keys() & previous.keys() if index[key] != previous[key]
            ),
        }
        write_json(
            destination.with_suffix(".json"),
            {"files": index, "changes": changes, "archive_sha256": digest_file(destination)},
        )
        return index, changes

    def close(self):
        errors = []
        try:
            self.stop_session()
        except PlaytimeError as error:
            errors.append(str(error))
        if self.created_holder:
            try:
                command(["docker", "rm", "--force", self.holder], timeout=20)
            except PlaytimeError as error:
                errors.append(str(error))
        if self.created_volume:
            try:
                command(["docker", "volume", "rm", self.volume], timeout=20)
            except PlaytimeError as error:
                errors.append(str(error))
        return errors
