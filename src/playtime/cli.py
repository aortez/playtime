import argparse
import copy
import json
import signal
import subprocess
import sys
from pathlib import Path

from .common import PlaytimeError, digest_file, write_json
from .config import load_recipe, repo_path, smoke_recipe, validate
from .environment import prepare_image
from .ollama import Ollama
from .report import report
from .runner import check, run


def prepare(root, recipe, *, refresh=False, image=None):
    locked_image = prepare_image(root, recipe, refresh=refresh, image=image)
    model_path = repo_path(root, recipe["model"]["lock"])
    client = Ollama(recipe["model"]["endpoint"])
    if model_path.exists() and not refresh:
        client.check(json.loads(model_path.read_text()))
    else:
        locked_model = client.inspect(recipe["model"]["name"])
        write_json(model_path, locked_model)
    print(f"Prepared: {locked_image['image_id']}")
    print(f"Model lock: {model_path}")


def verify_smoke(directory, result, scripted):
    if result["error"] or result["cleanup_errors"] or result["stop_reason"] != "agent_finished":
        raise PlaytimeError("Smoke run did not finish cleanly; inspect its report")
    if len(result["sessions"]) != 2:
        raise PlaytimeError("Smoke run did not cross exactly one context reset")
    if (directory / "final-workspace/probe.txt").read_text().strip() != "playtime-persistent":
        raise PlaytimeError("Smoke persistence check failed")
    if scripted:
        events = [
            json.loads(line) for line in (directory / "events.jsonl").read_text().splitlines()
        ]
        results = [event["result"] for event in events if event["kind"] == "tool_result"]
        if not any(result.get("timed_out") for result in results):
            raise PlaytimeError("Smoke did not exercise a command timeout")
        if not any(
            "persistence-ok" in result.get("stdout", "") and result.get("exit_code") == 0
            for result in results
        ):
            raise PlaytimeError("Smoke did not verify temporary-state reset")
    print("Smoke passed: tools, file persistence, fresh sessions, snapshots, report, and cleanup")


def result_exit_code(result):
    if result["error"] or result["cleanup_errors"]:
        return 1
    if result["stop_reason"] == "interrupted":
        return 130
    return (
        0
        if result["stop_reason"]
        in {"agent_finished", "session_limit", "token_limit", "wall_limit", "log_limit"}
        else 1
    )


def main(argv=None):
    parser = argparse.ArgumentParser(description="Repeatable local-model workshop experiments")
    parser.add_argument(
        "--root",
        type=Path,
        default=Path(__file__).resolve().parents[2],
        help="project data root (also used when invoking an archived controller)",
    )
    commands = parser.add_subparsers(dest="command", required=True)
    prep = commands.add_parser(
        "prepare", help="build/archive workshop and lock the installed model"
    )
    prep.add_argument("recipe", type=Path)
    prep.add_argument("--refresh", action="store_true", help="deliberately replace existing locks")
    prep.add_argument("--image", help="freeze an already-built matching workshop image")
    for name in ["check", "run", "smoke"]:
        command_parser = commands.add_parser(name)
        command_parser.add_argument("recipe", type=Path)
        if name == "check":
            command_parser.add_argument(
                "--offline",
                action="store_true",
                help="check image and local files without contacting Ollama",
            )
        if name == "smoke":
            command_parser.add_argument(
                "--live",
                action="store_true",
                help="use the real model for the two-session infrastructure test",
            )
    repeat = commands.add_parser(
        "repeat", help="repeat archived starting conditions in a fresh run"
    )
    repeat.add_argument("run_directory", type=Path)
    repeat.add_argument("--sampling-seed", type=int)
    repeat.add_argument(
        "--endpoint", help="alternate Ollama URL; model identity and version must still match"
    )
    reporting = commands.add_parser(
        "report", help="regenerate an HTML report without running the agent"
    )
    reporting.add_argument("run_directory", type=Path)
    args = parser.parse_args(argv)

    def terminate(signum, frame):
        raise KeyboardInterrupt

    signal.signal(signal.SIGTERM, terminate)
    root = args.root.resolve()
    try:
        if args.command == "report":
            print(report(args.run_directory.resolve()))
            return 0
        if args.command == "repeat":
            previous = args.run_directory.resolve()
            old = json.loads((previous / "manifest.json").read_text())
            recipe = copy.deepcopy(old["recipe"])
            if args.sampling_seed is not None:
                recipe["sampling_seed"] = args.sampling_seed
            if args.endpoint:
                recipe["model"]["endpoint"] = args.endpoint
            validate(recipe)
            directory, result = run(
                root,
                recipe,
                previous=previous,
                scripted=old["kind"] == "scripted-smoke",
                smoke=old["kind"] == "live-smoke",
            )
            print(directory / "report.html")
            return result_exit_code(result)
        path = args.recipe if args.recipe.is_absolute() else root / args.recipe
        recipe = load_recipe(path)
        if args.command == "prepare":
            prepare(root, recipe, refresh=args.refresh, image=args.image)
        elif args.command == "check":
            check(root, recipe, live=not args.offline)
            for name in ["prompt", "protocol"]:
                print(f"{name}: {digest_file(repo_path(root, recipe[name]))}")
            print("Checks passed")
        else:
            is_smoke = args.command == "smoke"
            live = getattr(args, "live", False)
            if is_smoke:
                recipe = smoke_recipe(recipe, live=live)
            directory, result = run(root, recipe, scripted=is_smoke and not live, smoke=is_smoke)
            if is_smoke:
                verify_smoke(directory, result, scripted=not live)
            print(directory / "report.html")
            return result_exit_code(result)
        return 0
    except (PlaytimeError, OSError, ValueError, KeyError, subprocess.CalledProcessError) as error:
        print(f"playtime: {error}", file=sys.stderr)
        return 1
    except KeyboardInterrupt:
        print("playtime: interrupted", file=sys.stderr)
        return 130
