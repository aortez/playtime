# Controller/archive transcript contract

This contract defines protocol decoding, lossless controller facts, late
checkpoint acknowledgments, and verified storage traversal. The
[incremental durable driver](rust-archive-driver.md) connects it to real archive
publication. [Storage](rust-storage.md) owns confirmed frontiers and file policy;
[worker lifecycle](rust-worker-lifecycle.md) owns admission and control.

## Observation contract

The version-1 event envelope gains kind `controller_observation`. Its data is:

```json
{
  "schema_version": 1,
  "controller_sequence": 14,
  "elapsed_ms": 300,
  "session_id": "s1",
  "observation_kind": "tool_dispatch",
  "identity": {
    "effect_id": 4,
    "request_id": "r1",
    "tool_request_sequence": 13
  },
  "data": {"storage": "inline", "value": {"name": "exec", "request_sequence": 13}}
}
```

The source record's fields and original JSON value are unchanged. Controller sequence and elapsed time remain in
their original domains; the envelope owns archive sequence and UTC time. A
tool's stable identity is `(run_id, original tool_request sequence)`. Effects
and request IDs are scoped to the run. Null associations mean not recorded,
never proof that an effect did not occur. Known model observations require
session/request/effect identity; tools require their source request association.
Rejected tools may have no external effect. The driver must capture associations
when processing the effect/outbox, before later transitions replace them.

`decode_event` returns `EventPayload::Observation`, separate from canonical
`KnownEvent` lifecycle/accounting. `decode_observation` provides typed known
payloads plus the complete original wrapper JSON. Unknown kinds and optional
fields remain accessible. Unsupported wrapper versions and malformed known
records fail explicitly. Raw model chunks remain raw, including malformed model
fields: text interpretation can fail without deleting the observed chunk.

Final model response fields replace corresponding stream previews. Chunks stay
available as source evidence; do not concatenate the final text onto them.
Absent thinking is `NotRecorded`; recorded empty text is distinct. Text status
can be partial, complete, or truncated only from observed completion/limit
evidence. Tool results preserve delivered output, an optional unabridged result,
and original truncation/error flags. No formatter infers missing content.

`tool_dispatch` and `memento_write` are **intent** names in the engine.
Typed decoding calls them `ToolIntent` and `MementoWriteIntent` to preserve that
distinction. `tool_not_dispatched` cancels the intent; no command completion is
invented. `request_not_dispatched` releases an unstarted reservation. A durable
intent without a durable non-dispatch proof must retain its allowance as
possibly dispatched/unknown during recovery.

`ObservationLocation` maps original controller sequence to archive sequence and
exact JSONL SHA-256, including LF. It is a derived index: rebuild it by reading
verified committed records, and reject duplicate source identities. Never store
a record's hash inside that same record. Evidence references resolve through
this mapping without changing text already delivered to the model.

The [three mapping fixtures](../crates/playtime-protocol/tests/fixtures/controller-observations/README.md)
cover final text superseding chunks, a cancelled tool intent, and partial output
followed by failed inference. An engine test independently executes their
scenarios and checks every wrapped source record and effect association.

## Controller and storage interfaces

`Controller::take_control_updates()` drains a bounded queue independently of raw
logging. Drivers must drain it before further transitions when its reserve is
full, and retain the drained facts until publication resolves. `take_observations()`
adds stable identities without changing `take_records()` or its behavioral
fixtures. `pending_info()` provides a compact effect intent. `ScriptedAdapters`
exposes incremental dispatch, scheduled inputs, and synchronous cancellation;
the original simulator uses these same adapters.

`Input::ExternalLogBudgetExhausted` supplies actual archive capacity exhaustion.
`Input::PublicationCommitted` acknowledges an exact dispatched checkpoint even
after timeout or cleanup, preserving the first stop reason and never starting a
new session on a late receipt. Wrong, duplicate, and unstarted acknowledgments
are rejected. An ordinary `Outcome::Published` remains available to existing
drivers, but must only represent a successful durable receipt.

`ArchiveWriter::open_prepared` acquires writer ownership, verifies the expected
archive identity and every frozen input, requires an empty event file and exact
zero marker, and confirms durability before returning a live receipt. It cannot
resume a nonempty archive or hide an uncommitted tail. Execution authorization
belongs to the caller. Storage validates checkpoint-index/source-map references
and their content leaves; model JSON is never searched for paths. Source maps
must name exact preceding committed observations, including their LF bytes.

## Durable driver mapping

Canonical facts come from the lossless typed control-update drain, never by
scraping the byte-limited raw outbox. The raw copies remain
inspectable but may be explicitly omitted when their allowance is exhausted.
The implemented typed drain supplies admission/settlement/non-dispatch, session boundaries,
checkpoint receipts, the first stop decision, and the final result even when
ordinary observations cannot fit. Actual capacity exhaustion enters the
controller through its external-log-budget input and stops with `log_limit`.

| Boundary | Required archive behavior |
| --- | --- |
| Prepared archive / permission to Begin | Commit canonical `run_started` against the exact prepared identity before experimental effects. An existing folder is not execution permission. |
| `take_records()` | Move records into an owned pending batch. Retain records and associations until commit succeeds; never retry an uncertain writer. |
| Phase observation | Keep the source wrapper and deliberately emit the typed canonical phase event. |
| Model/tool pending effect | Commit intent, effect/request identity and allowance; recheck cancellation and current deadlines after I/O; acknowledge `Dispatched` with current time; recheck the pending ID; only then invoke the adapter. |
| Typed request settlement | Emit one canonical `request_finished` for that request with the exact known or unknown charge. Raw `request_settled` is a copy, not another charge. No zero-token settlement stands in for non-dispatch. |
| Session start | Publish the exact incoming memory and separate evidence, then emit `session_started` with the selected workspace and content references. Fresh conversation policy stays in the controller. |
| `PublishCheckpoint` effect | Publish and verify full workspace, file index/changes, incoming/outgoing memory, evidence/provenance, and a checkpoint content manifest. Commit typed publication before acknowledging `Input::PublicationCommitted`. |
| `controller_checkpoint` | Preserve the internal observation; it is not a second publication transaction. Only the durable publication can advance archive availability. |
| Cleanup / `controller_finished` | Derive canonical `run_finished` from the typed outcome with verified settlements and the last published checkpoint. If it cannot commit, the archive remains incomplete even if the in-memory controller finished. |

The driver must cancel/join abandoned external effects before conflicting
workshop work. Observation batching targets 250 ms or 64 KiB, with immediate
commits before dependent effects and lifecycle acknowledgments. Larger valid
records commit individually. Only successful writer receipts feed live reads.
Actual serialized envelopes and mapping metadata count against the frozen log
allocation, including the 65,536-byte finalization reserve. Prewrite capacity refusal feeds `ExternalLogBudgetExhausted`; uncertain I/O
failure feeds `PersistenceFailed` and retires the writer. Neither permits new
experimental effects. The integration driver services timers while a
serialized writer thread owns pending I/O; the service owns worker termination.

The checkpoint content manifest must expose immutable file-version references,
session/checkpoint provenance, original metadata, and explicit unavailable
content. Individual retained images must be downloadable without extracting a
workspace archive. The daemon maps verified references to opaque artifact IDs; source
paths and unpublished workspace contents are not remote API capabilities.

## Large observation bodies

The engine permits model responses up to 4 MiB and chunks up to 512 KiB
under its own encoded-size accounting. Storage defaults to 1 MiB per JSONL
record, and the HTTP API also specifies 1 MiB per event (with a separate SSE framing
allowance).
Adding an inline wrapper can exceed those bounds even when the source response
is valid. The representation uses a 256 KiB inline value threshold.

Larger original data values are immutable JSON artifacts. The same observation
header retains sequence, elapsed time, kind and identities, with
`data: {"storage": "artifact", "content": <ContentRef>}`. Protocol decoding
returns an explicit referenced payload until exact bytes are fetched; `resolve`
verifies length and SHA-256 before typed interpretation. Both representations
yield the same source record. Clients fetch through the opaque artifact endpoint.
Storage traversal verifies these known references; arbitrary model JSON does
not grant file access. Externalized payload bytes still consume the frozen log
allocation. This representation neither truncates content nor raises event
limits; it is not a claim that a reference has already been published.

A late checkpoint commit requires a distinct typed acknowledgment path. It must
register the last durable checkpoint without replacing the first stop reason or
starting another session, even after the ordinary publication effect timed out.
The final archive outcome waits until that publication result is settled. An
unresolved operation can still commit; a timeout does not establish failure.

## Integration and recovery boundary

The [durable driver](rust-archive-driver.md) executes two sessions through the
production controller with scripted adapters and real storage. Its acceptance
test closes the writer, copies only the run folder, removes the source, and
reads identical observations, accounting, memory, evidence, and individual files
without a database. Failed or unresolved publication returns incomplete evidence
and the last actual receipt; it never silently resumes work.

The [daemon](rust-daemon.md) connects authorized workers and archives to the
public API. The [read-only recovery planner](rust-recovery-contract.md) cannot
acquire ownership or mutate an archive. Neither a discovered marker nor a timeout
gives a service authority to execute or reconcile a worker. Automatic archive
recovery remains unimplemented.
