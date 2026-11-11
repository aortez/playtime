# Rust CLI and shared client state

`playtime-cli` builds the Rust `playtime` binary used with `playtimed`. The
existing `./playtime` is the frozen Python launcher. The Rust client owns parsing
and presentation; `playtime-client` owns authenticated transport, connection
resolution, receipts, cursors, and verified downloads. It does not start the
service, execute workers, or access Docker or the model directly. IDs in commands
are opaque API identifiers, not filesystem paths.

For normal use, start with [go.sh and daemon operation](rust-mvp.md#run-it).
The examples below use scripted HTTP fixtures. Their gate and the working normal
service gate are distinct from the dedicated client simulation acceptance gate,
which still fails pending full restart/recovery integration.

## Commands and fixture use

Start the bounded `api_fixture` from [the HTTP guide](rust-http-api.md) in a new
directory, then use its actual connection descriptor:

```sh
cargo run --locked -p playtime-cli -- --connection-file /tmp/playtime-api-demo/.playtime/connection.json recipes list --format json
cargo run --locked -p playtime-cli -- --connection-file /tmp/playtime-api-demo/.playtime/connection.json recipes preview fixture-example --mode simulation
cargo run --locked -p playtime-cli -- --connection-file /tmp/playtime-api-demo/.playtime/connection.json run fixture-example --mode simulation --idempotency-key cli-demo-1 --follow --timeout 60s --format jsonl
cargo run --locked -p playtime-cli -- --connection-file /tmp/playtime-api-demo/.playtime/connection.json launches retry cli-demo-1 --format json
```

The HTTP example has explicitly unknown cleanup, so outcome-sensitive follow
returns exit 10 while still printing its recorded outcome. This is deliberate:
a terminal stream alone cannot prove cleanup. The process test fixture supplies
separate scripted normal, interrupted, unknown-cleanup, stalled, lost-reply, and
bad-content cases. Neither fixture is a persistent admission service; a new
fixture invocation starts fresh state. Do not reuse old launch receipts as if a
fresh fixture instance preserved an earlier ledger.

For normal development the executable is `target/debug/playtime`. Help/version,
reserved-command explanations, and local receipt listing work offline.

| Command | Result |
| --- | --- |
| `service status` | Service identity/build, backend provenance, versions, features, limits, connection diagnostics |
| `recipes list`; `recipes preview ID --mode MODE` | One catalog page or the full read-only preview |
| `run ID [--mode MODE] [--preview-revision REV] [--idempotency-key KEY]` | Save request, submit, return after acceptance; default mode is `experiment` |
| `run … --follow` / `--wait` | Stream and wait / return one final combined launch/result document |
| `launches list`; `launches show KEY`; `launches retry KEY` | Offline local receipts / service lookup / original request recovery |
| `runs list`; `runs show ID`; `runs wait ID` | One page / detailed run view / outcome-sensitive observation |
| `runs cancel ID [--wait]` | Cancellation intent, optionally await quiescence and cleanup |
| `runs release ID --reason TEXT` | Operator release of an interrupted or failed run; refused with what remains |
| `runs report ID [--limit N]` | The run's records totalled, as Markdown or typed with `--format json`; see [run reports](#run-reports) |
| `runs download ID --output DIR [--session SESSION] [--force]` | The run's final workspace, or a checkpoint's, saved under a directory, each file verified; see [workspace downloads](#workspace-downloads) |
| `events ID [--cursor CURSOR \| --cursor-file FILE] [--limit N] [--all \| --follow]` | Fixed-frontier page by default; all keeps that frontier, follow observes later commits |
| `checkpoints list ID`; `checkpoints show ID SESSION` | Published checkpoint metadata and content references |
| `artifacts list ID`; `artifacts show ID ARTIFACT` | Published/frozen-content catalog and metadata |
| `artifacts download ID ARTIFACT --output FILE [--force]` | Bounded, verified, atomic content download |

All list commands support `--cursor` and `--limit` (1–1,000, default 200).
Local receipt listing uses a key cursor; HTTP cursors remain opaque. Only events
supports all/follow. Unsupported modes fail visibly. Reserved prepare/check/smoke,
top-level report (the Python HTML report), repeat/pilot, serve/worker commands
explain their owning migration issues and exit 6. See
[the command map](rust-compatibility.md#rust-cli-command-map).

## Shared connection resolution

`playtime-client::config::{Options, Environment, resolve}` contains the shared
resolver. It has no clap or egui dependency. `resolve` and `Resolved::load` perform
bounded synchronous file I/O; call them on a blocking task. `LoadedConnection`
then connects asynchronously. The CLI does this outside its observation task.
The workbench must do the same outside its render loop.

Select a whole connection source in this order:

1. Explicit `--connection-file FILE`, or `--endpoint URL --token-file FILE`.
2. `PLAYTIME_CONNECTION_FILE`, or `PLAYTIME_ENDPOINT` plus `PLAYTIME_TOKEN_FILE`.
3. TOML chosen by `--config`, `PLAYTIME_CONFIG`, then
   `$XDG_CONFIG_HOME/playtime/client.toml` (fallback `~/.config/playtime/client.toml`).
4. Exactly `./.playtime/connection.json`, if present. No ancestor search.

Forms conflict within a source; a pair must be complete. No endpoint borrows
credentials from another source. Explicit missing/invalid files fail. Configs
reject unknown/duplicate fields and exceed neither 64 KiB nor regular-file bounds.
A selected config is validated even when connection flags override its source,
since it may still select the state directory. A missing default config is allowed.

Example config (choose one connection form):

```toml
connection_file = "../service/.playtime/connection.json"
state_dir = "../client-state"
```

Alternatively use `endpoint = "http://127.0.0.1:8766"` and `token_file = "token"`.
Paths in flags/environment are relative to cwd. Paths in TOML/discovery files
are relative to those files. State precedence is `--state-dir`,
`PLAYTIME_STATE_DIR`, config `state_dir`, `$XDG_STATE_HOME/playtime` (fallback
`~/.local/state/playtime`). Relative XDG base directories are ignored, following
[the XDG specification](https://specifications.freedesktop.org/basedir/latest/).

Credentials remain protected files, never command-line token values. Shared
no-follow, ownership, mode, regular-file and link checks remain in `playtime-local`.
The transport retains loopback-only endpoints and DNS validation, no redirects,
no system proxy, no hidden mutation retry, and pinned service identity. Diagnostics
can identify the endpoint/file location, never token contents.

## Shared durable launch receipts

`playtime-client::state::{ReceiptStore, LaunchReceipt}` is the single CLI/UI
format and store. Before any launch POST it publishes
`<state-dir>/launches/<key>.json` with schema version 1, service ID, original
endpoint, exact typed request/revision, key, creation time in Unix milliseconds,
and optional acknowledged run ID. There are no bearer bytes or full prompts.

New keys use OS randomness. State directories are private, files are 0600, and
all opens are descriptor-relative with no-follow checks. An advisory lock
coordinates processes (five-second contention bound). Exclusive publication
syncs file bytes and parent directories before submission; acknowledgment uses
atomic replacement and directory sync. A killed writer releases its lock.
Partially staged hidden files may remain after process death; they are not
receipts and are ignored by listing.

Concurrent matching clients reuse the first saved request and creation time.
Different content/service binding is rejected without replacement. When `run`
finds its supplied key, it uses the saved request before requesting a preview.
A failed acknowledgment write cannot undo server acceptance: diagnostics include
the key, receipt path, and known run ID. `launches show` can resolve an unresolved
receipt; a 404 never permits generating a fresh key. `launches retry` requires
that original saved request. The workbench removes the receipt of a key it just
generated when the service refuses that first request with `run_busy` or
`stale_preview`, since nothing was admitted; the CLI keeps such receipts for now.
An endpoint move requires explicit connection flags
and the same persistent service identity. Every mutation still sends the expected
service ID to protect the handshake/request interval.

These are recovery aids, not execution authority or an experiment journal. The
service ledger remains authoritative. Shared tests exercise independent CLI/UI
store handles and competing writers. Local listing is bounded to 1,000 returned
items and a maximum 100,000 directory entries; receipts are at most 16 KiB.

## Observation, output, and downloads

The CLI output schema is independently versioned:
`{"schema_version":1,"kind":"result|page|launch|event|notice|error","data":…}`.
Text is concise and escapes control characters in untrusted fields. JSON preserves
the underlying data. Stdout contains command data; stderr contains diagnostics.
Machine stderr uses one JSON envelope per line, including errors and backend
provenance. Help/version remain ordinary informational output.

Finite JSON commands print exactly one document. JSONL emits complete
newline-terminated frames. Follow emits launch (when applicable), events, a
reconciled stream notice, then the final result. JSON is rejected for all/follow
before connection activity. Event data includes the original envelope and opaque
cursor; unknown event payload fields are retained. Reviewed formatter and actual
process-output fixtures live in `crates/playtime-cli/tests/fixtures/`.

Follow/wait has a 30-minute observer deadline, overridable with a positive
`--timeout` (`ms`, `s`, `m`, or `h`). Expiry exits 8 without changing the run.
Reconnect uses the shared client's bounded exponential backoff, at most 60
seconds of consecutive outage. Valid SSE activity resets that outage budget;
quiet stalled reconnect bodies cannot extend it. Status polling handles pending
preparation and lifecycle reconciliation. EOF is transport loss, never completion.
Conflicting prefix metadata on a record or terminal notice fails with exit 6;
the CLI emits no conflicting record or successful completion, and retains its
last saved cursor. The shared client checks prefix continuity across reconnects.

Optional cursor files are private, versioned, and bound to service/run/archive
identity. After a complete event frame is written **and flushed**, the CLI saves
its cursor atomically and then acknowledges transport delivery. Updates use a
private sibling lock file and reject regressions. This records CLI emission, not
processing by a downstream pipeline. A crash before cursor publication can
redeliver a record; consumers needing durable processing must save their own cursor
with their processing result. There is no exactly-once claim.

SIGINT/SIGTERM detach and return 130/143; they never call cancel. Broken output
returns 7 without a panic or run cancellation. Terminal orchestration, experimental
outcome, usage uncertainty, cancellation enforcement, and cleanup remain distinct.
Ordinary inspection may successfully describe a failed run; wait/follow on a run
requires a committed normal stop and complete cleanup. Cancellation-wait instead
requires confirmed quiescence/cleanup and, when there is an intent, stopped
enforcement without uncertainty. Unknown usage alone does not make a normal stop
fail.

| Exit | Meaning |
| --- | --- |
| 0 | Requested operation succeeded; outcome-sensitive observation also succeeded |
| 1 | Internal client failure |
| 2 | Invalid arguments or local configuration |
| 3 | Unavailable connection/service, timeout, exhausted reconnect, unresolved mutation |
| 4 | Authentication/authorization rejection |
| 5 | Rejected operation, missing ID, missing or unknown target, stale preview, busy root, key/service conflict |
| 6 | Unsupported version/capability, invalid/corrupt evidence, integrity failure |
| 7 | Receipt, cursor, download, or output I/O failure |
| 8 | Observer deadline; run unchanged |
| 9 | The run stopped and waits for an operator release (`playtime runs release`) |
| 10 | Abnormal/missing committed outcome, failed start/interruption, or failed/unknown cleanup |
| 130 / 143 | This observer received SIGINT / SIGTERM |

Normal stops are agent_finished, session_limit, token_limit, wall_limit, and
log_limit. A remotely interrupted run is exit 10 when waiting, not signal exit 130.

A cancelled or failed run doesn't become terminal on its own. Once the daemon
stops monitoring it (Reconciling with reason `worker_unavailable`,
`coordinator_restarted` or `execution_failed`), `runs cancel --wait`, `runs wait`
and `run --wait` return exit 9, and the result's `next` field gives the
`playtime runs release RUN_ID --reason TEXT` command. A plain `runs cancel`
result carries the same `next` field when the run waits for a release. The run
summary adds a `Worker failure:` line with the worker's own account when it gave
one. Reconciling with `control_uncertain` keeps waiting. A normal run doesn't pass through Reconciling:
a starting worker that hasn't answered yet keeps its phase until its startup
deadline, and a completed run stays Running while the daemon stops its unit
(#93). `--follow` still waits for a terminal stream.
Server error envelopes retain codes, safe details, and request IDs.

`download_to_path_with_force` is reusable by the UI. It holds the selected parent
directory, creates a private sibling staging file, streams and verifies length and
digest, syncs bytes, then publishes atomically. By default an existing destination
is preserved. Force permits replacing a regular file, not a symlink/device/directory.
A single download's filename is the caller's, never derived from model-controlled
metadata; a [workspace download](#workspace-downloads) takes the archive's
names under a policy. Ordinary failures
remove staging; abrupt process death may leave a `.part` file. An interrupted HTTP
body can be a transport failure before the digest comparison is possible; it is
still never published as a successful download.

## Workspace downloads

`playtime runs download RUN_ID --output DIR` saves the run's final workspace
under the directory: an ended continuous run's final capture, or a memento
run's last published checkpoint, as `runs report` lists it; `--session`
names a published checkpoint instead. A continuous run without a published
capture yet has no workspace to save (exit 5). The shared client
(`playtime_client::workspace`) resolves the workspace's files to the
artifacts holding their content, from the capture index, read and decoded
as a checkpoint's is, or from the checkpoint's file list, then saves each
retained file as `download_to_path_with_force` saves one: staged beside its
destination, verified, then published, so a file that fails publishes
nothing and leaves no staging file.

Files are saved under the names their index gave them, which the model
chose, under a policy rather than by sanitizing: a name is a relative path
without an empty, `.` or `..` component, control character or backslash,
with components of at most 255 bytes and at most 1,024 in all; any other is
refused, and its content stays downloadable by artifact ID. The directories
a name lists are created private (0700) as needed and opened without
following a link, so a link planted among them, or at the name, is never
written through, even with `--force`: such an entry, a directory or special
file at the name, or a file where the name needs a directory, is reported as
blocking the file. The output directory itself is created when missing, but
not its parents. Saved files are private (0600) regardless of the archived
mode, so a saved script isn't executable until the reviewer makes it so. An
existing file at a name is left alone unless `--force`, which replaces a
regular file only. Content the archive didn't retain is listed as
unavailable with the archive's reason and isn't a failure.

The result lists every file (name, kind, artifact ID and content reference
when retained, status `saved`, `unavailable`, `refused`, `blocked`, `exists`
or `failed`, path, verified bytes, or the reason or error) with the totals
saved, unavailable and not saved, and names the workspace's source, session
and published sequence. Every file is tried; the command then exits 0 when
nothing that could have been saved wasn't, and otherwise with the first such
file's exit code: 7 for a refused name, a blocked entry or an existing file,
and the download's own classification (6 for a verification failure, 3 for
transport) for a failed one. Cancellation ends the download at the current
file and reports nothing; the files saved before it stay, and a later
download lists them as existing.

## Run reports

`playtime runs report RUN_ID` totals a run's records the way the workbench's
Results tab does, from the same accumulator (`playtime_client::report`): time
in model requests by purpose; model calls with their median and longest
durations, tokens and rates, and the calls with unknown usage, never
dispatched or still open; model errors by kind with the longest parse streak;
tool calls by name with their failures; compactions; sessions; and the final
workspace. It reads every event page as of the frontier the first page fixes
(`--limit` sets the page size; the service's default otherwise), within the
workbench's bounds (open calls, durations per purpose, error kinds, tool
names, failure messages and compaction times are capped, with the overflow
counted), then lists the final workspace: an ended continuous run's final
capture from its artifact listing, or a memento run's last published
checkpoint, counted by kind. The run has ended when the service reports it
terminal or its archive records its end, as the workbench reads it. A run
that hasn't ended reports its totals so far, with open calls in progress and
its latest workspace. If the listing can't be read, the report is still
written and the command fails with the listing's error.

Text output is Markdown: a heading with the run, a line with its state,
outcome, recipe, mode and the records counted, then a heading per section
with a line per total. Besides control characters, the link, image and HTML
punctuation of archive-sourced text (tool names, error kinds, failure
messages) is escaped, so a name or message a model chose can't embed a link,
an image or a tag. `--format json` carries the run view (`run`), the records
counted (`records`), the typed totals (`report`: `calls` and `tools` are
always present; `time`, `errors`, `compactions`, `sessions` and `workspace`
once something counts toward them; the first record's sequence wherever a
count can be located), the workspace listing (`files`) and the worded
sections (`summary`). A rate or median is left out rather than guessed, and
nothing reads model text as evidence. The reserved top-level `report`
command is the Python HTML report's migration (#26), not this.

## Test bed and gates

Run the [required workspace checks](development.md#required-checks-for-rust-changes)
and the CLI fixture gate:

```sh
./scripts/test-rust-cli-e2e.sh --backend fixture
```

The **fixture** wrapper builds binaries/examples once and discovers executable
paths from [Cargo's JSON artifacts](https://doc.rust-lang.org/cargo/reference/external-tools.html#json-messages).
It runs the Rust `cli-e2e` process driver with the actual CLI and a separate
`cli_fixture` process. Cargo's integration test runs the same cases with the actual
CLI process and an in-process real loopback server. Neither invokes Cargo from a
running test. Tests use port zero, deadline-based readiness, fresh protected
service/client directories, isolated XDG config/state, and owned-child cleanup.
`playtime-test-support` provides this infrastructure for the workbench suite too.

Cases cover command/output contracts, 10 known + 100 unknown = 110 charged tokens,
ambiguous admission and same-key recovery, observer signals/deadlines, broken
pipes, killed observer reconnection, cursor redelivery and flush ordering, identity
rebinding, configuration/auth failures, normal versus abnormal/unclean results,
and verified downloads that preserve existing files and reject symlinks/corruption.
Each case is bounded to 30 seconds and each suite to 180 seconds after build.
Failures retain a bounded diagnostic bundle under `/tmp/playtime-cli-failure-*`
containing command stdout/stderr and operation/ID traces. Credential files and
client receipts are excluded and known test credentials are redacted.

The dedicated `./scripts/test-rust-cli-e2e.sh --backend simulation` invocation
still exits nonzero. Its diagnostic names the original integration issues; it
must not skip or substitute a fixture. The [normal service gate](rust-daemon.md#executable-gate-and-remaining-gates)
already supplies the real daemon, controller, archives and client inspection.
The broader simulation acceptance also requires disconnect accounting, workshop
timeout, cancellation, API restart with a surviving worker, worker loss, no
repeated dispatch, and checkpoint/artifact inspection through the API alone.
Do not claim that full acceptance from fixture or normal-path tests.
A test supervisor is not a systemd smoke;
[Worker lifecycle](rust-worker-lifecycle.md#real-systemd-supervision) documents
the opt-in real-systemd boundary. None of this fixture validation requires
a model, Docker daemon, GPU, or desktop.

## Inference target selection

`playtime targets` lists configured inference locations without probing them.
`playtime --target ID models` reads that target's installed-model metadata.
`--target ID` also scopes `recipes preview` and `run`; it is retained in the launch
receipt. `launches retry` always uses the original retained target. See
[named inference targets](rust-inference-targets.md) for daemon configuration and
examples. Simulation and scripted Docker previews do not use inference targets.
Without `--target`, a daemon with named targets refuses model metadata as
`invalid_parameters` and live previews or launches as `execution_unavailable`;
text output lists the error's reasons, such as `target_required`. When every
reason is a target choice (`target_required`, `target_unknown` or
`target_requires_live_execution`) the command exits 5, because retrying can't
clear it; other execution outages exit 3.
