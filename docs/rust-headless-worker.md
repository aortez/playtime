# Headless editing worker (protocol 1)

`playtime-task` runs one assignment through the same persistent Session,
compaction controller, async driver, Strata transport and Docker project tools
as chat. It is a separate binary in `playtime-chat`, so console rendering does
not define the worker protocol. It is distinct from the experiment-oriented
`playtime-worker` bundle.

Install with `PLAYTIME_CHAT_INSTALL_DIR=/private/package
scripts/install-chat-launcher.sh` (one shell command). The package includes both
chat and task binaries; install candidates separately from daily-use chat.

A trusted caller prepares a new private run directory and UTF-8 request file
outside the project. Supply the same request on stdin, after recording process
ownership:

```sh
/private/package/run-strata.py framework-gyro-m-fast --worker \
  --environment general --protocol-version 1 \
  --workspace /project/checkout --run-dir /private/run \
  --request /private/run/prompt.txt < /private/run/prompt.txt
```

The run directory and state ancestors must be owned and protected from other
users' writes; run/session directories must be mode 0700. `--check-inputs` checks
local paths, configuration and completed-session compatibility before inference.
The run directory must not have been invoked before; `artifacts/` must be empty.
`--environment /private/project-environment.lock.json` selects a previously
prepared project image. Worker launches never open a picker or implicitly build
an environment. The wrapper validates arguments and exact stdin request before
SSH, verifies installed protocol capability, then owns and verifies inference
as it does for chat. The host owner may use the operator's SSH agent; the worker
child has `SSH_AUTH_SOCK` removed, and project containers never mount that
socket. Busy inference owners are never evicted.

`playtime-task --describe-protocol` is local and requires no inference. Version 1
supports editing workers, project-docker-v1 tools, worker-report-v1 JSON reports
and completed-session-only continuation. Unsupported protocol versions fail.

## Report and evidence

Stdout is JSONL, schema version 1: one `worker_started`, shared `runtime` events,
and one `worker_finished` or `worker_failed`. A successful completion requires
`AgentFinished`, confirmed cleanup and a valid report. Startup/validation errors
may end without a terminal event: callers must reject partial streams and
nonzero exit codes. Diagnostics go to stderr.

The canonical report is one strict JSON object with `status`, `summary`, `changes`,
`verification`, `remaining_work`, `questions` and `blockers`. Status is
`checkpoint`, `completed`, `needs_input` or `blocked`. Completed requires at
least one passing check and no unresolved work. Questions/blockers are required
for their respective statuses; a report never supplies independent authority.

Each real exec result is retained under private `artifacts/tool-N.json` before
it is delivered to the model. Its additive `evidence_log_path` remains present
when the context fitter shortens output. Reports copy the exact command, exit
code and reference; validation rejects missing, escaped, timed-out or mismatched
evidence. These logs are not writable or readable by project tools. Aggregate
artifact storage is bounded at 64 MiB; failures stop the turn through the shared
driver and still clean up. Tool output retains the existing per-command cap;
these logs are actual adapter responses, not necessarily uncapped process output.

A model response may have a prose preface before its single trailing JSON object.
Normalization accepts only that complete object; multiple objects, truncated
JSON, trailing non-whitespace and invalid schema/evidence remain errors. The
unmodified model response is always saved before parsing. No extra model call
or automatic repair attempt is made.

The run retains `worker-invoked.json`, `worker-events.jsonl`, raw/validated report
files and `worker-result.json`. Successful terminal receipts include the session,
turn, cleanup, status, usage and SHA-256 of the report. The caller verifies both
receipt and artifacts. Partial evidence remains available after failure.

## Session and lifecycle

Workers use opaque `ptw-` IDs followed by 64 lowercase hex digits, under
`~/.local/state/playtime-chat` unless a trusted `--state-dir` is supplied. They
share the console's project lease root, so callers should keep that default.
Resume with `--worker-session ID` only after a completed turn. Model, workspace,
image and limits must match. Missing/incompatible sessions fail without a fresh
fallback. Chat/Codex IDs cannot be adopted as worker sessions.

SIGINT, SIGTERM and SIGHUP cancel the shared driver and clean project tools before
releasing remote ownership. Uncertain tools/containers keep the existing durable
project fence. There is no replay, implicit recovery, publication or second agent
loop. A caller must allow the wrapper's cleanup bounds (240 seconds covers the
current client plus Framework host cleanup) before escalating termination.

The default turn limits are 900 seconds, 32,768 generated tokens, 8,192 tokens per
call, 300 seconds per model call and 90 seconds per command. A caller's shorter
absolute deadline can cancel startup or execution. Completed continuation uses
the same frozen configuration; changing `--wall-seconds` changes that binding.

Validation: `cargo test -p playtime-chat --locked`, shared engine/context tests,
and `python3 -m unittest discover -s scripts/chat -p 'test_*.py' -v`. Devbot owns
the signed task/checkout journey and independently validates report artifacts.

## Integration evidence

The November 14 Devbot pilot on Framework fast passed a first editing checkpoint,
a second checkpoint in the same saved session and cancellation during a real
command. Every turn confirmed cleanup, protected Git state remained unchanged,
and the inference host was released. Evidence is retained at
`~/.local/state/playtime-worker-pilot-20261114-r7`; setup decisions were scripted
while worker inference and project tools were real. This establishes integration,
not comparative coding quality.
