import html
import json
from pathlib import Path
from urllib.parse import quote


def escaped(value):
    return html.escape(
        value if isinstance(value, str) else json.dumps(value, indent=2, ensure_ascii=False)
    )


def report(directory):
    directory = Path(directory)
    manifest = json.loads((directory / "manifest.json").read_text())
    result_path = directory / "result.json"
    result = (
        json.loads(result_path.read_text())
        if result_path.exists()
        else {"stop_reason": "unfinished"}
    )
    timeline = []
    events = directory / "events.jsonl"
    if events.exists():
        with events.open() as source:
            for line in source:
                try:
                    event = json.loads(line)
                except ValueError:
                    timeline.append("<p class='error'>The final log record is incomplete.</p>")
                    break
                kind = event["kind"]
                if kind in {"model_chunk", "model_request"}:
                    continue
                title = f"{event['sequence']} · {event['time']} · {kind}"
                if "session" in event:
                    title += f" · session {event['session']}"
                body = {
                    key: value
                    for key, value in event.items()
                    if key not in {"sequence", "time", "kind"}
                }
                timeline.append(
                    f"<details><summary>{escaped(title)}</summary><pre>{escaped(body)}</pre></details>"
                )
    rows = []
    for session in result.get("sessions", []):
        number = session["session"]
        folder = f"sessions/{number:03d}"
        changes = session["changes"]
        rows.append(
            f"<tr><td>{number}</td><td>{session['generated_tokens_total']}</td>"
            f"<td>+{len(changes['added'])} / Δ{len(changes['modified'])} / −{len(changes['removed'])}</td>"
            f"<td><a href='{folder}/memento.txt'>Memento</a> · <a href='{folder}/workspace.json'>Changes</a> · "
            f"<a href='{folder}/workspace.tar' download>Snapshot</a></td></tr>"
        )
    artifacts = []
    workspace = directory / "final-workspace"
    if workspace.exists():
        for path in sorted(workspace.rglob("*")):
            if path.is_file() and not path.is_symlink():
                relative = path.relative_to(directory).as_posix()
                label = path.relative_to(workspace).as_posix()
                artifacts.append(
                    f"<li><a href='./{quote(relative)}' download>{escaped(label)}</a> "
                    f"<span>{path.stat().st_size:,} bytes</span></li>"
                )
    tokens = result.get("generated_tokens", 0)
    if not result.get("token_usage_complete", True):
        tokens = f"{tokens} (last request unaccounted)"
    document = """<!doctype html><html lang="en"><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'; base-uri 'none'">
<title>Playtime run</title>
<style>
:root{color-scheme:light dark;font-family:system-ui,sans-serif}body{max-width:1100px;margin:3rem auto;padding:0 1.5rem;line-height:1.55}
h1{margin-bottom:.2rem}h2{margin-top:2.5rem}a{color:#2387bc}pre{white-space:pre-wrap;overflow-wrap:anywhere;font-size:.83rem}
details{border:1px solid #8885;border-radius:6px;margin:.6rem 0;padding:.6rem 1rem}summary{cursor:pointer;font-weight:600}
table{border-collapse:collapse;width:100%}td,th{text-align:left;border-bottom:1px solid #8885;padding:.6rem}
.meta{display:flex;gap:1rem;flex-wrap:wrap}.meta>div{padding:1rem;border:1px solid #8885;border-radius:8px;min-width:140px}
.meta strong{display:block}li span,.subtle{opacity:.7}.error{color:#cf6150}
</style><body>"""
    document += f"<h1>Playtime · {escaped(manifest['recipe']['name'])}</h1><p class='subtle'>{escaped(manifest['id'])} · {escaped(manifest['kind'])}</p>"
    document += (
        "<div class='meta'>"
        + "".join(
            f"<div><strong>{escaped(label)}</strong>{escaped(str(value))}</div>"
            for label, value in [
                ("Stopped", result["stop_reason"]),
                ("Generated tokens", tokens),
                ("Sessions", len(result.get("sessions", []))),
                ("Elapsed seconds", result.get("elapsed_seconds", "—")),
            ]
        )
        + "</div>"
    )
    if result.get("error") or result.get("cleanup_errors"):
        document += f"<pre class='error'>{escaped([result.get('error'), result.get('cleanup_errors')])}</pre>"
    document += "<p><a href='manifest.json'>Manifest</a> · <a href='events.jsonl'>Complete event log</a> · <a href='result.json'>Result</a></p>"
    document += (
        "<h2>Starting direction</h2><pre>"
        + escaped((directory / "inputs/prompt.txt").read_text())
        + "</pre>"
    )
    document += (
        "<h2>Sessions</h2><table><thead><tr><th>Session</th><th>Total tokens</th><th>Files</th><th>Records</th></tr></thead><tbody>"
        + "".join(rows)
        + "</tbody></table>"
    )
    document += "<h2>Final artifacts</h2><ul>" + "".join(artifacts) + "</ul>"
    document += "<h2>Timeline</h2><p>Full requests and streamed chunks are preserved in the event log. Model statements can be compared with tool results and snapshots.</p>"
    document += "".join(timeline) + "</body></html>\n"
    output = directory / "report.html"
    output.write_text(document)
    return output
