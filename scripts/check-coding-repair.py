#!/usr/bin/env python3
"""Assess a stopped workspace in Docker; this helper never runs a model."""
import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import signal
import stat
import subprocess
import sys
import tempfile
import uuid

ROOT = Path(__file__).resolve().parent.parent
SUPPORT = ROOT / "assessments/coding-repair-v1"
MAX_REPORT = 1024 * 1024
MAX_FILES = 4096
MAX_WORKSPACE = 48 * 1024 * 1024
OUTCOMES = ("unknown", "completed", "timed_out", "cancelled", "failed", "skipped")


class AssessmentInterrupted(Exception):
    pass


class Cancellation:
    def __init__(self):
        self.requested = False
        self.cleaning_up = False

    def __call__(self, signum, frame):
        self.requested = True
        # Cancellation must not abort owned-container removal or scratch cleanup.
        if not self.cleaning_up:
            raise AssessmentInterrupted()


def digest(data):
    return hashlib.sha256(data).hexdigest()


def bounded_read(path, limit):
    with path.open("rb") as stream:
        data = stream.read(limit + 1)
    if len(data) > limit:
        raise ValueError(f"{path.name} exceeds {limit} bytes")
    return data


def load_task(task_id):
    catalog = json.loads(bounded_read(SUPPORT / "catalog.json", MAX_REPORT))
    if catalog["schema_version"] != 1 or task_id not in catalog["tasks"]:
        raise ValueError("unknown coding-repair task")
    return catalog["tasks"][task_id]


def checker_identity(task_id):
    paths = [SUPPORT / name for name in
             ("catalog.json", "driver.py", "assessment_support.py")]
    paths += [ROOT / "assessments" / task_id / "check.py", Path(__file__).resolve()]
    files = {path.relative_to(ROOT).as_posix(): digest(path.read_bytes()) for path in paths}
    return {"files": files, "sha256": digest(json.dumps(files, sort_keys=True).encode())}


def freeze_candidate(source, destination):
    """Copy bounded regular files before assessment; never import candidate code."""
    source = source.resolve(strict=True)
    if not source.is_dir():
        raise ValueError("candidate must be a directory")
    destination.mkdir(mode=0o755)
    destination.chmod(0o755)
    inventory = []
    directory_paths = []
    total = entries = 0

    def traversal_error(error):
        raise error

    for parent, directories, names in os.walk(source, followlinks=False, onerror=traversal_error):
        directories.sort()
        names.sort()
        for name in directories + names:
            entries += 1
            if entries > MAX_FILES:
                raise ValueError("candidate exceeds 4096 entries")
            path = Path(parent) / name
            relative = path.relative_to(source)
            mode = path.lstat().st_mode
            target = destination / relative
            if stat.S_ISDIR(mode):
                target.mkdir(mode=0o755)
                target.chmod(0o755)
                directory_paths.append(relative.as_posix())
            elif stat.S_ISREG(mode):
                with os.fdopen(os.open(path, os.O_RDONLY | os.O_NOFOLLOW), "rb") as stream:
                    if not stat.S_ISREG(os.fstat(stream.fileno()).st_mode):
                        raise ValueError("candidate file changed type")
                    data = stream.read(MAX_WORKSPACE - total + 1)
                total += len(data)
                if total > MAX_WORKSPACE:
                    raise ValueError("candidate exceeds 48 MiB")
                target.write_bytes(data)
                copied_mode = 0o755 if mode & 0o111 else 0o644
                target.chmod(copied_mode)
                inventory.append({"path": relative.as_posix(), "size_bytes": len(data),
                                  "sha256": digest(data), "mode": f"{copied_mode:04o}"})
            else:
                raise ValueError("candidate contains a link or special file")
    inventory.sort(key=lambda item: item["path"])
    directory_paths.sort()
    identity = {"files": inventory, "directories": directory_paths}
    return {**identity, "size_bytes": total,
            "sha256": digest(json.dumps(identity, sort_keys=True).encode())}


def freeze_checker(task_id, destination, identity):
    destination.mkdir(mode=0o755)
    destination.chmod(0o755)
    sources = [SUPPORT / name for name in ("driver.py", "assessment_support.py")]
    sources.append(ROOT / "assessments" / task_id / "check.py")
    for source in sources:
        data = source.read_bytes()
        if digest(data) != identity["files"][source.relative_to(ROOT).as_posix()]:
            raise ValueError("checker changed during preparation")
        target = destination / source.name
        target.write_bytes(data)
        target.chmod(0o644)


def validate_result(task, raw, exit_code):
    """Only a complete report plus a matching container exit is a check score."""
    if not isinstance(raw, dict):
        raise ValueError("checker report must be an object")
    expected = [case["id"] for case in task["cases"]]
    rows = raw.get("results")
    if not isinstance(rows, list) or any(not isinstance(row, dict) for row in rows):
        raise ValueError("checker report has no complete case list")
    if [row.get("case") for row in rows] != expected:
        raise ValueError("checker case identities/order do not match the catalog")
    if any(type(row.get("passed")) is not bool for row in rows):
        raise ValueError("checker verdicts must be booleans")
    passed = sum(row["passed"] for row in rows)
    if type(raw.get("passed")) is not int or type(raw.get("total")) is not int:
        raise ValueError("checker counts must be integers")
    if raw["passed"] != passed or raw["total"] != len(expected):
        raise ValueError("checker totals disagree with case verdicts")
    if exit_code != (0 if passed == len(expected) else 1):
        raise ValueError("container exit disagrees with checker verdicts")
    result = {"status": "complete", "passed": passed, "total": len(expected),
              "all_checks_passed": passed == len(expected), "results": []}
    for kind in ("functional", "deliverable"):
        selected = [row for case, row in zip(task["cases"], rows) if case["kind"] == kind]
        result[kind] = {"passed": sum(row["passed"] for row in selected), "total": len(selected)}
    result["results"] = [dict(row, kind=case["kind"]) for case, row in zip(task["cases"], rows)]
    return result


def command(args, timeout=15, *, start_new_session=False):
    return subprocess.run(args, stdin=subprocess.DEVNULL, capture_output=True,
                          text=True, timeout=timeout, check=True,
                          start_new_session=start_new_session)


def assess(task_id, candidate, *, outcome="unknown", metadata=None, timeout=30, cancellation=None):
    cancellation = cancellation or Cancellation()
    task = load_task(task_id)
    lock_path = ROOT / "environments/general.lock.json"
    lock_bytes = lock_path.read_bytes()
    image = json.loads(lock_bytes)["image_id"]
    report = {"schema_version": 1, "task": dict(id=task_id, **{
        key: task[key] for key in ("task_revision", "assessment_revision")}),
        "execution": {"outcome": outcome, "metadata": metadata or {},
                      "provenance": "caller-supplied; not inferred from checks"},
        "assessment": {"status": "not_run" if outcome == "skipped" else "incomplete"},
        "checker": checker_identity(task_id), "candidate": None,
        "environment": {"image_id": image, "lock_sha256": digest(lock_bytes),
                        "assessment_timeout_seconds": timeout}}
    if outcome == "skipped":
        return report, 0
    name = "playtime-coding-check-" + uuid.uuid4().hex
    created = False
    cleanup_error = None
    try:
        with tempfile.TemporaryDirectory(prefix="playtime-coding-check-") as temporary:
            try:
                temp = Path(temporary)
                snapshot = temp / "candidate"
                report["candidate"] = freeze_candidate(candidate, snapshot)
                checker_snapshot = temp / "checker"
                freeze_checker(task_id, checker_snapshot, report["checker"])
                command(["docker", "image", "inspect", image])
                args = ["docker", "create", "--name", name, "--pull", "never",
                        "--label", "playtime.assessment=coding-repair-v1",
                        "--network", "none", "--read-only", "--user", "1000:1000",
                        "--cap-drop", "ALL", "--security-opt", "no-new-privileges",
                        "--pids-limit", "32", "--cpus", "1", "--memory", "256m",
                        "--memory-swap", "256m", "--log-driver", "none",
                        "--tmpfs", "/tmp:rw,uid=1000,gid=1000,size=32m,nosuid,nodev",
                        "--workdir", "/candidate", "--mount",
                        f"type=bind,src={snapshot},dst=/candidate,readonly",
                        "--mount", f"type=bind,src={checker_snapshot},dst=/assessment,readonly",
                        "--entrypoint", "python3", image, "-I", "/assessment/driver.py"]
                # Record the owned name before create; cleanup also covers a timed-out
                # create whose container was created but whose response was lost.
                created = True
                command(args)
                with (temp / "stdout").open("wb") as output, (temp / "stderr").open("wb") as errors:
                    attached = subprocess.run(["docker", "start", "--attach", name],
                                              stdin=subprocess.DEVNULL, stdout=output,
                                              stderr=errors, timeout=timeout)
                state = json.loads(command(["docker", "inspect", "--format", "{{json .State}}", name]).stdout)
                if state["Running"] or state["OOMKilled"] or attached.returncode not in (0, 1):
                    raise ValueError("assessment container did not finish normally")
                raw = json.loads(bounded_read(temp / "stdout", MAX_REPORT))
                report["assessment"] = validate_result(task, raw, state["ExitCode"])
            finally:
                cancellation.cleaning_up = True
                if created:
                    try:
                        # Terminal Ctrl-C reaches the parent's process group.
                        # Keep the bounded cleanup client outside that group.
                        command(["docker", "rm", "--force", name], start_new_session=True)
                    except (OSError, subprocess.SubprocessError) as error:
                        cleanup_error = str(error)[:2000]
                        report["cleanup"] = {"status": "failed", "container": name, "error": cleanup_error}
                    else:
                        report["cleanup"] = {"status": "completed"}
    except subprocess.TimeoutExpired:
        report["assessment"] = {"status": "incomplete", "reason": "assessment_timeout"}
    except AssessmentInterrupted:
        report["assessment"] = {"status": "incomplete", "reason": "assessment_interrupted"}
    except (OSError, ValueError, KeyError, TypeError, subprocess.CalledProcessError) as error:
        report["assessment"] = {"status": "incomplete", "reason": str(error)[:2000]}
    if cancellation.requested:
        report["assessment"] = {"status": "incomplete", "reason": "assessment_interrupted"}
    if cleanup_error:
        report["assessment"] = {"status": "incomplete", "reason": "container_cleanup_failed"}
    assessment = report["assessment"]
    if assessment["status"] != "complete":
        return report, 2
    return report, 0 if assessment["all_checks_passed"] else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    catalog = json.loads((SUPPORT / "catalog.json").read_text())
    parser.add_argument("task", choices=sorted(catalog["tasks"]))
    parser.add_argument("candidate", nargs="?", type=Path)
    parser.add_argument("--outcome", choices=OUTCOMES, default="unknown")
    parser.add_argument("--metadata", type=Path, help="JSON with harness/version, model/settings, run id and timing")
    parser.add_argument("--assessment-timeout", type=float, default=30,
                        help="checker container deadline in seconds (1–120); not the model task budget")
    args = parser.parse_args()
    if args.candidate is None and args.outcome != "skipped":
        parser.error("candidate is required unless --outcome skipped")
    if not math.isfinite(args.assessment_timeout) or not 1 <= args.assessment_timeout <= 120:
        parser.error("assessment timeout must be between 1 and 120 seconds")
    metadata = {}
    if args.metadata:
        metadata = json.loads(bounded_read(args.metadata, 65536))
        if not isinstance(metadata, dict):
            raise ValueError("metadata must be an object")
    cancellation = Cancellation()
    previous_handlers = {sig: signal.signal(sig, cancellation)
                         for sig in (signal.SIGTERM, signal.SIGINT)}
    try:
        report, exit_code = assess(args.task, args.candidate, outcome=args.outcome,
                                   metadata=metadata, timeout=args.assessment_timeout,
                                   cancellation=cancellation)
        print(json.dumps(report, indent=2))
        return exit_code
    finally:
        for sig, handler in previous_handlers.items():
            signal.signal(sig, handler)


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, ValueError, AssessmentInterrupted) as error:
        print(f"coding-repair: {error}", file=sys.stderr)
        raise SystemExit(2)
