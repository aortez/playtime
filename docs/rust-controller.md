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
or finish. This is a private Rust interface with one concrete implementation,
`memento_turns_v1`; it is not an external plugin interface.

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
limits, and deterministic behavior. Session-neutral purposes and records remain
the next slice of #109; continuous conversation/compaction remains #110.

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
