# Rust foundation architecture decision

Tracking: [#16](https://github.com/aortez/playtime/issues/16), with the bounded
foundation scope in [#17](https://github.com/aortez/playtime/issues/17).
Branch `17-rust-foundation` starts at the specified Python baseline
`ad7a2f2a5a6947d396f601a43fe622716e41200c`. Its tree is identical to the merged
`main` revision `ca276261605632701b018bb0a3b879eecc80d107` inspected at startup.

Status: implemented foundation for #17, now extended by #18's bounded controller
and deterministic in-memory driver in `playtime-engine`. See the
[controller contract](rust-controller.md). There are no real Rust adapters,
service, storage engine, or desktop application. The Python launcher remains
the operational entry point.

## Agreed boundaries

`playtime-core` owns pure experiment values, validation, and accounting without
I/O or a runtime. `playtime-protocol` owns versioned client/event contracts and
serialization/schema dependencies. Neither depends on a service or UI.

`playtime-engine` now implements orchestration decisions and simulation adapters;
real adapters and the internal `playtime-worker` binary follow later. Later
packages are `playtime-server` (`playtimed`), `playtime-client`
(shared reqwest client), `playtime-cli` (`playtime`), and `playtime-ui` (egui).
Add these in their owning issues when they have implementations.

The service owns admission. Each run has a separate worker, supervised by a
Linux systemd user service. A client/API restart leaves a surviving worker
running. A confirmed worker crash becomes an interrupted result with the last
valid checkpoint; continuation requires an explicit new allocation. The Rust
and Python implementations must share mutual exclusion for a data root.
The existing coordination point is an exclusive `flock` on
`<data-root>/.playtime/run.lock`. The service must also account for a starting
legacy systemd unit before admitting work; checking a PID or replacing the lock
file is insufficient. #20 implements this coordination, with no queue or worker
auto-restart.

Portable run folders own frozen inputs, experimental events, checkpoints, and
results. SQLite owns admission/idempotency and orchestration ledgers; query
indexes and reports are derived. The lifecycle is preview → freeze verified
inputs → admit worker → publish checkpoints → publish result. Publication and
reconciliation details belong to #19; supervision and API behavior to #20.
Inputs are frozen before admission, and worker startup verifies the frozen
identities again. Client disconnect does not cancel work. Cancellation, failed
inference, and incomplete cleanup remain recorded outcomes. Operational tracing
is separate from the durable experiment event history.

Worker identity is independent of viewer/client/service identity. Strict repeat
will verify the frozen worker executable and inputs, recording source/build/
dependency provenance and platform requirements separately. Rust execution is
a new controller baseline; historical exact repeats retain the archived Python
route. Source plus a lockfile is not a promise of bit-identical rebuilding.

## Values and accounting

`UsageTotals` aggregates completed request outcomes. Known zero generation is
valid. Unknown usage carries a positive dispatched allowance and a request
count; subsequent successes do not erase either. Overflow rejects the entire
update. The future controller must separately reserve before dispatch and
settle requests exactly once; this accumulator does not implement admission.

Run, session, and request IDs are distinct opaque types. They accept 1–128 ASCII
letters/digits/underscores/hyphens with an alphanumeric first character; clients
must not infer ordering, timestamps, or filesystem locations from them. Event
sequence is a positive `u64`, strictly increasing per run. `EventOrder` accepts
gaps for filtered observations; it does not certify a complete durable log.

Seconds, tokens, byte counts, and CPU cores have explicit field names. Requested
limits reject unknown keys, invalid types, invalid bounds, conflicting reserves,
and arithmetic overflow. Zero sampling seed and measured zero tokens are valid;
missing usage has a distinct positive reservation. The context configuration
check preserves the Python baseline; admission of each request needs the full
conversation/context/handoff checks in #18. `RunParameters` is the bounded
foundation subset, not a complete recipe or the future override API.

Command exits, timeouts, and absent results are separate outcomes. Missing,
available (including empty), and unavailable memory are separate states.
Cleanup can be complete, failed with errors, or unknown. These are observations;
the engine implements transition and cancellation rules separately in #18.

Archive paths are lexical relative paths without empty, dot, parent, backslash,
NUL, or colon components. This is not permission to open/extract them: #19 must
also enforce root containment, symlink rules, file types, and size limits.
Event times are UTC RFC 3339 strings with `Z`, years 0001–9999, seconds 00–59,
and optional 1–9 fractional digits. The parser rejects invalid dates and local
offsets instead of normalizing evidence timestamps.

## Versioning and frozen identity

| Contract | Version | Reader |
| --- | --- | --- |
| HTTP DTOs | `/api/v1` | `v1`; endpoints belong to #20 |
| Event envelope | `schema_version: 1` | `events::decode_event` |
| Frozen input manifest | `schema_version: 1` | `archive::decode_input_manifest` |

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
manifest reference. #19 defines its pre-start publication/admission record.
Do not deserialize and reserialize bytes before verifying their digest.

Executable, source artifact, and Cargo.lock references are distinct. Compiler,
target, build profile, revision/dirty status, and platform requirements are
separate provenance, not substitutes for executable identity. GUI/CLI/service
rebuilds do not change worker identity. Concrete byte verification, compatible
host checks, and archived execution routes belong to #19/#27. Existing Python
hashes retain their own serializer and validation rules.

## Fixtures and checks

The [fixture notes](../crates/playtime-protocol/tests/fixtures/README.md) describe
the two-session trace, its exact referenced bytes, and synthetic Python excerpts.
The trace is labeled `simulation` and includes fresh context, persisted workspace
and memento, separate evidence, unknown/known usage, and terminal cleanup.
It describes an outcome; it does not execute an experiment.

The schema golden fixes the event/error envelopes and archive/read-model shapes.
Known event payloads are validated by Rust types; their detailed schemas can be
extended alongside #18's additional event kinds. The generic `data` schema
allows arbitrary JSON so future kinds remain inspectable.

`.github/workflows/foundation.yml` runs formatting, Clippy, Rust tests, and the
Python suite on Linux with the pinned toolchain and Python 3.12. Tests require
no model, GPU, Docker daemon, display server, service manager, or secrets.
Docker tests remain explicitly opt-in. See README for the exact local commands.

Local validation on the foundation branch: formatting and Clippy with warnings
denied passed; 25 Rust tests passed; the Python suite ran 91 tests with 6 explicit
Docker skips. The GitHub workflow is provided but has not yet run remotely.

## Following work

The [compatibility matrix](rust-compatibility.md) assigns operational parity
to the later migration issues. #18's [controller contract](rust-controller.md)
defines transitions/effects, request reservation/settlement, the deterministic
scenario grammar, and cancellation/checkpoint/limit precedence. Before #19, settle
file/SQLite publication, durability, and restart reconciliation. Both build on
these contracts. Real adapters, service supervision, authenticated remote access,
portable reports, and strict repeat remain separate deliverables.
