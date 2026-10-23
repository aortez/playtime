# Recovery contract and read-only planning

`playtime-runner::recovery` implements the read-only R1 contract introduced in
#50 for #43. It can prepare an explicitly
recoverable simulation and compute a deterministic recovery proposal from
bounded evidence. It cannot acquire recovery ownership, stop a process, change
an archive, append events, truncate a tail, adopt a receipt, release admission,
or abandon a run. Those storage transactions and lifecycle integrations remain
unimplemented (#43 R2 and daemon integration). The daemon's operator release is
a separate lifecycle operation: it does not apply a recovery proposal or modify
archive evidence. See [development](development.md#gates-that-are-not-implemented)
for the incomplete restart/recovery acceptance gates.

## Preparation and execution

New static packages advertise `Capabilities::current().recovery_profile` as
`controller_archive_v1`. The optional capability is omitted on older packages;
their serialized capability shape is unchanged. `VerifiedBundle::open` still
validates the static executable, platform, hashes and build metadata.
`VerifiedBundle::prepare_recoverable` adds exactly one manifest-bound
`inputs/execution-contract.json`. Its runner counterpart accepts an explicitly
validated bundle and frozen descriptor/build inputs. The runner does not claim
to replace the production ELF/build verifier. Demo tests label self-captured
images explicitly and never use them as production bundle evidence.

Ordinary `prepare`/`prepare_simulation_with_inputs` remain additive compatibility
paths and do not advertise recovery. Both preparation paths reject caller-supplied
execution contracts. Missing, older or unsupported package capabilities cannot
opt in. Copying a contract into an old archive does not upgrade its worker.

`PreparedExecution::acquire` holds the prepared writer and verifies the actual
`/proc/self/exe` image, frozen scenario, original allocations, complete storage
limits and compiled recovery profile before creating adapters or a controller.
The production worker also verifies the archived package against its embedded
build record. The same prepared writer still survives through Begin. No Cargo
command runs during preparation, admission or execution.

The contract freezes all nine `StorageLimits` fields, required event/effect
kinds, accounting rule and the 65,536-byte finalization reserve. The reserve is
inside the original log allocation. Recovery control decoding rejects missing
required fields, unknown fields (including reused nested DTOs), duplicate JSON
keys, unsupported versions, inconsistent limits and checked-arithmetic overflow.
Ordinary readers retain inspection of older archives without a contract.

## Read-only inputs and decisions

`playtime_runner::recovery::plan_recovery` accepts `PlanningInput` and a bounded
iterator of `storage::Result<StoredEvent>`. It returns `Finalize(plan)`,
`PreserveTerminal(frontier)`, `NeverStarted`, or typed `Blocked` evidence.
All byte reads go through `PlanningContents`, whose provider must enforce the
supplied limit before allocating. The planner verifies exact descriptor,
manifest, marker, content and LF-inclusive record hashes, the entire committed
prefix and both saved confirmation boundaries. It never searches backward for
a convenient marker. Local limits can restrict, but cannot enlarge, frozen
limits. State is bounded by event/reference/content limits.

Inputs confer no ownership or mutation authority. R2 must construct them from a
held validated scan, authenticate saved writer receipts and original invocation,
prove Begin history and termination, enforce independent I/O deadlines, and
independently validate a returned proposal before any transaction. A serialized
frontier or discovered marker is never a writer receipt.

`inventory_bytes` and its ContentRef are exact bounded private preservation
evidence. `inventory_entries` is the count from the caller's bounded inventory
walk. R2 owns validation of the inventory entries and preservation of their
source bytes; R1 hashes the supplied inventory and reserves maintenance capacity.
It does not recursively discover content references inside diagnostic JSON.
Plans follow only typed input/checkpoint/observation leaves. A plan's private
inventory reference is not an ordinary artifact edge.

Accounting uses unique model requests/effects and exact session/cap matching.
A matching `request_not_dispatched` alone releases its original cap. A generic
`effect_not_dispatched` alone leaves that cap pending. Known zero, measured
overruns and existing unknown charges are preserved. Each remaining intent gets
one unknown settlement, in source-event order, charged at its original cap.
Observations cannot create or release charges.

The proposed terminal preserves the first canonical stop (or uses interrupted),
last committed checkpoint even beyond the last acknowledgment, and unknown
cleanup. It invents no controller observations or command/session outcomes.
Every proposed event uses the persisted attempt ID/time and sequential archive
sequence. A later acquired generation does not change the logical attempt.
The plan has no patch/self/target-marker digest or patch size; the dependency
order is inventory → plan → patch and marker → selection.

Log cost is exact JSONL bytes including LF plus each retained observation payload
occurrence, including repeated references. Omission sizes, checkpoint contents,
plans and diagnostics are not retained observation output. The exact proposed
patch must fit the original allocation and one batch, with record/event/ref and
content bounds and separate maintenance limits. Encoding previews cap batches at
the current 4 MiB default; R1 preparation freezes that same bound. The selection
contains final encoded sizes and capacity totals. It is a proposal, not a commit.

Valid terminals are preserved without touching suffix bytes. Recovery-authored
terminals additionally require an exact plan matching the independently scanned
base and an exact deterministic patch. Older interrupted archives without a
supported contract are blocked, while older verified terminals remain readable
and preservable. Empty history is `NeverStarted` only for an empty file, zero
marker, complete identity and caller-proved `ProvenNeverAuthorized`.

`inspect_reentry` classifies exact selected base/target markers and patch tails
without retrying anything. It identifies evidence for preserving the same patch
or verifying the selected terminal; intermediate markers, unexpected bytes and
missing selection after possible destructive work block. R2 must preserve and
validate held evidence before acting. These return values are not adoption or
truncation permissions.

## Validation and goldens

The exact synthetic fixture set lives in
`crates/playtime-protocol/tests/fixtures/recovery/`. Its README explains each
schema choice and the independent Python encoder/hash oracle. Tests never
regenerate goldens from the planner. All 27 issue review vectors execute through
accounting, log-cost, decision and read-only reentry tests. Additional rejection
tests cover missing/old capabilities, duplicate contract paths, wrong executable
and configuration, frozen-limit mismatches, versions/profiles, duplicate fields,
saved boundaries, corrupted references, incomplete patches and capacity limits.

A real scripted controller archive supplies an independent JSON/file oracle for
usage, retained byte cost, source hashes and exact proposed patches; no
`ArchiveQueries::run` accounting projection is used. The worker-package gate
executes an opted-in detached static worker and verifies rejection before
readiness for forged execution contracts. It is a CPU gate, separate from real
systemd enforcement and production daemon/CLI/workbench integration.
