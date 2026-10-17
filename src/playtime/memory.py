"""Evidence-only memory comparisons shared by saved and live reports."""

import difflib
import json


def compare_memory(received, saved, handoff_saved):
    before, after = received.get("content", ""), saved.get("content", "")
    status = saved.get("status", "unrecorded")
    changed = before != after
    if "FileNotFoundError" in status:
        state, label = "missing", "Missing memento"
        detail = "No MEMENTO.md was available at the checkpoint."
    elif status != "loaded":
        state, label = "unavailable", "Memento unavailable"
        detail = status
    elif not after.strip():
        state, label = "empty", "Empty memento"
        detail = "The checkpoint contains no usable memory text."
    elif handoff_saved:
        state = "saved"
        label = "Fresh handoff" if changed else "Saved again · unchanged"
        detail = "end_session saved a memento in this session."
    elif changed:
        state, label = "updated", "Changed outside handoff"
        detail = "Memory text changed, but no successful end_session call was recorded."
    else:
        state, label = "unchanged", "Unchanged · no handoff"
        detail = "The same memory was carried forward without a successful end_session call."
    comparable = (
        received.get("status") == "loaded" or "FileNotFoundError" in received.get("status", "")
    ) and status == "loaded"
    diff = (
        "\n".join(
            difflib.unified_diff(
                before.splitlines(),
                after.splitlines(),
                fromfile="Memory received",
                tofile="Memory at checkpoint",
                lineterm="",
            )
        )
        if comparable and changed
        else ""
    )
    return {
        "state": state,
        "label": label,
        "detail": detail,
        "handoff_saved": handoff_saved,
        "changed": changed if comparable else None,
        "received": received,
        "saved": saved,
        "diff": diff,
    }


def checkpoint_memory(directory, number, summary, *, received=None, handoff_saved=None):
    folder = directory / "sessions" / f"{number:03d}"
    if received is None:
        path = folder / "memory-received.json"
        if path.is_file():
            received = json.loads(path.read_text())
    if handoff_saved is None:
        handoff_saved = summary.get("handoff_saved")
    # Older archives have this evidence only in their append-only event log.
    if received is None or handoff_saved is None:
        path = directory / "events.jsonl"
        recorded_handoff = False
        if path.is_file():
            with path.open() as source:
                for line in source:
                    try:
                        event = json.loads(line)
                    except ValueError:
                        break
                    if event.get("session") != number:
                        continue
                    if event["kind"] == "session_start" and received is None:
                        received = event.get("memory", {})
                    if event["kind"] == "tool_result" and event.get("name") == "end_session":
                        result = event.get("result", {})
                        recorded_handoff |= "bytes_written" in result and "error" not in result
        if handoff_saved is None:
            handoff_saved = recorded_handoff
    path = folder / "memento.txt"
    saved = {
        "content": path.read_text() if path.is_file() else "",
        "status": summary.get("memento_status", "unrecorded"),
    }
    return compare_memory(received or {"content": "", "status": "unrecorded"}, saved, handoff_saved)
