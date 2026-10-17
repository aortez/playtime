import base64
import hashlib
import html
import json
import tempfile
from pathlib import Path
from urllib.parse import quote

from .report_assets import SCRIPT, STYLE

SCRIPT_HASH = base64.b64encode(hashlib.sha256(SCRIPT.encode()).digest()).decode()
CSP = (
    "default-src 'none'; style-src 'unsafe-inline'; "
    f"script-src 'sha256-{SCRIPT_HASH}'; base-uri 'none'; form-action 'none'"
)


def escaped(value):
    return html.escape(
        value if isinstance(value, str) else json.dumps(value, indent=2, ensure_ascii=False)
    )


def page(title, body, *, refresh=False, controls=False):
    return (
        '<!doctype html><html lang="en"><head><meta charset="utf-8">'
        '<meta name="viewport" content="width=device-width, initial-scale=1">'
        f'<meta http-equiv="Content-Security-Policy" content="{escaped(CSP)}">'
        + ('<meta http-equiv="refresh" content="15">' if refresh else "")
        + f"<title>{escaped(title)}</title><style>{STYLE}</style></head>"
        '<body data-view="readable"><main>'
        + body
        + "</main>"
        + (f"<script data-report-controls>{SCRIPT}</script>" if controls else "")
        + "</body></html>\n"
    )


def write_page(path, document):
    # A viewer must never receive half a report while it is being regenerated.
    path = Path(path)
    with tempfile.NamedTemporaryFile(
        mode="w", encoding="utf-8", dir=path.parent, delete=False
    ) as f:
        temporary = Path(f.name)
        try:
            f.write(document)
            f.close()
            temporary.replace(path)
        finally:
            temporary.unlink(missing_ok=True)
    return path


def excerpt(value, *, characters=700, lines=8):
    text = value if isinstance(value, str) else json.dumps(value, ensure_ascii=False, indent=2)
    short = "\n".join(text.splitlines()[:lines])[:characters]
    return text, short, len(short) < len(text.rstrip("\n"))


def text_block(label, value):
    text, short, shortened = excerpt(value)
    body = f'<div class="text-block"><div class="text-label">{escaped(label)}</div>'
    body += f'<pre class="preview">{escaped(short if shortened else text)}</pre>'
    if shortened:
        body += (
            "<details><summary>Excerpt shown · Read full text "
            f"({len(text):,} characters)</summary><pre>{escaped(text)}</pre></details>"
        )
    return body + "</div>"


def raw_details(records):
    return (
        '<details class="raw"><summary>Raw records</summary>'
        + f"<pre>{escaped(records)}</pre></details>"
    )


def badge(text, style=""):
    return f'<span class="badge {style}">{escaped(text)}</span>'


def load_timeline(directory):
    """Group recorded turns without inventing a model-authored summary.

    Keep only request metadata and assemble streaming text for incomplete calls.
    The exact requests and chunks remain in events.jsonl, avoiding repeated full
    conversations in the HTML for every turn.
    """
    sessions, notices = {}, []
    turns = {}
    current = None
    path = directory / "events.jsonl"
    if not path.exists():
        return sessions, notices
    with path.open(encoding="utf-8") as source:
        for line in source:
            try:
                event = json.loads(line)
                if not isinstance(event, dict) or "kind" not in event:
                    raise ValueError("Invalid event")
            except ValueError:
                notices.append(
                    "The final log record is incomplete; the preceding records are shown."
                )
                break
            kind = event["kind"]
            number = event.get("session")
            if number is None:
                if kind not in {"run_start", "run_end"}:
                    notices.append(event)
                continue
            session = sessions.setdefault(
                number, {"start": {}, "checkpoint": {}, "turns": [], "notices": []}
            )
            if kind == "session_start":
                session["start"] = event
                current = None
            elif kind in {"model_request", "model_chunk", "model_response"}:
                key = (number, event.get("call", event.get("sequence", 0)))
                if key not in turns:
                    turns[key] = {
                        "call": event.get("call", "?"),
                        "time": event.get("time", ""),
                        "request": {},
                        "response": None,
                        "stream": {"content": "", "thinking": ""},
                        "tools": [],
                    }
                    session["turns"].append(turns[key])
                current = turns[key]
                if kind == "model_request":
                    payload = event.get("payload", {})
                    current["request"] = {
                        **{k: v for k, v in event.items() if k != "payload"},
                        "message_count": len(payload.get("messages", [])),
                        "options": payload.get("options", {}),
                    }
                elif kind == "model_chunk":
                    message = event.get("chunk", {}).get("message", {})
                    for field in ["thinking", "content"]:
                        current["stream"][field] += message.get(field, "")
                else:
                    current["response"] = event
                    current["stream"] = {}
            elif kind == "tool_request":
                if current is None:
                    current = {
                        "call": "?",
                        "time": event.get("time", ""),
                        "request": {},
                        "response": None,
                        "stream": {},
                        "tools": [],
                    }
                    session["turns"].append(current)
                current["tools"].append({"request": event, "result": None})
            elif kind == "tool_result":
                match = next(
                    (
                        tool
                        for tool in (current or {}).get("tools", [])
                        if tool["result"] is None
                        and tool["request"].get("name") == event.get("name")
                    ),
                    None,
                )
                if match is not None:
                    match["result"] = event
                else:
                    session["notices"].append(event)
            elif kind == "checkpoint":
                session["checkpoint"] = event
            else:
                session["notices"].append(event)
    return sessions, notices


def tool_label(tool):
    request = tool["request"]
    name, arguments = request.get("name"), request.get("arguments", {})
    arguments = arguments if isinstance(arguments, dict) else {}
    if name == "exec":
        command = str(arguments.get("command", ""))
        first = command.splitlines()[0] if command else ""
        return "Run · " + first[:120] + ("…" if len(command) > len(first[:120]) else "")
    if name in {"read_file", "write_file"}:
        return ("Read · " if name == "read_file" else "Write · ") + str(arguments.get("path", ""))
    if name == "end_session":
        return "Save memento & " + ("finish run" if arguments.get("finish_run") else "end session")
    return str(name or "Unknown tool")


def tool_status(tool):
    if tool["result"] is None:
        return "No result recorded", "warn"
    result = tool["result"].get("result", {})
    if result.get("error"):
        return "Tool error", "bad"
    if result.get("timed_out"):
        return "Timed out", "bad"
    if "exit_code" in result:
        return f"Exit {result['exit_code']}", "good" if result["exit_code"] == 0 else "bad"
    if "bytes_written" in result:
        return f"Saved {result['bytes_written']:,} bytes", "good"
    if "content" in result:
        return f"Read {len(result['content'].encode('utf-8')):,} bytes", "good"
    return "Result recorded", ""


def render_tool(tool):
    request = tool["request"]
    arguments = request.get("arguments", {})
    arguments = arguments if isinstance(arguments, dict) else {}
    result = (tool["result"] or {}).get("result", {})
    status, style = tool_status(tool)
    body = f'<div class="tool {"failed" if style == "bad" else ""}"><div class="row">'
    body += f"<strong>{escaped(tool_label(tool))}</strong>{badge(status, style)}</div>"
    for key, label in [
        ("command", "Command"),
        ("content", "Content written"),
        ("memento", "Handoff requested"),
    ]:
        if key in arguments:
            body += text_block(label, arguments[key])
    for key, label in [
        ("stdout", "Standard output"),
        ("stderr", "Standard error"),
        ("content", "Content read"),
        ("error", "Error"),
    ]:
        if result.get(key):
            body += text_block(label, result[key])
    if request.get("name") == "read_file":
        body += f'<p class="tool-status">Byte offset: {escaped(result.get("offset_bytes", arguments.get("offset_bytes", 0)))}</p>'
    if result.get("truncated") or result.get("output_truncated"):
        body += (
            '<p class="notice">The tool truncated its output before delivering it to the model.</p>'
        )
    if tool["result"] is None:
        body += '<p class="notice">The log contains this request but no corresponding result.</p>'
    return body + "</div>"


def render_turn(turn):
    response = turn["response"]
    payload = (response or {}).get("response", {})
    message = payload.get("message", {}) if response else turn["stream"]
    if response and not message:
        message = response
    titles = [tool_label(tool) for tool in turn["tools"]]
    title = (
        " · ".join(titles)
        if titles
        else ("Model response" if response else "Incomplete model call")
    )
    statuses = [tool_status(tool)[1] for tool in turn["tools"]]
    status = (
        "Tool error"
        if "bad" in statuses
        else ("Incomplete" if not response or "warn" in statuses else "")
    )
    tokens = payload.get("eval_count")
    count = f"{tokens:,} tokens" if tokens is not None else "Token usage unknown"
    timing = str(turn["time"])[11:19] if "T" in str(turn["time"]) else str(turn["time"])
    body = '<article class="step"><div class="step-heading">'
    body += f'<span class="step-number">CALL {escaped(turn["call"])}</span><span class="step-title">{escaped(title)}</span>'
    if status:
        body += badge(status, "bad" if "bad" in statuses else "warn")
    body += f'<span class="meta">{escaped(count)} · {escaped(timing)} UTC</span></div><div class="step-body">'
    for key, label in [("thinking", "Model reasoning"), ("content", "Model message")]:
        if message.get(key):
            body += text_block(label, message[key])
    if not response:
        body += '<p class="notice">Incomplete response. Any text shown is the recorded stream; final token usage is unknown.</p>'
    body += "".join(render_tool(tool) for tool in turn["tools"])
    body += raw_details(
        {"request_metadata": turn["request"], "response": response, "tools": turn["tools"]}
    )
    return body + "</div></article>"


def render_session(directory, number, session, previous_tokens):
    checkpoint = session["checkpoint"]
    total = checkpoint.get("generated_tokens_total")
    count = (
        f"{total - previous_tokens:,} tokens this session"
        if total is not None
        else "Token total unavailable"
    )
    if total is not None and any(turn["response"] is None for turn in session["turns"]):
        count = f"{total - previous_tokens:,} + unknown tokens this session"
    folder = f"sessions/{number:03d}"
    body = f'<section class="session" id="session-{number}"><header class="session-head row">'
    body += f'<h2>Session {number}</h2><span class="meta">{len(session["turns"])} calls · {escaped(count)}</span></header>'
    memory = session["start"].get("memory", {})
    body += '<div class="memory-grid"><div>' + text_block(
        "Memory received", memory.get("content") or "No memento supplied."
    )
    memory_status = memory.get("status", "unrecorded")
    if memory_status != "loaded" and "FileNotFoundError" not in memory_status:
        body += f'<p class="notice">{escaped(memory_status)}</p>'
    body += "</div><div>"
    memento = directory / folder / "memento.txt"
    if memento.is_file():
        body += text_block(
            "Memory saved at checkpoint", memento.read_text(encoding="utf-8") or "Empty memento."
        )
        if checkpoint.get("memento_status", "loaded") != "loaded":
            body += f'<p class="notice">{escaped(checkpoint["memento_status"])}</p>'
        body += f'<a href="{folder}/memento.txt" class="meta">Open memento</a>'
    else:
        body += text_block("Memory saved at checkpoint", "No checkpoint memento recorded.")
    body += '</div></div><div class="steps">'
    body += "".join(render_turn(turn) for turn in session["turns"])
    for notice in session["notices"]:
        body += f'<div class="notice">{escaped(notice.get("kind", "Notice"))}{raw_details(notice)}</div>'
    body += '</div><div class="session-footer">'
    changes = checkpoint.get("changes", {})
    files = [
        f"{symbol} {name}"
        for key, symbol in [("added", "+"), ("modified", "Δ"), ("removed", "−")]
        for name in changes.get(key, [])
    ]
    body += f'<div class="row"><strong>{len(files)} file changes</strong><div class="links">'
    if (directory / folder / "workspace.tar").exists():
        body += f'<a href="{folder}/workspace.tar" download>Snapshot</a>'
    if (directory / folder / "summary.json").exists():
        body += f'<a href="{folder}/summary.json">Checkpoint record</a>'
    body += "</div></div>"
    if files:
        body += text_block("Files at checkpoint", "\n".join(files))
        if number == 1:
            body += '<p class="muted">The first checkpoint includes any starting seed files.</p>'
    if checkpoint.get("interrupted"):
        body += '<p class="notice">This checkpoint was taken during interruption or shutdown.</p>'
    body += raw_details({"session_start": session["start"], "checkpoint": checkpoint})
    return body + "</div></section>"


def read_result(directory):
    path = directory / "result.json"
    return (
        json.loads(path.read_text(encoding="utf-8"))
        if path.exists()
        else {"stop_reason": "unfinished"}
    )


def token_label(result):
    tokens = result.get("generated_tokens")
    label = f"{tokens:,}" if tokens is not None else "Unknown"
    return label if result.get("token_usage_complete", True) else label + " + unknown"


def report(directory):
    directory = Path(directory)
    manifest = json.loads((directory / "manifest.json").read_text(encoding="utf-8"))
    result = read_result(directory)
    sessions, notices = load_timeline(directory)
    for summary in result.get("sessions", []):
        number = summary["session"]
        session = sessions.setdefault(
            number, {"start": {}, "checkpoint": {}, "turns": [], "notices": []}
        )
        if not session["checkpoint"]:
            session["checkpoint"] = summary
    recipe = manifest["recipe"]
    body = (
        '<div class="topline"><a href="../index.html">← All runs</a>'
        + badge(manifest["kind"])
        + "</div>"
    )
    body += f'<h1>{escaped(recipe["name"])}</h1><p class="meta">{escaped(manifest["id"])}</p>'
    model = recipe.get("model", {}).get("name", "Unknown model")
    body += f'<p class="meta">{escaped(model)} · {escaped(", ".join(recipe.get("workshop", {}).get("toolsets", [])))}</p>'
    stats = [
        ("Stop reason", result["stop_reason"]),
        ("Generated tokens", token_label(result)),
        ("Sessions", len(sessions)),
        ("Model calls", result.get("model_calls", sum(len(s["turns"]) for s in sessions.values()))),
        ("Elapsed seconds", result.get("elapsed_seconds", "—")),
    ]
    body += (
        '<div class="stats">'
        + "".join(
            f'<div class="stat"><strong>{escaped(str(value))}</strong><span>{escaped(label)}</span></div>'
            for label, value in stats
        )
        + "</div>"
    )
    if not result.get("token_usage_complete", True):
        body += '<p class="notice">Generated tokens include only accounted requests. The final request has unknown usage.</p>'
    if result.get("error") or result.get("cleanup_errors"):
        body += (
            '<div class="notice error">'
            + text_block("Run errors", [result.get("error"), result.get("cleanup_errors")])
            + "</div>"
        )
    body += '<nav class="links"><a href="manifest.json">Manifest</a><a href="events.jsonl">Complete event log</a>'
    if (directory / "result.json").exists():
        body += '<a href="result.json">Result</a>'
    body += '</nav><section class="panel"><h2>Starting direction</h2>'
    body += (
        text_block(
            "Prompt supplied to this run",
            (directory / "inputs/prompt.txt").read_text(encoding="utf-8"),
        )
        + "</section>"
    )
    body += '<div class="toolbar"><div class="row"><strong>Session trace</strong><div class="controls" role="group" aria-label="Trace detail level">'
    for view in ["compact", "readable", "full"]:
        body += f'<button type="button" data-set-view="{view}" aria-pressed="{str(view == "readable").lower()}">{view.title()}</button>'
    body += '</div></div><nav class="session-nav" aria-label="Sessions">'
    body += (
        "".join(f'<a href="#session-{number}">Session {number}</a>' for number in sessions)
        + "</nav></div>"
    )
    body += '<p class="muted">Recorded model text, actions, and tool results. Long passages show labeled excerpts; full requests and stream chunks are in the event log.</p>'
    for notice in notices:
        body += f'<p class="notice error">{escaped(notice)}</p>'
    previous_tokens = 0
    for number, session in sessions.items():
        body += render_session(directory, number, session, previous_tokens)
        previous_tokens = session["checkpoint"].get("generated_tokens_total", previous_tokens)
    if not sessions:
        body += '<p class="empty">No session records yet.</p>'
    artifacts = []
    workspace = directory / "final-workspace"
    if workspace.exists():
        for path in sorted(workspace.rglob("*")):
            if path.is_file() and not path.is_symlink():
                relative = path.relative_to(directory).as_posix()
                label = path.relative_to(workspace).as_posix()
                artifacts.append(
                    f'<li><a href="./{quote(relative, safe="/")}" download>{escaped(label)}</a><span>{path.stat().st_size:,} bytes</span></li>'
                )
    body += (
        '<section class="panel"><h2>Final artifacts</h2><ul class="artifact-list">'
        + "".join(artifacts)
        + "</ul>"
    )
    if not artifacts:
        body += '<p class="muted">No final artifacts recorded.</p>'
    body += "</section><footer>Playtime · Report generated from saved records. View controls do not change the experiment.</footer>"
    return write_page(
        directory / "report.html", page(f"Playtime · {recipe['name']}", body, controls=True)
    )


def available_runs(directory):
    records = []
    for path in sorted(Path(directory).iterdir(), reverse=True):
        if not path.is_dir() or path.is_symlink() or not (path / "report.html").is_file():
            continue
        try:
            manifest = json.loads((path / "manifest.json").read_text(encoding="utf-8"))
            result = read_result(path)
            if not isinstance(manifest["recipe"]["name"], str) or not isinstance(
                result["stop_reason"], str
            ):
                continue
        except (OSError, ValueError, KeyError, TypeError):
            continue
        records.append((path, manifest, result))
    return records


def gallery(directory):
    records = available_runs(directory)
    body = '<div class="eyebrow">Local experiment archive</div><h1>Playtime</h1>'
    body += '<p class="muted">A workshop, fresh sessions, and the record of what happened.</p>'
    if records:
        body += f'<p><a href="./{quote(records[0][0].name, safe="")}/report.html">Open latest report →</a></p>'
    body += '<p class="meta">New reports appear automatically. This gallery refreshes every 15 seconds.</p>'
    for path, manifest, result in records:
        body += f'<a class="run-card" href="./{quote(path.name, safe="")}/report.html"><div class="row">'
        body += (
            badge(manifest.get("kind", "unknown"))
            + badge(result["stop_reason"], "bad" if result.get("error") else "")
            + "</div>"
        )
        body += f'<h2>{escaped(manifest["recipe"]["name"])}</h2><div class="meta">{escaped(manifest.get("id", path.name))}</div>'
        body += f'<div class="meta">{escaped(token_label(result))} generated tokens · {len(result.get("sessions", []))} sessions · {escaped(str(result.get("elapsed_seconds", "—")))} seconds</div>'
        body += "</a>"
    if not records:
        body += '<div class="panel empty">No reports yet. Completed runs will appear here.</div>'
    return page("Playtime · All runs", body, refresh=True)


def publish(directory):
    output = report(directory)
    write_page(output.parent.parent / "index.html", gallery(output.parent.parent))
    return output
