import json
import subprocess
from pathlib import Path

from .common import PlaytimeError, command, digest_file, digest_json, tree_manifest, write_json
from .config import repo_path


def inspect_image(reference):
    return json.loads(command(["docker", "image", "inspect", reference]))[0]


def prepare_image(root, recipe, *, refresh=False, image=None):
    root = Path(root)
    lock_path = repo_path(root, recipe["workshop"]["lock"])
    sources = tree_manifest(root / "workshop")
    source_hash = digest_json(sources)
    if lock_path.exists() and not refresh:
        locked = json.loads(lock_path.read_text())
        if locked["build_context_sha256"] != source_hash:
            raise PlaytimeError(
                "Workshop sources changed; use prepare --refresh to create a new image lock"
            )
        check_image(root, recipe, locked)
        return locked
    base = (root / "workshop/base-image.txt").read_text().strip()
    toolsets = recipe["workshop"]["toolsets"]
    tag = "playtime-workshop:" + digest_json([source_hash, toolsets])[:16]
    if image is None:
        print("Building workshop " + tag, flush=True)
        subprocess.run(
            [
                "docker",
                "build",
                "--build-arg",
                "BASE_IMAGE=" + base,
                "--build-arg",
                "TOOLSETS=" + ",".join(toolsets),
                "--tag",
                tag,
                str(root / "workshop"),
            ],
            check=True,
        )
        image = tag
    info = inspect_image(image)
    actual_sets = (
        command(
            [
                "docker",
                "run",
                "--rm",
                "--network",
                "none",
                info["Id"],
                "cat",
                "/opt/workshop/toolsets.txt",
            ]
        )
        .decode()
        .strip()
        .split(",")
    )
    if actual_sets != toolsets:
        raise PlaytimeError("Existing image's toolsets do not match the recipe")
    actual_helper = (
        command(
            [
                "docker",
                "run",
                "--rm",
                "--network",
                "none",
                info["Id"],
                "sha256sum",
                "/opt/playtime/tool.py",
            ]
        )
        .decode()
        .split()[0]
    )
    if actual_helper != digest_file(root / "workshop/tool.py"):
        raise PlaytimeError("Existing image does not contain the current tool bridge")
    archive = root / ".playtime/images" / (info["Id"].split(":")[-1] + ".tar")
    archive.parent.mkdir(parents=True, exist_ok=True)
    if not archive.exists():
        print("Archiving the exact workshop image", flush=True)
        temporary = archive.with_suffix(".partial")
        command(
            ["docker", "image", "save", info["Id"]],
            timeout=600,
            output_path=temporary,
            max_bytes=32 * 1024**3,
        )
        temporary.replace(archive)
    locked = {
        "schema": 1,
        "image_id": info["Id"],
        "base_image": base,
        "toolsets": toolsets,
        "architecture": info["Architecture"],
        "os": info["Os"],
        "build_context_sha256": source_hash,
        "build_sources": sources,
        "archive": archive.relative_to(root).as_posix(),
        "archive_sha256": digest_file(archive),
    }
    write_json(lock_path, locked)
    return locked


def check_image(root, recipe, locked):
    if locked["toolsets"] != recipe["workshop"]["toolsets"]:
        raise PlaytimeError("Workshop lock has different toolsets")
    try:
        info = inspect_image(locked["image_id"])
    except PlaytimeError:
        archive = Path(root) / locked["archive"]
        if not archive.is_file() or digest_file(archive) != locked["archive_sha256"]:
            raise PlaytimeError(
                "Locked image is missing and its archive is unavailable or corrupt"
            ) from None
        print("Restoring archived workshop image", flush=True)
        command(["docker", "image", "load", "--input", archive], timeout=600)
        info = inspect_image(locked["image_id"])
    if info["Id"] != locked["image_id"]:
        raise PlaytimeError("Docker image identity differs from lock")
    return info
