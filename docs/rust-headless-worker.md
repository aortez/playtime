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

Native execution is opt-in via `--tool-backend native --native-policy PATH`, using
the actual assigned checkout path without Docker or an image lock. See
[native tools](rust-native-project-tools.md) for the dedicated-account helper,
configuration/session binding, capability fields and separate qualification gates.
Do not combine native mode with `--environment` or `--image-lock`.

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

### Model-facing references and canonical conversion

The model-facing report uses the same top-level fields, but each selected check is
`{"evidence_ref":"tool-<invocation>-<effect>.json"}`. The model copies the entire
`evidence_log_path` from its exec result; it never constructs an identifier or
transcribes a command. `<invocation>` is a fresh 64-character lowercase hex nonce
for each invocation, including completed-session continuation. `<effect>` is the
controller effect ID. The reference is also the retained artifact's basename.
The existing context fitter preserves `evidence_log_path` when shortening output;
no context or compaction policy changes are required.

For example, the model can submit:

```json
{
  "status": "completed",
  "summary": "The selected checks passed.",
  "changes": ["Updated the parser"],
  "verification": [{"evidence_ref": "tool-<invocation>-<effect>.json"}],
  "remaining_work": [],
  "questions": [],
  "blockers": []
}
```

The placeholder in this example must be replaced with an actual observed
`evidence_log_path`. After retaining the exact raw response, the harness resolves
each reference against its in-memory registry of successful evidence commits in
this invocation. It checks the private file's SHA-256 against the committed bytes,
then copies the captured command, integer exit code and basename into the canonical
`verification` entry. Selection and array order are preserved; uncited commands
are never included. Commands retain prefixes such as `cd /work &&`, whitespace,
quotes and multiline heredocs exactly. The raw `evidence_ref` equals the canonical
entry's `log_path`, making their association independently auditable without a
second manifest.

The external worker-report-v1 contract remains unchanged: each canonical entry
has only `command`, `exit_code` and `log_path`. Devbot's frozen assignment and worker
guidance describe that external report. The worker's frozen system instructions
explicitly distinguish its reference input from that contract and direct the model
to use references; no Devbot schema or adapter change is required. Legacy full-field
model reports remain accepted only with exact command/exit-code matches against
current committed evidence. They are never corrected. Mixing reference and legacy
entries, supplying both shapes in an entry, or selecting a reference twice is an
error, even when both selections agree.

Unknown, malformed, escaped, foreign and uncommitted references are rejected before
filesystem access. Missing, changed or linked artifacts are rejected. Merely
copying a log into the artifact directory does not register it. Local effect IDs
can repeat in a later invocation without aliasing an earlier reference. Compaction
can retain a reference to current committed evidence, but an earlier invocation's
reference in history or a summary remains unavailable. The harness never searches
old runs, substitutes a new check, or replays tools to repair a report.

Each exec result is committed to private `artifacts/` before it is delivered to
the controller or model. Project tools cannot read or write these logs. Aggregate
artifact storage remains bounded at 64 MiB, with a 1 MiB per-artifact read/write
limit. Both model-input and expanded canonical reports have a 1 MiB parsing/output
limit. Evidence failures stop the turn through the shared driver and still clean
up. Tool output retains the existing per-command cap; retained logs are actual
adapter responses, not necessarily uncapped process output. A missing integer exit
code, missing timeout status, timeout or adapter error cannot become verification.
A known nonzero exit code may appear in a checkpoint, but cannot support completed
status. Existing unresolved-work and status requirements still apply.

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
The reference-reporting instructions also change the configuration fingerprint:
saved workers from the earlier full-field-only implementation are rejected, even
though the external protocol remains version 1. Use the caller's explicit fresh
reconstruction path with a new worker session, preserve the old session/evidence,
and inspect surviving edits. There is no silent session migration or fresh fallback.

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


## Caller-supplied output contract

`playtime-worker-contract-v1` is an additive editing invocation for callers that
own their report format. Worker protocol 1 above is unchanged: its instructions,
report rules, events, files and `ptw-` sessions keep their exact bytes and meaning.
The contract invocation shares the worker's tools, session journal, evidence
capture and cleanup, and uses the read-only roles'
[`json-schema-subset-v1` profile](rust-readonly-model-roles.md#output-contract-profile-v1).

Select it on the worker role by passing an output contract:

```sh
/private/package/run-strata.py framework-gyro-m-fast --worker \
  --environment general --workspace /project/checkout --run-dir /private/run \
  --request /private/run/prompt.txt \
  --output-contract /private/run/report-schema.json \
  --output-contract-sha256 EXACT_SCHEMA_BYTES_SHA256 \
  --evidence-property verification --repair-turns 1 < /private/run/prompt.txt
```

The wrapper freezes `--request-sha256` from the delivered request, as it does for
read-only roles; direct `playtime-task` callers pass it themselves.
`--evidence-property`, `--repair-turns` and `--caller-deadline-unix-ms` are
optional and are refused without a contract. Native tools need an `edit` policy. The worker probe advertises the
capability under a new top-level key and leaves every existing field alone:

```json
"contract_invocation": {
  "protocol": "playtime-worker-contract", "version": 1,
  "capability": "playtime-worker-contract-v1", "roles": ["worker"],
  "report": "structured-result-v1", "output_contract": "json-schema-subset-v1",
  "evidence": "evidence-ref-v1", "continuation": "completed-session-only",
  "repair_turns": 1, "caller_deadline": "unix-ms-v1",
  "transport": "strata-responses-v1",
  "tool_backends": {"docker": "project-docker-v1", "native": "project-native-v1"}
}
```

### Contract, evidence and validation

The contract describes the **resolved** result: the object written to
`response.json` and hashed in the receipt. Playtime checks shape only. Status
rules, readiness and the meaning of every field stay with the caller, which
validates the result again. A schema-valid result grants no authority.

With `--evidence-property NAME` (`evidence-ref-v1`), `NAME` must be a root array
whose items are objects with exactly `command` (string), `exit_code` (integer)
and `log_path` (string), all required. The model is shown the same contract with
those items replaced by `{"evidence_ref": string}` and cites `evidence_log_path`
values. The harness replaces each citation with the command and exit code it
recorded and the artifact's name, then validates the resolved object against the
caller's contract. Resolution uses the same registry and checks as worker
protocol 1: only successful evidence commits from this invocation, no duplicates,
and no timed-out or incomplete results. A nonzero exit code resolves; whether it
is acceptable is the contract's and the caller's decision, for example
`"exit_code": {"type": "integer", "maximum": 0}`. Without the option the
contract is validated as written and nothing is substituted.

The final response must be exactly one JSON object. Prose prefixes and Markdown
fences are rejected here, unlike protocol 1's tolerated preface.

### Rejected output and the repair turn

A turn whose final response is rejected is still a completed turn: its checkpoint
is committed and its cleanup confirmed before the response is examined. The
session therefore stays resumable, and the caller may continue it with a
correction as its next request. That later invocation has a new evidence
registry, so it must rerun any check it cites.

`--repair-turns 1` lets the harness send one fixed correction request inside the
same invocation instead, where earlier references are still citable. It is off by
default and one turn is the maximum. The request names the rejection reason and
the harness's own diagnostic; it never contains corrected content. A repair turn
is a second full turn under the same frozen per-turn limits and, with Docker, a
new owned container, so a caller must budget up to twice `--wall-seconds` plus
cleanup. No repair turn starts after cancellation. A repair turn that is itself
interrupted leaves the session fenced like any other interrupted turn.

`--caller-deadline-unix-ms N` (`unix-ms-v1`) says when the caller stops the
worker: the Unix time, in milliseconds, at which it sends SIGTERM, before any
cleanup allowance it grants. The harness measures it once on the monotonic clock
at startup, and starts a repair turn only if at least `--wall-seconds` remain
before it. Otherwise the invocation ends with the rejection as if no repair turn
were enabled, adding `"repair_skipped": "caller_deadline"`, and the session stays
resumable. The first turn is unaffected. A deadline that has already passed is an
input error, so the wrapper's input check refuses it before acquiring the model.

### Records and receipts

The invocation uses the read-only family of files and events, and never adds an
event type to a protocol-1 stream: `request.txt`, `output-contract.json`,
`invocation.json`, `invocation-events.jsonl` (`invocation_started`, `runtime`,
then `invocation_finished` or `invocation_failed`), `artifacts/`,
`response.raw.txt`, `response.repair-1.raw.txt` when a repair turn ran,
`response.json` after acceptance, and `invocation-result.json`. Sessions use
`ptc-` IDs with `--worker-session`; a `ptw-` or `ptr-` ID is refused, and a
changed contract or evidence property changes the configuration fingerprint.

The startup event, terminal event and `invocation.json` carry the same binding:

```json
{"capability": "playtime-worker-contract-v1", "role": "worker",
 "output_contract_sha256": "<exact input digest>", "request_sha256": "<exact input digest>",
 "evidence_property": "verification", "repair_turns": 1,
 "execution": {"capability": "project-docker-v1"}}
```

With `--caller-deadline-unix-ms`, the binding also has `caller_deadline_unix_ms`,
exactly as given. It stays out of the configuration fingerprint, so a resumed
session may run under a different deadline.

`invocation_finished` adds `turn_id`, `cleanup`, `usage` and `response_sha256`
for the accepted turn, plus `repair_turns_used`. When a repair turn ran,
`rejected` lists the rejected turn's `turn_id`, `reason`, `detail`, `cleanup` and
`usage`, so its confirmed cleanup is receipt evidence too.
There is no status echo: the caller reads status from the validated result.
After a completed turn, `invocation_failed` has `reason` `invalid_output` (not
one JSON object, or not matching the contract) or `evidence_unresolved` (a
citation that is unknown, duplicated, changed or incomplete), with `detail` and
`turn_id`, plus `repair_skipped` when the caller deadline stopped an enabled repair
turn. Other failures keep the driver's stop reason. Startup errors may still
end without a terminal event.

### Verification

`cargo test -p playtime-chat --locked` covers contract admission, the derived
model-facing shape, typed rejections, repair with live evidence, per-invocation
evidence, session binding, the caller deadline's admission and binding, and the
unchanged protocol-1 and read-only probes. The
launcher suite covers the wrapper's gate. With the locked workshop image present:

```sh
cargo test -p playtime-chat --test contract_docker --locked -- --ignored
```

This runs the real task binary against real Docker with a scripted loopback
model, three times. The first contract run makes a tool call, returns a fenced
result that is rejected, and is repaired in a second owned container, under a
caller deadline an hour away. The second has the same script, but its deadline
leaves less than one turn, so it ends with the rejection and no repair turn
starts. The other run is a version-one worker on the same task path, whose
stream, files and receipt keep their shape. Neither runs a model or qualifies native tools, the dedicated
account or how well a model uses the correction. No caller uses the contract
invocation yet.

### Live check, November 20

A separately installed build of this change ran two contract invocations through
the owner wrapper on Framework `framework-gyro-m-fast`, with Docker tools,
Devbot's generated worker schema as the contract, `--evidence-property
verification` and `--repair-turns 1`:

- A one-turn repair from a cold start finished in 46 seconds. The model fixed a
  one-line bug, ran the tests and returned one JSON object citing three
  references. The harness wrote the recorded commands and exit codes,
  `response_sha256` matched `response.json`, cleanup was complete and no repair
  turn ran.
- A second assignment also asked for a leading sentence and a fenced block, to
  provoke a rejection. The first response was rejected as `invalid_output`. The
  repair turn returned clean JSON citing the same two references, without
  running tools again, and was accepted; the invocation took 28 seconds. The
  receipt recorded one repair turn and the rejected turn's reason, usage and
  cleanup.
- Both results have status `completed` while citing the failing run from before
  the fix, with exit code 1, beside the passing one. They are shape-valid, so
  Playtime delivered them. A caller that requires passing checks for `completed`
  must reject them itself: the repair turn covers shape and evidence, not caller
  rules.

Framework ended as it began, with no model loaded, and no project containers
remained. These are integration checks on one preset, not coding-quality
measurements.

## Separate read-only role protocol

Setup, supervision and review can explicitly select the new
[read-only invocation protocol](rust-readonly-model-roles.md). These calls use
fresh native read_only contexts and caller-supplied output schemas; they do not
use worker reports or worker continuation. Existing worker argv and external
report fields remain unchanged.
