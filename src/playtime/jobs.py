"""Local run ownership and detached workers. Model code never enters this process."""

import fcntl
import json
import re
import sys
import time
from pathlib import Path

from .common import PlaytimeError, command, digest_json, timestamp, tree_manifest, write_json
from .config import load_recipe, repo_path, smoke_recipe

RUN_ID = re.compile(r"[0-9]{8}T[0-9]{6}Z-[0-9a-f]{8}\Z")
MODES = {"experiment", "live-smoke", "scripted-smoke"}


def read_json(path, default=None):
    try:
        return json.loads(Path(path).read_text())
    except FileNotFoundError:
        return default


class RunLock:
    def __init__(self, root, *, wait=0):
        self.path = Path(root) / ".playtime/run.lock"
        self.wait = wait

    def __enter__(self):
        self.path.parent.mkdir(parents=True, exist_ok=True)
        self.file = self.path.open("a")
        deadline = time.monotonic() + self.wait
        while True:
            try:
                fcntl.flock(self.file, fcntl.LOCK_EX | fcntl.LOCK_NB)
                return self
            except BlockingIOError:
                if time.monotonic() >= deadline:
                    self.file.close()
                    raise PlaytimeError("An experiment is already running or starting") from None
                time.sleep(0.05)

    def __exit__(self, *args):
        fcntl.flock(self.file, fcntl.LOCK_UN)
        self.file.close()


def lock_held(root):
    try:
        with RunLock(root):
            return False
    except PlaytimeError:
        return True


class Systemd:
    def start(self, root, directory):
        unit = "playtime-" + directory.name + ".service"
        command(
            [
                "systemd-run",
                "--user",
                "--quiet",
                "--collect",
                "--expand-environment=no",
                "--unit=" + unit,
                "--property=Type=exec",
                "--property=Restart=no",
                "--property=UMask=0077",
                "--property=KillMode=mixed",
                "--property=TimeoutStopSec=180",
                "--working-directory=" + str(root),
                sys.executable,
                str(directory / "inputs/controller/playtime"),
                "--root",
                str(root),
                "worker",
                directory.name,
            ],
            timeout=15,
        )

    def state(self, unit):
        data = (
            command(
                ["systemctl", "--user", "show", unit, "--property=ActiveState", "--value"],
                timeout=5,
            )
            .decode()
            .strip()
        )
        return data or "inactive"

    def stop(self, unit):
        command(["systemctl", "--user", "--no-block", "stop", unit], timeout=10)


def active_directory(root):
    active = read_json(Path(root) / ".playtime/active.json", {})
    name = active.get("id", "")
    if not RUN_ID.fullmatch(name):
        return None
    path = Path(root) / "runs" / name
    return path if path.is_dir() and not path.is_symlink() else None


def ensure_idle(root, backend=None):
    # Caller holds RunLock. A detached unit may be starting before its worker
    # acquires that lock, so its service state also reserves the slot.
    directory = active_directory(root)
    if directory:
        job = read_json(directory / "job.json", {})
        if job.get("unit"):
            state = (backend or Systemd()).state(job["unit"])
            if state not in {"inactive", "failed"}:
                raise PlaytimeError("An experiment is already running or starting")


def mark_active(root, directory, *, mode, recipe_path=None):
    job = {
        "mode": mode,
        "state": "running" if mode == "cli" else "starting",
        "created": timestamp(),
        "recipe_path": recipe_path,
    }
    if mode == "service":
        job["unit"] = "playtime-" + directory.name + ".service"
    write_json(directory / "job.json", job)
    write_json(Path(root) / ".playtime/active.json", {"id": directory.name})
    return job


def finish_job(directory, error=None):
    job = read_json(directory / "job.json", {})
    result = read_json(directory / "result.json")
    job.update(state="finished" if result else "failed", finished=timestamp())
    if error:
        job["error"] = error
    elif not result:
        job["error"] = "The worker exited before writing a final result."
    write_json(directory / "job.json", job)


def worker(root, run_id):
    if not RUN_ID.fullmatch(run_id):
        raise PlaytimeError("Invalid run ID")
    root = Path(root).resolve()
    directory = root / "runs" / run_id
    if directory.is_symlink():
        raise PlaytimeError("Invalid run directory")
    # The launch process owns the lock until systemd acknowledges startup.
    with RunLock(root, wait=20):
        job = read_json(directory / "job.json", {})
        if job.get("state") != "starting" or (directory / "events.jsonl").exists():
            raise PlaytimeError("This run has already started; use a fresh run")
        try:
            from .runner import check, execute_prepared

            manifest = read_json(directory / "manifest.json")
            if manifest["inputs"] != tree_manifest(directory / "inputs"):
                raise PlaytimeError("Archived inputs changed before launch")
            check(
                root,
                manifest["recipe"],
                sources=False,
                live=manifest["kind"] != "scripted-smoke",
                image=manifest["image"],
                model=manifest["model"],
            )
            job.update(state="running", started=timestamp())
            write_json(directory / "job.json", job)
            _, result = execute_prepared(directory, manifest)
            finish_job(directory)
            return result
        except BaseException as error:
            finish_job(directory, f"{type(error).__name__}: {error}")
            raise


class RunManager:
    def __init__(self, root, backend=None):
        self.root = Path(root).resolve()
        self.runs = self.root / "runs"
        self.backend = backend or Systemd()

    def directory(self, run_id):
        if not RUN_ID.fullmatch(run_id):
            raise PlaytimeError("Invalid run ID")
        directory = self.runs / run_id
        if directory.is_symlink() or not (directory / "manifest.json").is_file():
            raise PlaytimeError("Run not found")
        return directory

    def recipes(self):
        result = []
        for path in sorted((self.root / "experiments").glob("*.toml")):
            if path.is_symlink():
                continue
            name = path.relative_to(self.root).as_posix()
            try:
                recipe = load_recipe(path)
                result.append({"path": name, "name": recipe["name"]})
            except (OSError, ValueError, PlaytimeError) as error:
                result.append({"path": name, "error": str(error)})
        return result

    def preview(self, name, mode="experiment"):
        if name not in {item["path"] for item in self.recipes()} or mode not in MODES:
            raise PlaytimeError("Choose an available recipe and run type")
        recipe = load_recipe(self.root / name)
        if mode != "experiment":
            recipe = smoke_recipe(recipe, live=mode == "live-smoke")
        from .runner import SMOKE_PROMPT

        prompt = (
            repo_path(self.root, recipe["prompt"]).read_text()
            if mode == "experiment"
            else SMOKE_PROMPT
        )
        protocol = repo_path(self.root, recipe["protocol"]).read_text()
        inputs = {
            "recipe": recipe,
            "prompt": prompt,
            "protocol": protocol,
            "seeds": tree_manifest(repo_path(self.root, recipe["seeds"]))
            if recipe["seeds"]
            else {},
            "model_lock": read_json(repo_path(self.root, recipe["model"]["lock"])),
            "image_lock": read_json(repo_path(self.root, recipe["workshop"]["lock"])),
        }
        return {
            "path": name,
            "mode": mode,
            "recipe": recipe,
            "prompt": prompt,
            "protocol": protocol,
            "revision": digest_json(inputs),
        }

    def start(self, name, mode, revision):
        from .runner import prepare_run

        with RunLock(self.root):
            ensure_idle(self.root, self.backend)
            preview = self.preview(name, mode)
            if preview["revision"] != revision:
                raise PlaytimeError(
                    "Recipe or inputs changed. Refresh the preview before launching."
                )
            directory, _ = prepare_run(
                self.root,
                preview["recipe"],
                live=False,
                scripted=mode == "scripted-smoke",
                smoke=mode != "experiment",
            )
            mark_active(self.root, directory, mode="service", recipe_path=name)
            try:
                self.backend.start(self.root, directory)
            except (OSError, PlaytimeError) as error:
                finish_job(directory, str(error))
            return {"id": directory.name, "url": "/watch/" + directory.name}

    def status(self, directory):
        manifest = read_json(directory / "manifest.json")
        job = read_json(directory / "job.json", {})
        result = read_json(directory / "result.json")
        active = False
        service_state = None
        error = job.get("error")
        if active_directory(self.root) == directory:
            try:
                if job.get("unit"):
                    service_state = self.backend.state(job["unit"])
                    active = service_state not in {"inactive", "failed"}
                else:
                    active = lock_held(self.root)
            except PlaytimeError as exc:
                # Unavailable service manager is not evidence that a worker stopped.
                active, service_state, error = True, "unknown", str(exc)
        if active:
            state = (
                "stopping"
                if (directory / "stop-requested.json").exists()
                else job.get("state", "running")
            )
            if result:
                state = "finalizing"
        elif result:
            state = "finished"
        elif (directory / "stop-requested.json").exists():
            state = "interrupted"
            if not error or error.startswith("KeyboardInterrupt:"):
                error = "Stopped before a final result was recorded."
        elif job.get("state") == "failed":
            state = "failed"
        else:
            state = "interrupted"
            error = error or "Worker is no longer running; no final result was recorded."
        return {
            "id": directory.name,
            "name": manifest["recipe"]["name"],
            "kind": manifest["kind"],
            "created": manifest.get("created"),
            "state": state,
            "active": active,
            "can_stop": active and bool(job.get("unit")) and service_state != "unknown",
            "error": error,
            "result": result,
            "recipe": manifest["recipe"],
            "report_url": f"/{directory.name}/report.html"
            if (directory / "report.html").is_file()
            else None,
        }

    def listing(self):
        records = []
        for directory in sorted(self.runs.iterdir(), reverse=True):
            if not RUN_ID.fullmatch(directory.name) or directory.is_symlink():
                continue
            try:
                records.append(self.status(directory))
            except (OSError, ValueError, KeyError, TypeError):
                continue
        return records

    def stop(self, run_id):
        directory = self.directory(run_id)
        status = self.status(directory)
        if not status["can_stop"]:
            raise PlaytimeError("This run is not an active dashboard worker")
        write_json(directory / "stop-requested.json", {"time": timestamp()})
        self.backend.stop("playtime-" + run_id + ".service")
        return {"id": run_id, "state": "stopping"}
