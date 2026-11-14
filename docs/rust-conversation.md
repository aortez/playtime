# Reusable compacting conversations

`playtime_engine::Controller::chat` runs one bounded user turn using the existing
controller and continuous-compaction policy. `playtime_harness::Session` adds
private, incremental persistence without `playtimed`, an experiment recipe or
archive publication. This implements the conversation and storage portion of
[#208](https://github.com/aortez/playtime/issues/208).

[Strata transport](rust-strata.md) now exercises a saved chat turn with native
model/tool/follow-up requests in an isolated Docker workshop. The
[project chat console](rust-chat-console.md) now drives saved turns through
Strata and a separate persistent-project Docker adapter. These libraries execute
no command or model request themselves, and do not enable host execution in
existing experiments.

## Chat turns

`ConversationConfig` freezes instructions, inventory, common allocations,
execution mode, tool-timeout handling, compaction settings and identity digests
for the model and workspace policy. Simulation permits simulated tools; other
modes require confirmed external termination.
The model digest must identify the backend, model, template and inference options;
the workspace digest must identify the workspace and tool access policy. The
adapter must verify these identities against its real resources. The config
fingerprint also includes the exact tool definitions. Changing any of these inputs
requires a new session in this version.

Chat requires `allow_finish_run = false`. A normally completed, nonempty assistant
reply with no tools ends a user turn. Tool responses continue through the same
admission, output limits, retries and compaction logic as continuous experiments.
Truncated replies, absent completion metadata and empty final replies remain in
observations but cannot complete the turn successfully. Known usage stays known;
an interrupted request still carries its unknown reservation.

Each turn prepares its owned workspace and ends with cleanup. Chat does not request
an experiment workspace capture or invent a publication receipt. The project
workspace adapter preserves editable files while stopping owned processes at
the turn boundary; process continuity is only within a turn. Existing archived
continuous runs retain their capture/publication lifecycle.

Only `AgentFinished` with confirmed successful cleanup can produce a
`ConversationCheckpoint`. Its version-1 protocol contains root messages, derived
summary, complete exchanges, failure facts and compaction count. Every saved tool
call must have its corresponding result. A follow-up enters the pending input,
outside the older exchanges selected for compaction, then joins the next exchange.

Request ordinals and controller observation sequences continue across turns so
failure references stay distinct. An `IdSource` must generate unique IDs from those
ordinals. Each turn has a new run ID, fresh token/time allocations and elapsed time
relative to its own start. Waiting for user input consumes no allocation. Context
measurements reset between turns; restored history is estimated conservatively
from bytes until the server supplies new measurements. This can trigger earlier
compaction, but does not fabricate measured usage.

## Private storage and driving effects

`Session::create` and `Session::open` hold an exclusive directory lock. Their
directory must be outside all tool write roots. The shared `playtime-local`
filesystem policy requires private files/directories and refuses links, unsafe
permissions and special files. The layout contains:

- `config.json` and a small, atomically replaced `head.json`.
- Immutable `input-<turn>.json`, incremental `events-<turn>-<sequence>.jsonl`
  batches and a final `result-<turn>.json` with usage and cleanup status.
- An immutable `checkpoint-<turn>.json`, limited to 16 MiB, for each completed turn.

`Session::begin` commits an active-turn marker before returning the first pending
effect. `Turn::advance` persists observations before exposing further work. The
caller records `Input::Dispatched`, rechecks the pending effect and deadline, then
executes it. Adapters must service deadlines and cancellation independently of
synchronous filesystem operations; this library is not an async supervisor.

If persistence fails, the controller stops work and still exposes cleanup. The
driver must complete cleanup even after `advance` returns an I/O error. The failed
writer is never retried. Call `Turn::finish` only after the terminal result: it
commits the checkpoint followed by the ready head. Until that head is durable, a
stopped or crashed turn remains fenced.

Opening a session with an active turn fails rather than replaying uncertain tools
or silently reverting to the previous checkpoint. Inputs, partial history and any
final result remain for inspection. Reopening recovers private files; it does not
produce an archive writer receipt or establish that a previous caller received a
commit acknowledgment. Crashed workers or incomplete cleanup still require manual
resource reconciliation.
Completed sessions reopen with the same configuration; a changed
model, workspace, policy, schema or malformed history is rejected.

`Session::recover_compaction` is an explicit, separate-session exception for a
fully observed first compaction that stopped at its output cap before any work
request. It requires one known-usage model call, a `length` completion, confirmed
cleanup, and contiguous saved records from the previous checkpoint through the
terminal observation. Any work/tool request, other event kind, missing record or
uncertainty is refused. Only the summary cap may increase in the new config.
The old checkpoint's history is copied without the rejected summary; request and
observation ordinals advance past the failed attempt. `recovery.json` records
provenance, the failed result and pending input, preserving its cost in the source
evidence. The original session stays fenced. Publication starts with a fenced new
head so an incomplete fork cannot reopen as an empty ready conversation. This
operation grants no server or tool authority and does not submit the pending input.

## Explicit stopped-turn recovery

`Session::inspect_interrupted` audits a private stopped session without effects.
`Session::recover_interrupted` creates a new session with the last completed
checkpoint (or no checkpoint after a first-turn interruption) and a bounded
`interruption.json` report. It requires the exact original configuration, a
non-held source lock, contiguous records through the matching terminal result,
and recorded complete project cleanup. Missing batches, tool-reference mismatches,
unknown event types, live source leases, and uncertain cleanup remain refused.

Tool records distinguish a result observed, an explicit non-dispatch, and an
unknown outcome. Dispatch records are intent, so an absent result is conservatively
unknown. Recovery requires acknowledgment when any tool outcome is unknown. A
recorded result does not imply success: its error/status remains evidence. All
source files stay unchanged; the new head starts fenced during publication.

The report includes the interrupted message, unknown model-usage count, tool
counts, and the latest 12 tool observations with explicit byte/preview omission
counts. Full evidence stays at the source path. On the next **new user message**,
the harness adds this labeled report as historical context. It archives both the
raw user input and the added context; no pending input or tool call is submitted
automatically. Closing/reopening an unused fork retains the notice. After a turn
completes, subsequent turns use the saved history without reinjecting it. A repeated
interruption carries a bounded excerpt of the earlier recovery as well.

This is conversation recovery, not restoration of a suspended controller. Unfinished
assistant text is not promoted to a completed reply, and project changes are neither
rolled back nor claimed successful. Cleanup records establish project cleanup only;
the caller must separately own and verify its model endpoint. The narrow
`recover_compaction` path above retains its separate validation and cap-change rules.

## Shared context planner

`playtime_engine::conversation::ContextPolicy` is the reusable context-planning
component for the compacting harness. The existing `continuous_compaction_v1`
policy uses it, and the standalone conversation test exercises the same planner
without an experiment, archive, workshop or service.

### Caller contract

Create a `ContextPolicy` with validated `CommonLimits`, `CompactionSettings` and
the exact tool definitions used by the model. Configuration is frozen inside
the policy. `initial_fits` checks that the original instructions and task leave
room for work and its reserves.

Implement `History` over the caller's transcript. Its exchanges must be complete
assistant responses with all their tool results. `work_messages` includes the
original instructions/task, previous summary, retained failure facts, complete
exchanges and pending input. `summary_messages(older)` includes only the older
complete exchanges with the task, previous summary and summary instructions.
Pending tools must finish or be reconciled before presenting a history view.

Pass the runtime's remaining time/tokens and measured model pace as `Budget`.
Maintain `ContextWindow` with the exact dispatched messages/tools and their
observed usage. Its calibrated estimates remain heuristics, with the same
limitations described in [the controller contract](rust-controller.md#continuous-compaction-policy).
Unknown usage must not be replaced with zero or a made-up server measurement.

`plan` returns work messages, a summary plan, or a stop reason. A summary plan
identifies the number of older exchanges and retains its before/after sizing
basis. Optional deferral observations explain when the preference trigger is
passed because a summary is unlikely to finish in the remaining time. Hard
context boundaries never defer for that reason.

After a normally completed, nonempty, tool-free summary, construct the candidate
replacement while preserving original instructions, recent complete exchanges
and recorded failure facts. Call `replacement` with the plan's `before_bytes`
before discarding history. It checks that the replacement shrinks and leaves
room below the trigger with the required reserves. The caller is responsible
for the content and integrity of that replacement; a fit check is not evidence
that the model retained every fact.

The planner grants no execution authority and does not settle usage, validate
model/tool output, retry effects, enforce deadlines, publish artifacts or save
sessions. Those responsibilities remain with the runtime and its adapters.
Existing experiments retain all controller admission and cleanup checks.

## Verification

The standalone test supplies a caller-owned chat history, compacts complete tool
exchanges, preserves a pending follow-up and resumes planning. It also rejects
a summary that leaves insufficient generation budget for continued work.
The existing controller compaction suite checks the integrated decisions,
failure history, retries, accounting and unchanged record golden.
The chat tests check turn continuation, cross-turn compaction, fresh allocations,
truncated/empty responses and cleanup uncertainty. Private-session tests reopen
real temporary files, verify incremental history, reject an interrupted tool call,
inject record-publication failure and exercise checkpoints larger than the small
head file's replacement limit.

```sh
cargo test -p playtime-engine --features test-harness --locked
cargo test -p playtime-harness --locked
cargo test -p playtime-runner --features test-harness --test workspace_probe --locked
```

These checks are CPU-only and execute no model-generated command or live model
request. They do not establish a working chat console or Strata integration.
