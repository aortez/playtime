"""The local dashboard document. All experiment content is inserted as text."""

import base64
import hashlib
import html
from pathlib import Path

from .report_assets import STYLE

SCRIPT = Path(__file__).with_name("dashboard.js").read_text()
DASH_STYLE = Path(__file__).with_name("dashboard.css").read_text()
SCRIPT_HASH = base64.b64encode(hashlib.sha256(SCRIPT.encode()).digest()).decode()
CSP = (
    "default-src 'none'; connect-src 'self'; img-src data:; style-src 'unsafe-inline'; "
    f"script-src 'sha256-{SCRIPT_HASH}'; base-uri 'none'; form-action 'none'"
)


def document(token, run_id=""):
    return (
        '<!doctype html><html lang="en"><head><meta charset="utf-8">'
        '<meta name="viewport" content="width=device-width, initial-scale=1">'
        f'<meta http-equiv="Content-Security-Policy" content="{html.escape(CSP, quote=True)}">'
        f'<meta name="playtime-token" content="{html.escape(token, quote=True)}">'
        f"<title>Playtime · {'Live experiment' if run_id else 'Workshop'}</title>"
        f"<style>{STYLE}\n{DASH_STYLE}</style></head>"
        f'<body data-run="{html.escape(run_id, quote=True)}"><main>'
        '<header class="topline"><div><div class="eyebrow">Local experiment workshop</div>'
        '<h1><a href="/">Playtime</a></h1></div><nav class="links">'
        '<a href="/archive">Completed reports</a><span id="connection" class="meta">Connecting…</span>'
        '</nav></header><p id="error" class="notice error" role="alert" hidden></p>'
        '<noscript><p>This dashboard needs JavaScript. <a href="/archive">Open the report archive</a>.</p></noscript>'
        '<section id="launcher" class="panel" hidden><div class="row"><h2>Start an experiment</h2>'
        '<span class="badge">One run at a time</span></div>'
        '<div class="launch-fields"><label>Recipe<select id="recipe"></select></label>'
        '<label>Run type<select id="mode"><option value="experiment">Experiment</option>'
        '<option value="live-smoke">Two-session model check</option>'
        '<option value="scripted-smoke">Scripted infrastructure check</option></select></label></div>'
        '<p id="recipe-meta" class="meta"></p><pre id="prompt" class="prompt"></pre>'
        '<details><summary>Session protocol and full settings</summary><pre id="protocol"></pre>'
        '<pre id="settings"></pre></details><div class="launch-actions row">'
        '<span id="launch-note" class="meta">Inputs are frozen when you launch.</span>'
        '<button id="launch" class="primary" disabled>Launch experiment</button></div></section>'
        '<section id="runs-panel" hidden><div class="row"><h2>Experiments</h2>'
        '<span class="meta">Live runs and saved history</span></div><div id="runs"></div></section>'
        '<section id="watch" hidden><div class="watch-head"><div class="row">'
        '<div><h2 id="run-title">Loading experiment…</h2><div id="run-id" class="meta"></div></div>'
        '<div class="links"><a id="report-link" hidden>Open final report →</a>'
        '<button id="stop" class="danger" disabled>Stop run</button></div></div>'
        '<div class="stats"><div class="stat"><strong id="phase">Starting</strong><span id="activity">Waiting for worker</span></div>'
        '<div class="stat"><strong id="session">—</strong><span>Session / maximum</span></div>'
        '<div class="stat"><strong id="time">—</strong><span id="time-label">Time remaining</span></div>'
        '<div class="stat"><strong id="tokens">0</strong><span id="token-detail">Confirmed generated tokens</span></div>'
        '<div class="stat"><strong id="health">Checking…</strong><span id="health-detail">Model device</span></div></div>'
        '<p id="run-error" class="notice error" hidden></p><div class="row live-toolbar">'
        '<span class="meta">Model output streams live. Tool output appears when the tool returns.</span>'
        '<button id="follow" aria-pressed="true">Following live</button></div></div>'
        '<div class="watch-grid"><section><h2>Activity</h2><div id="timeline"></div>'
        '<p id="waiting" class="notice">Preparing the workshop and checking the locked model…</p></section>'
        '<aside><h2>Checkpoints</h2><p class="meta">Images, files, and saved memory appear after each session.</p>'
        '<div id="checkpoints"></div></aside></div></section>'
        '<dialog id="preview"><div class="row"><strong id="preview-name"></strong>'
        '<button id="close-preview">Close</button></div><img id="preview-image" alt="">'
        '<a id="download-image" download>Download image</a></dialog>'
        f"</main><script>{SCRIPT}</script></body></html>"
    )
