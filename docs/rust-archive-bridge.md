# Controller/archive bridge refinement

Tracking: [#19](https://github.com/aortez/playtime/issues/19). This first slice
implements protocol decoding and an executable mapping contract. It does not
yet connect the live controller to `ArchiveWriter`, publish checkpoint content,
recover interrupted archives, or read Python archives.

## Pinned inputs

- Controller: `0d781da14e49bc51aa4fd22e4bd0718c003c23ef`, merged in #41. Its tree
  matches reviewed branch head `1a31bc09d25046491f29b17ed8924cbd63c55431`;
  controller source originated in `7b2fce1c55a59b5ed87de09d1cbf3d1ae21b2af1`.
- Storage: `6584db01831878bd5e764b4b48a3b3f6753589b3`, merged in #38 and included
  in the controller merge. Layout, confirmed frontiers, and error semantics are
  defined in [rust-storage.md](rust-storage.md).
- Worker ownership/recovery: #34's admission, Begin, and cancellation contract,
  inspected October 17, 2026. Its implementation is not a dependency of this
  mapping fixture; production reconciliation requires its reviewed adapter.

## Observation contract implemented here

The version-1 event envelope gains kind `controller_observation`. Its data is:

```json
{
  "schema_version": 1,
  "record": {
    "sequence": 14,
    "elapsed_ms": 300,
    "session_id": "s1",
    "kind": "tool_dispatch",
    "data": {"name": "exec", "request_sequence": 13}
  },
  "identity": {
    "effect_id": 4,
    "request_id": "r1",
    "tool_request_sequence": 13
  }
}
```

The source record is unchanged. Controller sequence and elapsed time remain in
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

`tool_dispatch` and `memento_write` are **intent** names in the pinned engine.
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

## Durable driver mapping to implement next

| Boundary | Required archive behavior |
| --- | --- |
| Prepared archive / permission to Begin | Commit canonical `run_started` against the exact prepared identity before experimental effects. An existing folder is not execution permission. |
| `take_records()` | Move records into an owned pending batch. Retain records and associations until commit succeeds; never retry an uncertain writer. |
| Phase observation | Keep the source wrapper and deliberately emit the typed canonical phase event. |
| Model/tool pending effect | Commit intent, effect/request identity and allowance; recheck cancellation and current deadlines after I/O; acknowledge `Dispatched` with current time; recheck the pending ID; only then invoke the adapter. |
| `request_settled` | Emit one canonical `request_finished` for that request with the exact known or unknown charge. No zero-token settlement stands in for non-dispatch. |
| Session start | Publish the exact incoming memory and separate evidence, then emit `session_started` with the selected workspace and content references. Fresh conversation policy stays in the controller. |
| `PublishCheckpoint` effect | Publish and verify full workspace, file index/changes, incoming/outgoing memory, evidence/provenance, and a checkpoint content manifest. Commit typed publication before acknowledging `Outcome::Published`. |
| `controller_checkpoint` | Preserve the internal observation; it is not a second publication transaction. Only the durable publication can advance archive availability. |
| Cleanup / `controller_finished` | Derive canonical `run_finished` from the typed outcome with verified settlements and the last published checkpoint. If it cannot commit, the archive remains incomplete even if the in-memory controller finished. |

The driver must cancel/join abandoned external effects before conflicting
workshop work. Observation batching targets 250 ms or 64 KiB, with immediate
commits before dependent effects and lifecycle acknowledgments. Larger valid
records commit individually. Only successful writer receipts feed live reads.
Actual serialized envelopes and mapping metadata count against the frozen log
allocation, including the 65,536-byte finalization reserve. A size/I/O failure
feeds `PersistenceFailed` into the controller and prevents new experimental
effects. Sync storage alone does not establish interruptible I/O deadlines;
#20's driver/supervisor owns that execution boundary.

The checkpoint content manifest must expose immutable file-version references,
session/checkpoint provenance, original metadata, and explicit unavailable
content. Individual retained images must be downloadable without extracting a
workspace archive. #35 maps verified references to opaque artifact IDs; source
paths and unpublished workspace contents are not remote API capabilities.

## Remaining contract decision: large observation bodies

The pinned engine permits model responses up to 4 MiB and chunks up to 512 KiB
under its own encoded-size accounting. Storage defaults to 1 MiB per JSONL
record, and #35 also specifies 1 MiB per event (with a separate SSE framing
allowance).
Adding an inline wrapper can exceed those bounds even when the source response
is valid. This small-fixture contract does not resolve that mismatch.

Proposed next change: publish an oversized original source record as an immutable
JSON artifact, and commit a small versioned reference observation carrying its
source identity and exact `ContentRef`. Extend storage verification and protocol
decoding together so a client can fetch and decode the full record through the
opaque artifact endpoint. Inline and referenced records must yield the same
typed transcript. Review that representation before using it for the execution
gate; do not silently truncate, raise API limits, or treat an unverified artifact
as available evidence. Include exact-boundary and Unicode/escaping cases.

## Recovery handoff and subsequent gates

The intended recovery entry point consumes a #34-owned exclusion guard, the
exact admitted archive identity, and termination/cleanup evidence. The guard
must hold both the shared legacy run exclusion and archive writer exclusion
through reconciliation and publication. It is not a public boolean or a socket
timeout. Ownership uncertainty blocks this entry point and competing admission.

After verified termination, the reconciler verifies the committed prefix,
preserves any uncommitted suffix as a separate diagnostic artifact, restores the
append boundary, and appends interruption/recovery with new archive sequence
numbers. It preserves settlements, non-dispatch proofs, the last checkpoint,
and observed cleanup status. A committed outcome stays authoritative even when
cleanup is unresolved. Corruption inside the committed prefix is refused.
No recovery action may restart a worker or reissue model/tool effects. The
concrete guard/adapter is to be pinned against #34's reviewed implementation.

First integration gate: execute two sessions through the production controller
with scripted external adapters and real storage, close it, copy only the run
folder, and read identical observations/accounting/content references without
SQLite. Test commit/dispatch/acknowledgment failure boundaries and retained
outboxes. Fixture replay does not meet this gate.

Subsequent #19 work supplies complete synthetic Python-baseline archives,
read-only legacy adapters, detached initial-input reconstruction, and controlled
reconciliation tested with #34. This mapping PR does not close #19.
