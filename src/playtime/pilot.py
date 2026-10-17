"""Freeze paired inputs before launch; keep every started slot, including failures."""

import copy
import json
import time
import uuid
from pathlib import Path

from .common import PlaytimeError, controller_manifest, digest_json, timestamp, write_json
from .jobs import RunLock, ensure_idle, finish_job, mark_active
from .runner import check, execute_prepared, prepare_run, validate_archive

ORDER = [
    (1042, "self"), (1042, "self_plus_evidence"),
    (2042, "self_plus_evidence"), (2042, "self"),
    (3042, "self"), (3042, "self_plus_evidence"),
]


def identity(manifest):
    recipe = copy.deepcopy(manifest["recipe"])
    recipe.pop("memory_condition", None)
    recipe.pop("sampling_seed")
    return {
        "recipe": recipe,
        "controller_sha256": manifest["controller_sha256"],
        "model": manifest["model"],
        "image": manifest["image"],
        "inputs": {
            k: v for k, v in manifest["inputs"].items() if k not in {"recipe.json", "pairing.json"}
        },
    }


def prepare_pilot(root, recipe):
    root = Path(root).resolve()
    batch_id = time.strftime("%Y%m%dT%H%M%SZ", time.gmtime()) + "-" + uuid.uuid4().hex[:8]
    directory = root / "runs/pilots" / batch_id
    with RunLock(root):
        ensure_idle(root)
        directory.mkdir(parents=True)
        plan = {"schema": 1, "id": batch_id, "created": timestamp(), "runs": []}
        anchor = None
        for index, (seed, condition) in enumerate(ORDER):
            variant = copy.deepcopy(recipe)
            variant.update(sampling_seed=seed, memory_condition=condition)
            pairing = {
                "pilot": batch_id, "pair": index // 2 + 1, "order": index + 1,
                "seed": seed, "condition": condition,
            }
            run_directory, manifest = prepare_run(root, variant, previous=anchor, pairing=pairing)
            if anchor is None:
                anchor = run_directory
                plan["identity"] = identity(manifest)
            if identity(manifest) != plan["identity"]:
                raise PlaytimeError("Inputs changed while freezing pilot")
            plan["runs"].append(
                {**pairing, "id": run_directory.name, "manifest_sha256": digest_json(manifest)}
            )
        write_json(directory / "pilot.json", plan)
    print(f"Frozen pilot: {directory}", flush=True)
    return directory


def execute_pilot(root, directory):
    root, directory = Path(root).resolve(), Path(directory).resolve()
    plan = json.loads((directory / "pilot.json").read_text())
    source = Path(__file__).resolve().parents[2]
    if digest_json(controller_manifest(source)) != plan["identity"]["controller_sha256"]:
        raise PlaytimeError("Controller changed; invoke the first run's archived controller")
    expected_order = [(r["seed"], r["condition"]) for r in plan["runs"]]
    if expected_order != ORDER:
        raise PlaytimeError("Pilot order differs from the six-run protocol")
    if len({entry["id"] for entry in plan["runs"]}) != len(plan["runs"]):
        raise PlaytimeError("Pilot run IDs must be unique")
    progress_path = directory / "progress.json"
    progress = json.loads(progress_path.read_text()) if progress_path.exists() else {}
    with RunLock(root):
        ensure_idle(root)
        # Validate every frozen input before spending any inference budget.
        manifests = []
        for index, entry in enumerate(plan["runs"]):
            run_directory = root / "runs" / entry["id"]
            manifest = json.loads((run_directory / "manifest.json").read_text())
            validate_archive(run_directory, manifest)
            if (
                digest_json(manifest) != entry["manifest_sha256"]
                or identity(manifest) != plan["identity"]
            ):
                raise PlaytimeError("Frozen pilot identities changed")
            seed, condition = ORDER[index]
            pairing = {
                "pilot": plan["id"], "pair": index // 2 + 1, "order": index + 1,
                "seed": seed, "condition": condition,
            }
            pairing_path = run_directory / "inputs/pairing.json"
            if (
                {key: entry.get(key) for key in pairing} != pairing
                or manifest["id"] != entry["id"]
                or manifest.get("pairing") != pairing
                or manifest["recipe"]["sampling_seed"] != seed
                or manifest["recipe"].get("memory_condition", "self") != condition
                or not pairing_path.is_file()
                or json.loads(pairing_path.read_text()) != pairing
            ):
                raise PlaytimeError("Frozen pilot membership differs from its archived inputs")
            manifests.append((run_directory, manifest))
        for entry, (run_directory, manifest) in zip(plan["runs"], manifests):
            key = entry["id"]
            if key in progress or (run_directory / "events.jsonl").exists():
                # A crash or failed inference is evidence, never a retry request.
                progress.setdefault(key, {"state": "previously_started", "retained": True})
                write_json(progress_path, progress)
                continue
            progress[key] = {"state": "started", "started": timestamp()}
            write_json(progress_path, progress)
            mark_active(root, run_directory, mode="cli")
            try:
                check(
                    root, manifest["recipe"], sources=False,
                    image=manifest["image"], model=manifest["model"],
                )
                _, result = execute_prepared(run_directory, manifest)
                progress[key].update(state="finished", result=result)
            except (KeyboardInterrupt, Exception) as error:
                progress[key].update(state="failed", error=f"{type(error).__name__}: {error}")
                raise
            finally:
                progress[key]["finished"] = timestamp()
                write_json(progress_path, progress)
                finish_job(run_directory, progress[key].get("error"))
            if result["stop_reason"] == "interrupted":
                # Progress and cleanup are complete; preserve the CLI's exit 130.
                raise KeyboardInterrupt
    return directory
