# Bounded Rust controller and simulation

Tracking: [#18](https://github.com/aortez/playtime/issues/18). Builds on the
foundation commit `de1647a` and Python baseline `ad7a2f2` in the compatibility
matrix. The Python launcher remains the operational entry point.

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
run wall timer during checkpointing. Real async scheduling, durable event
envelopes, publication transactions, and supervision belong to
#19/#20/#23/#24, not this in-memory driver.

At an observation boundary, cancellation precedes the run wall deadline, which
precedes the effect deadline, which precedes completion. Equality expires a
deadline. The first terminal decision is immutable. It prevents further model
or tool dispatch but permits one final checkpoint attempt and cleanup. During an
already pending checkpoint, cancellation marks the run interrupted without
restarting that effect. A successful `finish_run` write is a completion candidate;
checkpoint publication must succeed before it becomes `agent_finished`. Thus a
checkpoint failure remains an infrastructure failure, as in Python. Normal end
precedence is agent completion, token exhaustion, then session exhaustion.

Checkpoint effects have an 80-second bound (stop plus snapshot), publication
20 seconds, and cleanup 20 seconds, independent of the expired work deadline.
Cleanup failure is separate from the primary reason and never erases it. A
checkpoint is visible as `last_checkpoint` only after publication acknowledgment.
Publication failure preserves the preceding checkpoint. A partial startup still
receives cleanup. These explicit race/deadline rules clarify behavior previously
dependent on synchronous Python calls; they do not promise preemption of real
Docker/network work until those adapters exist.

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
Its consecutive and whole-run ceilings never reset the allocation. Partial tools
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
test artifacts, not frozen experiment archives. Existing protocol `RunMode`
already carries `simulation`. API/report/portable-manifest integration is deferred
to their owning issues, where that label must remain explicit.

Context estimates, prompts, tool descriptions, and delivered tool truncation
retain Python's canonical ASCII JSON sizing. Evidence uses sorted UTF-8 JSON,
the same 4096-byte ceiling, file/command ordering, excerpt budgets, and unknown
field semantics. File comparison includes permissions, entry types and link
targets as well as content hashes and lengths. Artifact hashes refer to exact
bytes. Evidence source references
identify checkpoint artifacts and observation sequences; #19 supplies durable
record hashes and validates archived provenance.

Ordinary observations reserve 65536 log bytes for finalization, matching Python.
An exhausted log stops admission; the result remains available separately even
when a deliberately tiny log cannot fit all final observations. Adapter input
must also be bounded before allocation: controller checks do not make reading
an unbounded network frame safe.

## Validation and next boundary

Rust checks cover deterministic repeated traces, independent Python recovery and
evidence oracles, and 128 fixed-seed property cases for monotonic completed
accounting, admission within allocation, and no dispatch after a stop decision.
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

The simulation's JSON workspaces remain explicitly synthetic. Real adapters,
process supervision, and recovery require their owning tickets. The
[transcript contract](rust-archive-bridge.md) defines protocol-only decoding,
reviewed mapping fixtures, and the separation between raw copies and canonical
accounting/publication authority.
