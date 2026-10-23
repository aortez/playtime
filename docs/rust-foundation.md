# Rust foundation architecture decision

The Rust workspace separates pure values, wire contracts, execution policy,
external effects, durable evidence, service coordination, and presentation. This
guide records those boundaries and the foundation's value/versioning contracts.
See [architecture and operation](rust-mvp.md) for the complete run path and
[compatibility rules](rust-compatibility.md) for the retired Python parity policy.

## Agreed boundaries

`playtime-core` owns pure experiment values, validation, and accounting without
I/O or a runtime. `playtime-protocol` owns versioned client/event contracts and
serialization/schema dependencies. Neither depends on a service or UI.

`playtime-engine` owns orchestration decisions and the deterministic simulator.
`playtime-runner` integrates it with synchronous `playtime-storage` through a
serialized writer and replaceable effects. `playtime-worker` supplies native
Docker/Ollama effects and private worker control. The daemon in `playtime-server`
owns admission and lifecycle coordination, separate from HTTP/auth modules.
`playtime-client` owns transport and shared durable client state;
`playtime-cli` and `playtime-ui` own presentation.

The service admits one active run per data root, with no queue or worker
auto-restart. Each run has a separate Linux systemd user service. Closing a
client does not cancel the worker. Restarting the daemon retains the reservation
and marks active runs reconciling; automatic reattachment and worker-loss recovery
are not implemented. Operator release requires verified termination and resource
cleanup and does not repair the archive.

Admission uses an exclusive `flock` on `<data-root>/.playtime/run.lock` and checks
for legacy workers before handoff. Checking a PID or replacing the lock file is
insufficient. The legacy exclusion remains a safety boundary, not a requirement
to run the frozen Python controller.

Portable run folders own frozen inputs, experimental events, checkpoints, and
results. SQLite owns admission/idempotency and orchestration ledgers; query
indexes and reports are derived. The service reserves admission, prepares frozen
inputs and the worker grant, then starts the worker. The worker verifies those
identities again before Begin. Cancellation, failed inference, and incomplete
cleanup remain recorded outcomes. Operational diagnostics are separate from the
durable experiment event history.

Worker identity is independent of viewer/client/service identity. Source/build/
dependency provenance and platform requirements are recorded separately from the
actual executable bytes. Source plus a lockfile is not a promise of bit-identical
rebuilding. Strict repeat is not implemented.

## Values and accounting

`UsageTotals` aggregates completed request outcomes. Known zero generation is
valid. Unknown usage carries a positive dispatched allowance and a request
count; subsequent successes do not erase either. Overflow rejects the entire
update. The controller separately reserves before dispatch and
settles requests exactly once; this accumulator does not implement admission.

Run, session, and request IDs are distinct opaque types. They accept 1–128 ASCII
letters/digits/underscores/hyphens with an alphanumeric first character; clients
must not infer ordering, timestamps, or filesystem locations from them. Event
sequence is a positive `u64`, strictly increasing per run. `EventOrder` accepts
gaps for filtered observations; it does not certify a complete durable log.

Seconds, tokens, byte counts, and CPU cores have explicit field names. Requested
limits reject unknown keys, invalid types, invalid bounds, conflicting reserves,
and arithmetic overflow. Zero sampling seed and measured zero tokens are valid;
missing usage has a distinct positive reservation. The context configuration
check is only one layer; request admission also applies the controller's full
conversation/context/handoff checks. `RunParameters` is the bounded controller
configuration; the [recipe profile](rust-recipes.md) adds texts, seeds and model
settings. An interactive override API is not implemented.

Command exits, timeouts, and absent results are separate outcomes. Missing,
available (including empty), and unavailable memory are separate states.
Cleanup can be complete, failed with errors, or unknown. These are observations;
the [engine](rust-controller.md) implements transition and cancellation rules.

Archive paths are lexical relative paths without empty, dot, parent, backslash,
NUL, or colon components. This is not permission to open/extract them: the
storage layer also enforces root containment, link rules, types, and byte bounds.
Event times are UTC RFC 3339 strings with `Z`, years 0001–9999, seconds 00–59,
and optional 1–9 fractional digits. The parser rejects invalid dates and local
offsets instead of normalizing evidence timestamps.

## Versioning and frozen identity

| Contract | Version | Reader |
| --- | --- | --- |
| HTTP DTOs | `/api/v1` | `playtime-protocol::api` and `playtime-server::http` |
| Event envelope | `schema_version: 1` | `events::decode_event` |
| Frozen input manifest | `schema_version: 1` | `archive::decode_input_manifest` |
| Archive layout | `schema_version: 1` | `storage::decode_archive_descriptor` |
| Event commit marker | `schema_version: 1` | `storage::decode_commit_marker` |

Each format has an independent major version, unrelated to application release.
The readers check the header before interpreting the body: an unsupported major
returns `unsupported_version`, not `malformed_data`, even if its body differs
entirely. Raw serializable envelopes are inspection data, not execution authority.

The event envelope fixes `schema_version`, `run_id`, `sequence`, `time`, `kind`,
and `data`. Compatible evolution may add optional fields or new event kinds.
Known payloads tolerate extra optional fields, and their original payload is
retained. Unrecognized kinds remain explicitly `Unknown` with the original JSON
payload. Malformed known kinds are errors, not disguised as unknown events.
Changing required fields, their meanings, or required enum values needs a new
kind or major version. Unknown events cannot authorize execution.

Errors have stable, extensible string `code`, readable `message`, optional opaque
`request_id`, and object `details`. Mutation/config inputs reject unknown
settings. A tolerant archive/view reader must never supply unchecked settings
to the worker. Read-time totals are checked for consistency before interpreting
a terminal result.

`inputs/manifest.json` identifies the effective configuration, initial workspace,
other frozen inputs, and independent worker artifacts. Each reference holds a
relative path, SHA-256 of the exact stored bytes, and byte length. Paths must be
unique and inside `inputs/`; the manifest excludes itself. The hash of the
stored manifest bytes lives outside it, represented by the `run_started` input
manifest reference. Storage also binds it in immutable `archive.json`; the service owns
the independent admission transaction.
Do not deserialize and reserialize bytes before verifying their digest.

Executable, source artifact, and Cargo.lock references are distinct. Compiler,
target, build profile, revision/dirty status, and platform requirements are
separate provenance, not substitutes for executable identity. GUI/CLI/service
rebuilds do not change worker identity. Storage verifies concrete bytes without
executing them; the worker package validator checks execution compatibility.
Archived repeat execution is not implemented, and Rust does not read Python
archives.

## Fixtures and checks

The [fixture notes](../crates/playtime-protocol/tests/fixtures/README.md) describe
the two-session trace, its exact referenced bytes, and synthetic Python excerpts.
The trace is labeled `simulation` and includes fresh context, persisted workspace
and memento, separate evidence, unknown/known usage, and terminal cleanup.
It describes an outcome; it does not execute an experiment.

The schema golden fixes the event/error envelopes and archive/read-model shapes.
Known event payloads are validated by Rust types; their detailed schemas can be
extended for additional event kinds with reviewed compatibility changes. The generic `data` schema
allows arbitrary JSON so future kinds remain inspectable.

The [development guide](development.md) lists current CI jobs and required checks.
Foundation tests use synthetic data and do not require Docker or a live model.
