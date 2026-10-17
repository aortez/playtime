"""Incremental, read-only projections of run records for the dashboard."""

import json

from .common import PlaytimeError
from .jobs import read_json
from .ollama import Ollama
from .report_previews import PreviewBudget, checkpoint_images


def project(event):
    result = {
        key: event[key] for key in ("sequence", "time", "kind", "session", "call") if key in event
    }
    kind = event["kind"]
    if kind == "model_request":
        result.update(
            maximum=event["payload"].get("options", {}).get("num_predict", 0),
            handoff=event.get("handoff", False),
            seconds_remaining=event.get("seconds_remaining"),
        )
    elif kind == "model_chunk":
        message = event.get("chunk", {}).get("message", {})
        result["message"] = {key: message[key] for key in ("thinking", "content") if key in message}
    elif kind == "model_response":
        response = event["response"]
        result.update(
            generated=event.get("generated_tokens_total", 0),
            charged=event.get("token_budget_charged_total", event.get("generated_tokens_total", 0)),
            eval_count=response.get("eval_count"),
            prompt_eval_count=response.get("prompt_eval_count"),
            message={
                key: response.get("message", {}).get(key, "") for key in ("thinking", "content")
            },
        )
    else:
        result.update(
            {key: value for key, value in event.items() if key not in {"recipe", "payload"}}
        )
    return result


def events_since(directory, cursor):
    if not isinstance(cursor, int) or cursor < 0:
        raise PlaytimeError("Invalid event cursor")
    path = directory / "events.jsonl"
    if not path.exists():
        return {"events": [], "cursor": 0, "more": False}
    events = []
    with path.open("rb") as source:
        size = path.stat().st_size
        if cursor > size:
            raise PlaytimeError("Event cursor is past the end of the log; reload this page")
        if cursor:
            source.seek(cursor - 1)
            if source.read(1) != b"\n":
                raise PlaytimeError("Event cursor must be at a record boundary")
        source.seek(cursor)
        consumed = 0
        while len(events) < 500 and consumed < 2 * 1024 * 1024:
            line = source.readline(16 * 1024 * 1024)
            if not line.endswith(b"\n"):
                if len(line) == 16 * 1024 * 1024:
                    raise PlaytimeError("Event is too large for live viewing; use the raw log")
                break  # A writer may still be appending this record.
            cursor = source.tell()
            consumed += len(line)
            events.append(project(json.loads(line)))
    return {"events": events, "cursor": cursor, "more": cursor < size and bool(events)}


def checkpoint(directory, number):
    if not 1 <= number <= 1000:
        raise PlaytimeError("Invalid session")
    folder = directory / "sessions" / f"{number:03d}"
    summary = read_json(folder / "summary.json")
    if not summary:
        raise PlaytimeError("Checkpoint is not ready")
    images, notice = checkpoint_images(
        directory, number, summary.get("changes", {}), PreviewBudget()
    )
    return {
        "session": number,
        "summary": summary,
        "memento": (folder / "memento.txt").read_text(),
        "images": images,
        "notice": notice,
    }


def model_health(recipe):
    try:
        models = Ollama(recipe["model"]["endpoint"]).request("/api/ps", timeout=3)["models"]
        loaded = next((model for model in models if model["name"] == recipe["model"]["name"]), None)
        if not loaded:
            return {"state": "unloaded", "label": "Model not loaded"}
        size, vram = loaded.get("size", 0), loaded.get("size_vram", 0)
        fraction = min(100, round(100 * vram / size)) if size else 0
        return {
            "state": "cpu" if not vram else "gpu",
            "size_vram": vram,
            "context": loaded.get("context_length"),
            "label": "CPU only" if not vram else f"{fraction}% GPU · {vram / 1024**3:.1f} GiB",
        }
    except (PlaytimeError, KeyError, TypeError, ValueError) as error:
        return {"state": "unavailable", "label": "Model server unavailable", "error": str(error)}
