#!/usr/bin/env python3
"""File-based seed authoring, not an experiment controller. Standard library only."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import sys

ROOT = Path(__file__).resolve().parent.parent
FIELDS = {"schema_version", "id", "name", "description", "revision", "files", "provenance", "assessment"}
ID = re.compile(r"[a-z0-9]+(?:-[a-z0-9]+)*")


def require(condition, message):
    if not condition:
        raise ValueError(message)


def plain_directory(root, relative):
    require(isinstance(relative, str), "directory must be a relative string")
    parts = relative.split("/")
    require(all(p and p not in (".", "..") for p in parts), "directory must be a plain relative path")
    current = root
    for part in parts:
        current = current / part
        require(not current.is_symlink() and current.is_dir(), f"not a plain directory: {relative}")
    return current


def validate(root, bundle_id):
    require(ID.fullmatch(bundle_id), "invalid bundle id")
    catalog = plain_directory(root, "seed-bundles")
    path = catalog / f"{bundle_id}.json"
    require(not path.is_symlink() and path.is_file(), "manifest must be a regular file")
    require(path.stat().st_size <= 16384, "manifest exceeds 16 KiB")
    manifest = json.loads(path.read_text(encoding="utf-8"))
    require(isinstance(manifest, dict) and set(manifest) == FIELDS, "manifest fields do not match schema 1")
    require(type(manifest["schema_version"]) is int and manifest["schema_version"] == 1, "unsupported schema")
    require(manifest["id"] == bundle_id, "manifest id must match filename")
    require(type(manifest["revision"]) is int and manifest["revision"] > 0, "revision must be a positive integer")
    for field in ("name", "description", "provenance"):
        require(isinstance(manifest[field], str) and manifest[field].strip(), f"{field} must be nonempty text")
    require(manifest["files"] == f"seeds/{bundle_id}", "files must name seeds/<id>")
    directory = plain_directory(root, manifest["files"])
    if manifest["assessment"] is not None:
        require(manifest["assessment"] == f"assessments/{bundle_id}", "assessment must name assessments/<id>")
        plain_directory(root, manifest["assessment"])
    files = {}
    entries = 1
    for parent, dirs, names in os.walk(directory, followlinks=False):
        entries += len(dirs) + len(names)
        for name in dirs + names:
            item = Path(parent) / name
            mode = item.lstat().st_mode
            require(stat.S_ISDIR(mode) or stat.S_ISREG(mode), f"link or special file: {item}")
            rel = item.relative_to(directory).as_posix()
            require(len(rel.encode("utf-8")) <= 200 and not any(ord(c) < 32 or 127 <= ord(c) <= 159 for c in rel), "invalid seed path")
        for name in sorted(names):
            item = Path(parent) / name
            require(item.stat().st_size <= 131072, f"file exceeds 128 KiB: {item}")
            data = item.read_bytes()
            content = data.decode("utf-8")
            require(len(json.dumps(content, ensure_ascii=False).encode("utf-8")) <= 196608, f"encoded seed too large: {item}")
            files[item.relative_to(directory).as_posix()] = data
    require(len(files) <= 16, "more than 16 seed files")
    size = sum(map(len, files.values()))
    require(size <= 524288, "seeds exceed 512 KiB")
    inventory = [{"path": p, "size_bytes": len(b), "sha256": hashlib.sha256(b).hexdigest()}
                 for p, b in sorted(files.items())]
    digest = hashlib.sha256(json.dumps({"manifest": manifest, "files": inventory},
                           sort_keys=True, ensure_ascii=False, separators=(",", ":")).encode()).hexdigest()
    return manifest, files, {"id": bundle_id, "revision": manifest["revision"],
                            "bundle_sha256": digest, "seed_bytes": size,
                            "minimum_workspace_bytes": size * 2,
                            "minimum_workspace_entries": entries * 2, "files": inventory}


def create(root, bundle_id, name, source=None):
    require(ID.fullmatch(bundle_id), "invalid bundle id")
    require(name.strip(), "name must not be empty")
    require(not (root / "seed-bundles" / f"{bundle_id}.json").exists(), "bundle already exists")
    require(not (root / "seeds" / bundle_id).exists(), "seed directory already exists")
    if source:
        old, files, receipt = validate(root, source)
        description = old["description"]
        provenance = f"Cloned from {source} revision {old['revision']}, bundle SHA-256 {receipt['bundle_sha256']}."
    else:
        files = {"ABOUT.txt": b"Describe these starting materials, their origin, and any suggested use.\n"}
        description = "New seed bundle; edit this description and the starting materials."
        provenance = "Locally authored; record the origin of imported material here."
    # Cloning copies starting materials, not an independent assessment whose
    # requirements may no longer apply. Add/review one explicitly if needed.
    manifest = dict(schema_version=1, id=bundle_id, name=name, description=description,
                    revision=1, files=f"seeds/{bundle_id}", provenance=provenance, assessment=None)
    for parent in ("seeds", "seed-bundles"):
        (root / parent).mkdir(exist_ok=True)
        plain_directory(root, parent)
    directory = root / "seeds" / bundle_id
    directory.mkdir()
    for path, data in files.items():
        target = directory / path
        target.parent.mkdir(parents=True, exist_ok=True)
        with target.open("xb") as stream:
            stream.write(data)
    with (root / "seed-bundles" / f"{bundle_id}.json").open("x", encoding="utf-8") as stream:
        stream.write(json.dumps(manifest, indent=2) + "\n")
    return validate(root, bundle_id)[2]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=ROOT, help="repository root (defaults to this checkout)")
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("list")
    check = commands.add_parser("validate")
    check.add_argument("id", nargs="?", help="omit to validate every bundle")
    new = commands.add_parser("new")
    new.add_argument("id")
    new.add_argument("--name", required=True)
    clone = commands.add_parser("clone")
    clone.add_argument("source")
    clone.add_argument("id")
    clone.add_argument("--name", required=True)
    args = parser.parse_args()
    root = args.root.resolve(strict=True)
    if args.command in ("new", "clone"):
        result = create(root, args.id, args.name, getattr(args, "source", None))
    else:
        catalog = plain_directory(root, "seed-bundles")
        ids = [args.id] if getattr(args, "id", None) else sorted(p.stem for p in catalog.glob("*.json"))
        result = []
        for bundle_id in ids:
            manifest, _, receipt = validate(root, bundle_id)
            result.append(({"id": bundle_id, "name": manifest["name"], "revision": manifest["revision"],
                            "description": manifest["description"], "assessment": manifest["assessment"]}
                           if args.command == "list" else receipt))
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    try:
        main()
    except (ValueError, OSError, UnicodeError) as error:
        print(f"seed-bundle: {error}", file=sys.stderr)
        raise SystemExit(1)
