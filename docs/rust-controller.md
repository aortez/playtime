# Bounded Rust controller and simulation

`playtime-engine` owns conversation policy and runtime enforcement for both
simulation and real runs. Its committed behavioral goldens are the spec; Python
parity is retired. The [archive driver](rust-archive-driver.md) persists its decisions, and the
[worker](rust-worker-package.md) supplies native effects.

To run the in-memory simulation without Docker, Ollama, or wall-clock sleeps:

```sh
cargo run --locked -p playtime-engine --example simulate -- crates/playtime-engine/tests/fixtures/two-session.json
```

This prints a simulation-labeled result and trace. It does not create a portable
archive; use the archive-driver example for that.

## Internal memento policy boundary

`policy::Memento` owns conversation construction, response/tool interpretation,
context shaping, retry and handoff decisions, session progression, and memory and
evidence delivery. It proposes a finite set of actions: start a session, begin
handoff, request inference, handle a tool call, complete a response, checkpoint,
or finish. This is a private Rust interface with one production implementation,
`memento_turns_v1`; the feature-gated workspace probe is a CPU contract test.
There is no external plugin interface.

The controller supplies read-only facts: remaining allocation, admitted work and
handoff counts, parse-error counts, and confirmed memory-write outcomes. These
counts stay in the runtime so policy transitions cannot reset them. Complete
responses reach the policy after envelope/byte checks and usage settlement;
partial chunks remain observations only. The existing bounded adapter-input
contract still applies to tool results and lifecycle observations.

`controller::admission` validates proposals against those facts and the frozen
`policy::memento_contract`. It checks lifecycle prerequisites, exact tool
capabilities, turn and retry limits, and the two-attempt handoff ceiling. The
contract derives the generation allowance and memento reserves from the accepted
configuration. Proposals cannot supply an allowance, cost estimate, deadline,
effect ID, accounting mutation, or publication receipt. Tool calls still pass
schema, handoff, and memento-size checks before becoming workshop effects.

The policy chooses messages and truncates delivered tool output. The runtime
retains the accepted context estimator and its measurements: a measurement binds
to the actual admitted messages plus the accepted response, never to history
rewritten by the policy. Every inference proposal is checked against its actual
messages/tools, derived output allowance, and frozen context reserve. Context
rejection preserves the existing early-handoff/skip behavior and strict boundary
comparison.

The runtime also owns global request/seed ordering, reservation and settlement,
absolute deadlines, cancellation, dispatch identities, durable control facts,
checkpoint publication, and cleanup. A proposed successful finish requires a
confirmed memento write and publication of this session's checkpoint. Publication
failure or a late receipt cannot turn a previous failure into success.

This extraction preserves memento's prompts, tools, records, saved definitions,
limits, and deterministic behavior. Continuous compaction uses the same controller; recipe selection remains #110 step C.

## Session-neutral request and lifecycle contract

`CommonParameters`/`CommonLimits` contain allocations shared by every policy,
without dummy session counts, memory conditions, memento sizes or handoff reserves.
The legacy `RunParameters` representation and validation entry point remain;
memento projects its common allocation and retains its own additional checks.

Every admitted request has a `RequestPurpose` (`work`, `handoff`, `compaction`),
a `RequestScope` (run or an actual session), an effect/request ID and a global
call ordinal. `RequestContract` derives its allowed tools, generation cap and
context reserve from the frozen profile. Memento work still preserves the exact
handoff allowance; the test profile has no handoff reserve. Maintenance uses the
same remaining generation allocation, wall deadline, model timeout, seed sequence,
context-fit check and unknown-usage settlement. `model_calls` counts admissions,
including an attempt canceled before dispatch or refused by adapter preflight.
There is no additional model-call cap or separate maintenance budget.

Purpose and scope accompany bound observations and durable control facts, including
settlement, non-dispatch, tool results and omissions. Legacy memento request JSON
still serializes the original `handoff` boolean; its work/handoff mapping is
lossless. The new record family writes explicit purpose instead. Common response
and tool-schema checks are independent of memento's progression and memory checks.

The feature-gated `test-harness` policy proposes `StartWorkspace`, work inference,
a bounded tool, a tool-free maintenance request, `CaptureWorkspace` and finish.
It uses this same controller and archive driver; commands remain scripted data.
It requires no session ID, `MEMENTO.md`, `end_session`, or memento checkpoint.
A stopped `WorkspaceSnapshot` is a candidate. Only the exact publication receipt
can set `last_capture` and satisfy successful finish. Capture/publication/cleanup
use the existing finalization timers and first-stop precedence, including late
receipts. Production workers reject this test execution profile before effects.
This probe does not prove native workspace continuity. The separate compaction
controller policy below exercises the actual conversation transformation.

Run the CPU boundary gate with:

```sh
cargo test -p playtime-runner --features test-harness --test workspace_probe --locked
```

## Continuous compaction policy

`Controller::continuous` runs `continuous_compaction_v1` through the shared
controller and effect dispatcher. The packaged worker supports an explicit
[continuous native profile](rust-continuous-native.md); recipe/API/workbench
selection remains #110 step C. `Controller::test_compaction` remains the
feature-gated CPU test constructor.

`CompactionSettings` version 1 defaults to a 75% projected-request trigger, two
recent exchanges, a 2,048-token summary cap and explicit editable instructions.
Structural bounds are trigger 1..99, retention 0..32, summary cap within the
accepted common per-call allocation, and nonempty instructions up to 65,536 UTF-8
bytes. The constructor also rejects initial inputs that cannot do work below the
trigger with required reserves. It never edits a retained allocation to make a
configuration fit.

One exchange is an assistant response and all its tool results, after any
notices the controller added for that request. System/inventory
instructions and the original task stay verbatim. A work request's projected
occupancy is its estimated input (including tools) plus its actual output
allowance. The independent strict fit check adds a fixed safety margin
`max(1024, ceil(context / 20))`, the full summary allowance, exact encoded
compaction instruction overhead and an aggregate tool-result allowance. Reaching
either the percentage trigger or that fit boundary schedules compaction at a
complete exchange boundary. When retained input alone prevents progress, it
stops rather than silently reducing retention.

With recent exchanges retained, the policy also checks the summary that would
be needed after one more work exchange. Its older prefix is already known: the
next response and its tool results will stay in the retained suffix. This check
uses encoded bytes, since dropping that suffix invalidates the next server
measurement. If the deferred summary would not fit with its instructions, output
allowance and safety margin, compaction runs at the current boundary while its
smaller input still fits. With no retained suffix, summarization extends the
measured prefix and the ordinary work reserve covers that continuation.

The aggregate retained result allowance is the existing `output_bytes` setting
for this policy, divided among a response's tool calls. Each needs at least 512
bytes; an oversized batch stops the run before effects. Actual fitted message
sizes are checked by the runtime independently of policy; full bounded results
and explicit shortening notices remain in observations. Memento keeps its
existing result-fitting semantics and wording.

Summarization receives the original instructions/task, preceding derived summary,
older complete exchanges and the compaction instructions, with no tools. It
uses the same request IDs, global call/seed ordering, timeout, wall deadline and
generation allocation. The policy requires enough remaining generation for the
summary cap and another work call; runtime admission independently reserves the
work allowance. Compaction cannot grant itself a separate budget.

`Outcome::ModelCompleted` supplies explicit termination metadata. A summary must
report `stop`, contain nonempty final-answer text and no tools, and stay within
its admitted generation allowance. Thinking remains original evidence and never
becomes the replacement summary. Partial, unknown-completion or failed responses
leave active history intact. Existing legacy model outcomes remain supported for
memento but cannot prove a normally completed summary.

Replacement retains the configured recent exchanges and installs a tagged
assistant summary. Its byte estimate must strictly decrease against the old
conversation's byte estimate (the same basis), fit below the trigger and leave
all work reserves. Rewritten context cannot reuse a server measurement for a
different prefix. A summary response the server can't parse is retried as below;
any other failure stops explicitly.
Observations retain source/retained request IDs, prior summary, exact before/after
messages, estimates and their bases, summary content and outcome. Durable
versioned source links and bounded archive projections are described in the
[native contract](rust-continuous-native.md).
`compaction_planned` records the decision without claiming request admission;
the ordinary model-request/admission facts identify any actual attempt. A rejected
or canceled plan cannot inherit the previous work request's purpose.

Work exposes `exec`, `read_file`, `write_file` and `finish_run`. The latter takes
exactly one non-whitespace `summary` string of at most 8 KiB and must be the sole
tool call in a normally completed response. The controller validates the entire
batch before tool effects. A malformed or mixed finish stops the run with
`invalid_completion`, and ordinary prose does not finish. A response that calls
no tool gets a notice to continue or call `finish_run`; otherwise the next request
would end with the model's own message, and gpt-oss then returned empty
one-token responses until the run stopped. When an ordinary call
is invalid, no call in the batch runs, but the run continues. Each call's result
is an error: its own, or "Not run because another call in this response was
invalid". Until 2026-10-26 (UTC), such a batch stopped the run. A valid
declaration enters final-only capture;
publication acknowledgment is still required before `agent_finished`. Capture
failure, cleanup failure, cancellation and late receipts retain their independent
evidence and established first-stop precedence.

Settings with `allow_finish_run: false` leave `finish_run` out of work requests,
and admission refuses a proposal that offers it. A call to it is then an
ordinary unavailable tool, so its batch runs nothing and work continues. The
no-tool notice says the run ends when its budget runs out instead of naming the
tool. Such a run ends only at a limit, a stopping error or cancellation, and
still captures its final workspace.

Run the CPU gate with:

```sh
cargo test -p playtime-engine --features test-harness --locked
```

The separate `compaction-policy-v1.json` golden captures exact request hashes,
settings, tools, transitions and final outcome. Its test driver supplies model
responses, tool data and explicit simulated writer acknowledgments. It executes
no host commands and does not establish native process continuity or publication.

## Transition and effect contract

`playtime-engine::Controller` is a synchronous state machine. It never reads a
clock, runs a command, performs inference, or writes an archive. A driver supplies
monotonic milliseconds, IDs, cancellation, model chunks and completion results.
The controller returns one pending, uniquely numbered effect and an ordered
outbox. Effects cover fresh session startup, inference, validated workshop tools,
stopping/snapshotting, checkpoint publication, and cleanup. There is no second
simulation stop policy. The sampling seed is `(initial + call - 1) % 2^31`.

A driver persists the outbox, acknowledges `Dispatched` with the current time,
rechecks the pending effect, then starts it. Persistence failure is an input and
prevents dispatch. A preflight cancellation releases an unstarted reservation;
it does not invent unknown inference. Replacing a pending effect obliges the
driver to cancel/join it before executing a conflicting workshop operation. Completions
must match both effect ID and type; stale/duplicate results cannot settle usage
or execute tools. Every effect has a deadline. A driver must deliver a tick at
`next_wakeup_ms()` even if its model or workshop never yields; this includes the
run wall timer during checkpointing. The archive runner and native effect
adapters supply real scheduling and publication; the worker/service supply
supervision. Those responsibilities stay
outside the controller and its in-memory simulator.

At an observation boundary, cancellation precedes the run wall deadline, which
precedes the effect deadline, which precedes completion. Equality expires a
deadline. The first terminal decision is immutable. It prevents further model
or tool dispatch but permits one final checkpoint attempt and cleanup. During an
already pending checkpoint, cancellation marks the run interrupted without
restarting that effect. A successful `finish_run` write is a completion candidate;
checkpoint publication must succeed before it becomes `agent_finished`. Thus a
checkpoint failure remains an infrastructure failure. Normal end
precedence is agent completion, token exhaustion, then session exhaustion.

Checkpoint effects have an 80-second bound (stop plus snapshot), publication
20 seconds, and cleanup 20 seconds, independent of the expired work deadline.
Cleanup failure is separate from the primary reason and never erases it. A
checkpoint is visible as `last_checkpoint` only after publication acknowledgment.
Publication failure preserves the preceding checkpoint. A partial startup still
receives cleanup. Native adapters must independently bound and cancel their work;
controller deadlines alone cannot establish Docker or model-host termination.

## Accounting invariants fixed for property tests

1. Completed measured generation, unknown reserved tokens, unknown request count,
   and their completed total are monotonically nondecreasing.
2. One request is in flight at most. Its positive allowance is reserved before
   its effect is exposed. Admission requires completed charges plus allowance
   plus the work-request handoff reserve to fit the original run allocation.
3. Settlement occurs once. A complete measured response replaces only its own
   pending allowance. Failure/cancellation/absent usage after dispatch moves that
   full allowance to completed unknown usage. Previously unknown usage is never
   erased. Admission and actual dispatch are separate observations; model_calls
   counts admitted attempts, including preflight failures.
4. The *pending plus completed* total may decrease on measured settlement; it is
   not a monotonic metric. A server can report more than its allowance; the
   reported amount is charged, and exceeding the run allocation stops execution
   before tools. An unrepresentable total records the reported count in the
   diagnostic event, retains the full unknown allowance, and stops.
5. No new model/tool effect follows a terminal decision. Checkpoint and cleanup
   effects remain legal. Successful zero-token responses still consume turns.

Parser recovery consumes an ordinary work turn or one of two handoff attempts.
More than `tool_parse_retries` consecutive parse errors stop the run. The error
ceiling, `tool_parse_error_limit` (default `tool_parse_retries + 1`), counts per
session: reaching it ends that session through the normal handoff, recorded as
`early_handoff` with reason `tool_parse_error_limit`, and the next session counts
from zero. Before 2026-10-18 this ceiling covered the whole run and stopped it.
Continuous compaction has no sessions and no recipe parse limits. It retries a
failed work request with memento's notice, minus its budget sentence, and that
notice opens the next exchange. More than `TOOL_PARSE_RETRIES` (5) consecutive
failures stop the run with `tool_parse_retry_limit`; admission enforces that
bound independently of the policy. A summary request the server can't parse
counts toward the same streak. It gets no notice: the policy plans the same
compaction again, so every check sees the failed attempt's charge, and the new
plan keeps the compaction's ordinal. With no total ceiling, each failure's full
unknown charge bounds the total through the run budget. Until 2026-10-26 (UTC),
the first failure stopped a continuous run.
Neither ceiling resets the allocation or the charge for a failed request. Partial tools
are observations only. A fresh session retains workspace/memento and optionally
bounded evidence, but no prior messages, measured context, processes, or `/tmp`.
Tool arguments and memento UTF-8 sizes are checked before dispatch.
Dispatch-intent records precede effects; `request_not_dispatched` and
`tool_not_dispatched` explicitly cancel those intents. Evidence excludes execs
that never crossed the dispatch boundary. Final-record persistence failures are
retained in result diagnostics even after a primary stop decision.

## Script format and limits

The version-1 JSON scenario declares `mode: simulation`, parameters, initial text
files, model scripts, optional scripted exec results/mutations, virtual delays,
cancellation time, and lifecycle failures. A model script contains delayed
chunks followed by one response/error, or no completion (a stalled model).
Exec scripts match the expected command exactly and return declared data; command
text is never interpreted or passed to a shell. Read/write operations affect only
the simulator's in-memory workshop. Files and temporary/process state are distinct.

The example prints a simulation-labeled result and normalized trace; these are
test artifacts, not frozen experiment archives. `RunMode::Simulation` also labels
the packaged simulation in the daemon, API, and archives.

Context estimates, prompts, tool descriptions, and delivered tool truncation
use canonical ASCII JSON sizing pinned by the Rust fixtures. Evidence uses
sorted UTF-8 JSON, a 4096-byte ceiling, deterministic file/command ordering,
excerpt budgets, and explicit unknown-field semantics. File comparison includes permissions, entry types and link
targets as well as content hashes and lengths. Artifact hashes refer to exact
bytes. Evidence source references
identify checkpoint artifacts and observation sequences; the runner/storage
integration supplies durable record hashes and validates archived provenance.

Ordinary observations reserve 65536 log bytes for finalization.
An exhausted log stops admission; the result remains available separately even
when a deliberately tiny log cannot fit all final observations. Adapter input
must also be bounded before allocation: controller checks do not make reading
an unbounded network frame safe.

## Validation and integration

Rust checks cover deterministic repeated traces, committed recovery and evidence
fixtures, and 128 fixed-seed property cases for monotonic completed
accounting, admission within allocation, and no dispatch after a stop decision.
Private-boundary tests inject invalid proposals independently of memento to check
runtime turn/retry/tool limits, context fit, preserved unknown charges, stop and
dispatch facts, and publication prerequisites for successful completion.
Virtual-time cases cover never-yielding inference, partial disconnects, parser
ceilings, missing usage, command timeouts, context/byte/log limits, handoff
exhaustion, oversized/missing memory, cancellation races, and checkpoint/cleanup
failures. The Python suite independently verifies tool definitions, canonical
JSON sizing, and bounded Unicode evidence without third-party packages.

The engine retains its I/O-independent boundary. The [durable driver](rust-archive-driver.md)
now supplies publication during execution, typed lossless facts, explicit source
identities, external log-capacity feedback, and late checkpoint receipts.
`take_control_updates` must be drained before its bounded transition reserve is
full; raw logging may omit records without erasing those control facts.
`ScriptedAdapters` supports incremental dispatch/cancellation and remains the
adapter used by the original in-memory simulation.

The simulation's JSON workspaces remain explicitly synthetic. Native Docker and
Ollama execution uses the same controller through worker-supplied effects; see
[run-and-view](rust-docker-run-and-view.md) and [live model](rust-live-model.md).
The [transcript contract](rust-archive-bridge.md) defines protocol-only decoding,
reviewed mapping fixtures, and the separation between raw copies and canonical
accounting/publication authority. Automatic recovery remains unimplemented.
