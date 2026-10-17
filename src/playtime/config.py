import copy
import tomllib
from pathlib import Path

from .common import PlaytimeError

TOOLSETS = {"core", "computation", "browser", "systems"}
ROOT_KEYS = {
    "name",
    "prompt",
    "protocol",
    "seeds",
    "reset_mode",
    "sampling_seed",
    "sessions",
    "work_turns_per_session",
    "workshop",
    "model",
    "limits",
}
GROUP_KEYS = {
    "workshop": {"toolsets", "lock", "cpus", "memory_mb", "workspace_mb", "tmp_mb", "pids"},
    "model": {
        "endpoint",
        "name",
        "lock",
        "think",
        "context_tokens",
        "temperature",
        "top_p",
        "top_k",
        "repeat_penalty",
    },
    "limits": {
        "wall_seconds",
        "generated_tokens",
        "tokens_per_call",
        "handoff_tokens",
        "model_seconds",
        "command_seconds",
        "output_bytes",
        "memento_bytes",
        "log_mb",
    },
}


def validate(recipe):
    if set(recipe) != ROOT_KEYS:
        raise PlaytimeError(f"Recipe keys differ from schema: {set(recipe) ^ ROOT_KEYS}")
    for group, expected in GROUP_KEYS.items():
        optional = {"tool_parse_retries", "tool_parse_error_limit"} if group == "limits" else set()
        if not isinstance(recipe[group], dict) or set(recipe[group]) - optional != expected:
            raise PlaytimeError(f"Invalid keys in [{group}]; expected {sorted(expected)}")
    if recipe["reset_mode"] != "files_only":
        raise PlaytimeError("Only reset_mode='files_only' is currently implemented")
    for key in ["name", "prompt", "protocol"]:
        if not isinstance(recipe[key], str) or not recipe[key]:
            raise PlaytimeError(f"{key} must be a nonempty string")
    if not isinstance(recipe["seeds"], str):
        raise PlaytimeError("seeds must be a path or an empty string")
    for key, maximum in [
        ("sessions", 1000),
        ("work_turns_per_session", 1000),
        ("sampling_seed", 2**31 - 1),
    ]:
        minimum = 0 if key == "sampling_seed" else 1
        if type(recipe[key]) is not int or not minimum <= recipe[key] <= maximum:
            raise PlaytimeError(f"Invalid {key}")
    workshop = recipe["workshop"]
    if (
        not isinstance(workshop["toolsets"], list)
        or not workshop["toolsets"]
        or not all(isinstance(x, str) for x in workshop["toolsets"])
        or not set(workshop["toolsets"]) <= TOOLSETS
        or "core" not in workshop["toolsets"]
        or len(set(workshop["toolsets"])) != len(workshop["toolsets"])
    ):
        raise PlaytimeError("toolsets must be unique supported collections including core")
    for key in ["memory_mb", "workspace_mb", "tmp_mb", "pids"]:
        if type(workshop[key]) is not int or not 1 <= workshop[key] <= 65536:
            raise PlaytimeError(f"Invalid workshop.{key}")
    if not 0.1 <= workshop["cpus"] <= 64:
        raise PlaytimeError("workshop.cpus must be between 0.1 and 64")
    if workshop["workspace_mb"] + workshop["tmp_mb"] >= workshop["memory_mb"]:
        raise PlaytimeError("Leave process memory beyond workspace_mb + tmp_mb")
    for key, value in recipe["limits"].items():
        if key == "tool_parse_retries":
            if type(value) is not int or not 0 <= value <= 10:
                raise PlaytimeError("limits.tool_parse_retries must be an integer from 0 to 10")
            continue
        if key == "tool_parse_error_limit":
            if type(value) is not int or not 1 <= value <= 100:
                raise PlaytimeError(
                    "limits.tool_parse_error_limit must be an integer from 1 to 100"
                )
            continue
        if type(value) is not int or value < 1:
            raise PlaytimeError(f"limits.{key} must be a positive integer")
    limits = recipe["limits"]
    if not 1024 <= limits["output_bytes"] <= 65536:
        raise PlaytimeError("output_bytes must be between 1024 and 65536")
    if limits["memento_bytes"] > 32768:
        raise PlaytimeError("memento_bytes cannot exceed 32768")
    if limits["generated_tokens"] <= limits["handoff_tokens"]:
        raise PlaytimeError("generated_tokens must leave room beyond the handoff reserve")
    model = recipe["model"]
    if (
        type(model["context_tokens"]) is not int
        or model["context_tokens"] < limits["tokens_per_call"] + limits["memento_bytes"] + 4096
    ):
        raise PlaytimeError("context_tokens is too small for the configured input/output reserves")
    if not 0 <= model["temperature"] <= 2 or not 0 < model["top_p"] <= 1:
        raise PlaytimeError("Invalid sampling parameters")
    if type(model["top_k"]) is not int or model["top_k"] < 0 or model["repeat_penalty"] <= 0:
        raise PlaytimeError("Invalid sampling parameters")
    return recipe


def load_recipe(path):
    with Path(path).open("rb") as source:
        return validate(tomllib.load(source))


def repo_path(root, name):
    path = (Path(root) / name).resolve()
    root = Path(root).resolve()
    if root not in path.parents:
        raise PlaytimeError(f"Input path must be inside the project: {name}")
    return path


def smoke_recipe(recipe, *, live=False):
    result = copy.deepcopy(recipe)
    result["name"] = "live-smoke" if live else "scripted-smoke"
    result["sessions"] = 2
    result["work_turns_per_session"] = 3
    result["limits"].update(
        wall_seconds=300,
        generated_tokens=12000,
        tokens_per_call=2048,
        handoff_tokens=1024,
        command_seconds=5,
        model_seconds=90,
    )
    result["seeds"] = ""
    return validate(result)
