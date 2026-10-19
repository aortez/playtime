# Durable controller archive driver

Tracking: [#42](https://github.com/aortez/playtime/issues/42), the execution slice
of [#19](https://github.com/aortez/playtime/issues/19). The implementation started
from merged controller/storage main `0d781da14e49bc51aa4fd22e4bd0718c003c23ef`.
`playtime-runner` integrates engine, protocol, and storage. Engine decisions stay
free of I/O; storage stays synchronous and independent of controller/runtime or
service policy. Python remains the operational model/Docker runner.

## Execute and inspect

Use a new output directory:

```sh
cargo run --locked --release -p playtime-runner --example archive_simulation -- --scenario crates/playtime-engine/tests/fixtures/two-session.json --output /tmp/playtime-simulation
cargo run --locked --release -p playtime-runner --example inspect_archive -- --run /tmp/playtime-simulation
```

The example executes the same controller and `ScriptedAdapters` used by the
in-memory simulator. It commits during execution, not after collecting a
finished simulation. Scripted commands are matched as data and mutate only the
in-memory workshop; no command text reaches a host shell. The standard scenario
finishes two sessions in 400 virtual milliseconds with 26 measured tokens.

The folder contains frozen inputs, exact JSONL events, checkpoints, individual
file contents, incoming/outgoing memory, and separate evidence and provenance.
After the writer closes, copy the folder and remove the original; the inspection
example needs only the copied folder and its own executable. It uses no service,
database, original checkout, model, Docker, or GPU. Inspection verifies bytes,
not the ability or permission to re-execute an archived worker.

`prepare_simulation` takes an explicit `WorkerBundle` and freezes the exact
scenario bytes and initial synthetic workspace. The demo deliberately calls
`capture_demo_worker` to read its bounded executing image through `/proc/self/exe`,
which remains correct after rename/unlink. It also supplies a build-time source bundle,
Cargo.lock, rustc/target/profile, and truthful revision/dirty metadata. The source
bundle contains the six compiling crates and root Cargo files, including their
compile-time fixture inputs; it excludes runs, images, and unrelated files.
The bundle records `demo_cargo_build_script_live_workspace_not_isolated` as its
capture method; it makes no isolated-build/reproducibility claim. These are not
the storage fixture's synthetic worker bytes. Production #20 preparation supplies
a separately validated worker bundle compiled from a source snapshot captured
before compilation in isolation. An API daemon must not call demo self-capture
and archive itself as the worker. Rebuild/repeat execution remains #27.

`run_prepared` takes an explicitly authorized prepared identity, verifies inputs
and the executing binary, then calls `ArchiveWriter::open_prepared`. That method
holds the writer lock, verifies the exact archive/manifest identity and all
inputs, requires a zero marker and an empty event file, and confirms durability.
It rejects tails, existing events, damage, and another writer. It neither grants
execution authority nor resumes an interrupted run.

## Dispatch and persistence

The driver owns one controller, one scripted external effect, and one serialized
storage operation. A storage thread owns `ArchiveWriter`; the controller thread
continues polling cancellation and `next_wakeup_ms` while I/O is pending.
`DriverClock` supplies monotonic time and UTC. `VirtualClock` advances only at
scripted deadlines or explicit injected advances while I/O is pending. Its UTC
is independently injected (the demo uses a fixed synthetic UTC origin); archive
sequence orders events and source `elapsed_ms` remains unchanged.

For each effect, the driver drains raw observations with their explicit bindings
and bounded typed control facts. It serializes the preceding records and an
`effect_intent`, including model request identity and maximum allowance, publishes
referenced objects, commits the batch, and waits for the writer receipt. It then
rechecks cancellation/current time, acknowledges `Dispatched`, rechecks the
surviving effect, and immediately initiates the adapter. There is no intervening
await or persistence operation. Completion flows through the controller; its
settlements commit before any dependent effect starts.

The raw outbox is not accounting authority. Canonical `request_finished` comes
once from `ControlFact::RequestSettled`; raw settlement records remain inspectable
copies. Measured zero, unknown allowance, and explicitly unstarted reservations
remain distinct. Cancellation/deadline between intent commit and dispatch emits
stable non-dispatch facts and releases only an unstarted reservation. An intent
without a committed settlement or non-dispatch proof remains unresolved.

Scripted cancellation synchronously retires its schedule, establishing ownership
before the next workshop operation. Already applied file mutations remain. A
real model/workshop adapter will need its own cancellation/join contract; this
simulation does not claim real process isolation or network preemption.

## Checkpoints and late receipts

A successful stopped snapshot produces the controller's `PublishCheckpoint`
effect. Its intent commits first. The driver then publishes exact synthetic
workspace bytes, individual regular-file objects, memory/evidence, a versioned
checkpoint index and source map, and commits `checkpoint_published`. Only that
receipt permits `Input::PublicationCommitted` to advance controller state.

A failed partial startup can still produce a checkpoint. Its incoming memory
is explicitly `not_delivered`, with no invented successful session start.

The acknowledgment names the exact dispatched effect. It can arrive after the
publication deadline or after cleanup, without rewinding time, replacing the
first stop reason, or starting another session. Wrong, duplicate, and unstarted
receipts are rejected. The driver delays canonical `run_finished` until every
publication has resolved and uses storage's last confirmed checkpoint. A raw
`controller_checkpoint` observation is not a second publication.

Indexes expose full bounded file metadata, changes, availability, exact incoming
and outgoing memory, separate evidence text/provenance, and original-observation
resolution. Links and nonregular entries remain metadata and are never opened.
Source maps refer only to preceding committed observations and hash exact JSONL
bytes including LF. Delivered evidence keeps its original controller numbers.
See [the transcript contract](rust-archive-bridge.md) for typed decoding and text
semantics.

Storage traverses only known observation payload references and
checkpoint-index to source-map/content leaves. It never searches arbitrary model
JSON for paths. Conflicting references, manifest cycles, unsupported traversal
representations, wrong source hashes/sequences, and content corruption fail
explicitly. Default bounds are 4 MiB per manifest, 10,000 references, and 512 MiB
of verified content, with existing storage path/link checks retained.

## Capacity and failure

Actual JSONL bytes, including envelopes and all canonical/control metadata, plus
externalized observation JSON bytes consume the frozen log allowance. Values up
to 256 KiB remain inline; larger values use exact verified content references.
Externalizing model text does not evade the log budget. Checkpoint/file objects
have separate content limits. Each JSONL event including LF is at most 1 MiB.

Control barriers commit immediately. Stream observations coalesce until 250 ms
or 64 KiB; larger valid lines commit individually. The driver retains immutable
batches through their publication result, accepts no further model chunks while
I/O is pending, and bounds queued bytes. A transition queue is bounded by the
content/batch limits; while a publication is pending only bounded cancellation
and deadline facts can accumulate alongside it. Source metadata is bounded by
storage's event limit.

Ordinary capacity leaves 65,536 bytes for finalization. Prewrite capacity
exhaustion feeds `ExternalLogBudgetExhausted`, selects `log_limit`, and explicitly
omits raw observations that cannot fit. Compact settlements, stop/checkpoint
references, and the outcome may use finalization space. If required facts cannot
fit, completion stays uncommitted; an in-memory result never substitutes for it.

An I/O failure retires the storage owner and queued records to bounded diagnostics,
feeds `PersistenceFailed`, and stops accepting model chunks/new experimental
work. There is no same-writer retry. Snapshot/cleanup remain bounded best effort
even when they cannot be archived. The first stop reason remains authoritative.

`DriverOptions::publication_wait` bounds waiting for unresolved storage (default
five real seconds). Expiry does not prove that I/O failed or cannot commit. The
driver returns incomplete evidence and a `PendingPublication` holding the storage
owner and immutable batch. The lock remains held until that thread actually
settles. An owner can poll its receipt for supervision; dropping the handle does
not cancel the operation. No terminal event is fabricated. Lost receipts likewise
remain incomplete from the driver's confirmed frontier even if offline inspection
later discovers a newer marker. Reconciliation is #43, not automatic continuation.

`DriverOptions::confirmed` accepts a cloneable read-only `LatestReceipt`. It
coalesces updates to one in-memory value, initialized only by the verified empty
writer receipt and advanced only after returned commit receipts. Failed/lost or
unresolved operations retain the preceding value. Sampling performs no filesystem
operation and never holds a lock across controller work or I/O. A late receipt
returned by `PendingPublication::poll` can update the same observer; it does not
fabricate a terminal event. #34/#20 own authenticated transport and service-ledger
persistence.

## Query interface and validation

`ArchiveQueries::offline` acquires storage's inspection lock for a verified fixed
snapshot; `live` requires a non-deserializable writer-confirmed receipt. Queries
return normalized run state, paged canonical/control/raw history, checkpoint
indexes, file versions, verified content, and original-observation resolution.
History includes exact JSONL bytes, preserving unknown envelope/payload fields.
Missing indexes/content, omitted observations, unknown command status, unsupported
schemas, and unsettled model intents remain explicit. Canonical usage does not
silently count an unresolved intent as a measured-zero request.

The acceptance tests compare actual controller records/dispatches/accounting,
copy an archive and remove its source, and verify exact hashes plus individual
memory/evidence/files. Reviewed mapping goldens cover multi-tool responses,
rejection/non-dispatch, measured zero, unknown exit, missing usage/partial output,
self/self-plus-evidence, and inserted archive sequences. Existing engine behavior
and foundation/schema fixtures remain intact.

Fault tests cover input tampering, prepared handoff across processes, intent
commit before/after marker replacement, cancellation/deadline before dispatch,
settlement/checkpoint/terminal failures, returned/lost late checkpoint receipts,
and an unresolved writer that can still commit after the driver returns. Storage
also tests each artifact/event/marker durability boundary and process crashes.
Streaming tests pin the time/size batching limits; capacity tests preserve usage
when raw records disappear and count externalized payload bytes.

Run workspace fmt, locked Clippy/tests, and Python unittest discovery. The existing
foundation CI workflow includes every workspace crate and example, so adding
`playtime-runner` needs no parallel CI pipeline. No model, Docker, systemd, GPU,
or active study is required.

Next execution work is #34 supervision and refined #43 recovery. #35's API and
scripted HTTP backend proceed independently in the other workspace. They meet
at #20, which connects authorized workers and these archives to real clients.
