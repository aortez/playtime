"""Bounded previews of saved files; never execute or extract workspace content."""

import base64
import codecs
import tarfile
from pathlib import PurePosixPath
from urllib.parse import quote

IMAGE_SUFFIXES = {".png", ".jpg", ".jpeg", ".gif", ".webp"}
IMAGE_BYTES = 4 * 1024 * 1024
TOTAL_IMAGE_BYTES = 16 * 1024 * 1024
TEXT_BYTES = 12_000
TEXT_LINES = 100


class PreviewBudget:
    def __init__(self):
        self.images = TOTAL_IMAGE_BYTES
        self.text = 2 * 1024 * 1024


def image_mime(name, header):
    suffix = PurePosixPath(name).suffix.lower()
    if suffix == ".png" and header.startswith(b"\x89PNG\r\n\x1a\n"):
        return "image/png"
    if suffix in {".jpg", ".jpeg"} and header.startswith(b"\xff\xd8\xff"):
        return "image/jpeg"
    if suffix == ".gif" and header[:6] in {b"GIF87a", b"GIF89a"}:
        return "image/gif"
    if suffix == ".webp" and header[:4] == b"RIFF" and header[8:12] == b"WEBP":
        return "image/webp"
    return None


def image_preview(source, name, size, budget):
    header = source.read(16)
    mime = image_mime(name, header)
    if not mime:
        return {"notice": "This file does not have a recognized image header."}
    if size > IMAGE_BYTES:
        return {"notice": "Image exceeds the 4 MiB inline preview limit."}
    if size > budget.images:
        return {"notice": "The report's 16 MiB image preview allowance is full."}
    data = header + source.read(max(0, size - len(header)) + 1)
    if len(data) != size:
        return {"notice": "The image could not be read completely."}
    budget.images -= size
    return {"src": f"data:{mime};base64,{base64.b64encode(data).decode()}", "mime": mime}


def text_preview(source, size, budget):
    maximum = min(TEXT_BYTES, budget.text)
    if not maximum:
        return None
    data = source.read(maximum)
    truncated = size > len(data)
    try:
        text = codecs.getincrementaldecoder("utf-8")().decode(data, final=not truncated)
    except UnicodeDecodeError:
        return None
    if any(ord(char) < 32 and char not in "\n\r\t" for char in text):
        return None
    lines = text.splitlines(keepends=True)
    budget.text -= len(data)
    return {"text": "".join(lines[:TEXT_LINES]), "truncated": truncated or len(lines) > TEXT_LINES}


def final_artifacts(directory, budget):
    workspace = directory / "final-workspace"
    artifacts = []
    if not workspace.is_dir() or workspace.is_symlink():
        return artifacts
    for path in sorted(workspace.rglob("*")):
        if path.is_symlink() or not path.is_file():
            continue
        name = path.relative_to(workspace).as_posix()
        artifact = {
            "name": name,
            "url": "./" + quote(path.relative_to(directory).as_posix(), safe="/"),
            "size": path.stat().st_size,
        }
        try:
            with path.open("rb") as source:
                if path.suffix.lower() in IMAGE_SUFFIXES:
                    artifact["image"] = image_preview(source, name, artifact["size"], budget)
                else:
                    artifact["text"] = text_preview(source, artifact["size"], budget)
        except OSError:
            artifact["notice"] = "Preview unavailable; the saved file could not be read."
        artifacts.append(artifact)
    return artifacts


def checkpoint_images(directory, number, changes, budget):
    wanted = {
        name: label
        for key, label in [("added", "Added"), ("modified", "Modified")]
        for name in changes.get(key, [])
        if PurePosixPath(name).suffix.lower() in IMAGE_SUFFIXES
    }
    archive_path = directory / "sessions" / f"{number:03d}" / "workspace.tar"
    if not wanted or not archive_path.is_file() or archive_path.is_symlink():
        return [], None
    images = {}
    try:
        with tarfile.open(archive_path) as archive:
            for member in archive:
                path = PurePosixPath(member.name)
                name = path.as_posix()
                if path.is_absolute() or ".." in path.parts or name not in wanted:
                    continue
                # Do not follow tar symlinks or hardlinks, even to other members.
                if not member.isfile():
                    continue
                with archive.extractfile(member) as source:
                    images[name] = {
                        "name": name,
                        "size": member.size,
                        "change": wanted[name],
                        "image": image_preview(source, name, member.size, budget),
                    }
    except (OSError, tarfile.TarError, ValueError):
        return [], "Image previews are unavailable because the saved snapshot could not be read."
    return [images[name] for name in sorted(images)], None
