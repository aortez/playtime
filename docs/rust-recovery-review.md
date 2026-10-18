# Recovery implementation handoff for #43

Refined against merged main b81db2a7aa4d38e702627ee78cceff2f171ca225 on
2026-10-18 UTC. This supersedes the review against separate #42/#34 branches.
The APIs below are implementation contracts, not implemented interfaces.
The companion JSON contains semantic review vectors, not archive/schema goldens.

## Outcome and scope

After confirmed worker termination and held takeover exclusion, preserve the
committed prefix, conservatively settle unresolved canonical model intents,
and publish at most one recovery terminal batch. Never restart the controller,
call adapters, execute archived commands or allocate more work. Keep live
writer receipts, exclusive recovery receipts and offline inspection distinct.

The CPU-only recover_interrupted example in
[#43](https://github.com/aortez/playtime/issues/43) remains the acceptance gate:
real controller/archive writer, scripted effects, verified child exit and a
labeled fixture authority. Actual systemd enforcement belongs to #34;
production API/CLI/UI recovery belongs to #20. #21/#22 stay open until their
production simulation gates pass.

## Merged-code review

| Evidence | Decision |
| --- | --- |
| runner prepare.rs: explicit WorkerBundle, empty InputManifest.files | Add opt-in frozen execution contract; absence remains inspect-only. |
| runner driver.rs: actual /proc/self/exe, scenario and log-bound checks before open_prepared | Independently verify supported contract before execution. #20 retains the same prepared writer across Begin. |
| storage writer.rs: open_prepared requires empty log/zero marker; uncertain writes stop owner | Separate exclusive recovery handle; no generic append/resume or stopped-writer retry. |
| storage log.rs already rejects duplicate settlements and inconsistent terminal totals/checkpoint | Preserve those checks; add ordered intent/session/non-dispatch validation. |
| runner queries.rs uses a request-keyed presentation map | Do not use that map as the recovery oracle. |
| runner mapping.rs charges JSONL plus retained observation payload bytes | Recompute that rule, including LF and every payload occurrence. |
| engine controller.rs preserves measured generation overruns | Do not clamp usage or reject a valid settlement merely for exceeding its cap/budget. |
| lifecycle ports.rs lacks fencing/takeover | #34 must prove termination and hold authority; timeout/generation fields are insufficient. |
| lifecycle Ledger holds its service flock through Arc ownership | Held authority owns a Ledger lease and run flock through blocking I/O and ledger adoption; no DB transaction spans external work. |

## Delivery slices

| Slice | Delivery | Dependencies and gate |
| --- | --- | --- |
| R1: contracts and planner | Versioned DTOs/exact goldens, opt-in preparation/worker checks, bounded read-only canonical planner | Ready in [#50](https://github.com/aortez/playtime/issues/50). Independent accounting vectors, malformed/order/overflow cases, producer mismatch checks and compatibility review; no mutation of interrupted archives. |
| R2: storage transaction | Authority trait, held scan/preflight, preservation, immutable selection, one-batch finalization/reentry | R1; truthful fixture authority suffices. Fault tests around preservation/truncation/patch/marker; no writer overlap, lost evidence or duplicate charges. |
| R3: lifecycle and reads | #34 production authority/attempt ledger, persisted recovery/public frontiers, query provenance, portable lifecycle resolutions | R1/R2 and #34 fencing. Live-worker takeover refused, lost acknowledgment adopted once, stale generation/status rejected. |
| R4: process demonstration | Kill scripted worker at an intent boundary, crash recovery, retry, inspect copied folder | R1–R3 fixture ports. Exact final bytes/totals/checkpoint, independent offline oracle and bounded owned-process cleanup. |

R1 is ready to implement. #43 retains its existing needs-refinement gate until
R1 freezes the compiled DTO/golden contract and #34 adopts the held authority
signatures. Documents and semantic vectors do not substitute for those checks.
No product choice or model/GPU access blocks R1.

Scheduling check: worker-package is now active in playtime, and
worker-supervision is active in playtime2. Their uncommitted files are not
dependencies of this review. Queue #50's overlapping runner/preparation edits
after the worker-package slice lands, then verify its committed bundle/profile
interface before implementation. The #34 supervisor work can continue.

## Frozen execution contract

Add playtime_protocol::recovery::ExecutionContractV1, referenced exactly once
as inputs/execution-contract.json in InputManifest.files. Strict independent
schema_version 1; all fields mandatory. Reuse existing IDs, ContentRef,
ArchiveIdentity, EventFrontier, UtcTime, StorageLimits, UsageSummary and
CheckpointRef. Unsupported control versions block mutation, not inspection.

| Field | Frozen meaning |
| --- | --- |
| rule_set | controller_archive_v1; authoritative/accounting changes need a new supported profile |
| event_schema_version, bridge_schema_version, observation_schema_version | Each exactly 1 |
| required_event_kinds | Sorted set of eight KnownEvent kinds, five BridgeEvent kinds and controller_observation; no missing/duplicate/unknown required kind |
| effect_kinds | Sorted: cleanup, model, publish_checkpoint, snapshot, start_session, tool |
| worker_executable, configuration | Exact manifest ContentRefs |
| generated_tokens, log_bytes | Original nonzero allocations, matching frozen scenario |
| storage_limits | Full frozen limits; event_log_bytes equals log_bytes; retries cannot enlarge them |
| finalization_reserve_bytes | Exactly 65,536 inside log_bytes; ordinary ceiling uses current saturating subtraction |
| observation_accounting | jsonl_plus_each_retained_payload_v1 |

Opt in only for a bundle whose validated capability metadata declares this
profile. #20 validates its separately built production bundle; self-capture
stays demo-only. Generic preparation must not silently advertise support for
arbitrary older bundles. The executing worker compares contract, actual image,
compiled profile and frozen scenario before creating the controller/effects.
A manually added contract cannot upgrade an old archive.

Existing archives without a contract stay readable. Preserve verified terminal
outcomes without invented settlements; interrupted unadvertised archives cannot
be automatically finalized. Review changes to existing goldens individually.

## DTO and hash boundaries

V1 mutation/control DTOs reject unknown fields, use checked u64 arithmetic and
bounded collections. IDs reuse 32-character lowercase hex ControlId. ContentRef
always hashes exact stored bytes, not a reserialized object.

| Type | Required fields/validation |
| --- | --- |
| RecoveryBindingV1 | Archive identity, original worker Binding (or reviewed production equivalent), original and superseding generation; exact run/manifest/executable agreement; no secrets or host paths |
| RecoveryAttemptV1 | Stable ID/chosen UTC, original binding/provenance and immutable maintenance limits; acquired generation is separate and may increase |
| ConfirmedBoundsV1 | Optional saved worker frontier and optional latest public frontier with worker/recovery provenance and attempt ID; verify both exact prefixes |
| SourceEventV1 | Archive sequence and record SHA-256 including LF; resolves inside base prefix, not controller sequence |
| RecoveryPlanV1 | Attempt/archive/base, prior bounds, execution-contract ref, initial inventory ref, unresolved intent source refs in source order, first stop/last checkpoint, existing/resulting usage, base accounted-log cost and available frozen capacity |
| RecoveryOriginV1 | schema_version, attempt_id, plan ContentRef; optional recovery_origin on authored request_finished/run_finished only; omitted when absent |
| RecoverySelectionV1 | Attempt/archive/base, exact plan/inventory/patch/target-marker refs, final frontier and exact capacity totals; recovery/intent.json selects the transaction |
| RecoveryResolutionV1 | Tagged terminal_preserved/never_started/abandoned_incomplete; archive/attempt and verified frontier or evidence fingerprint; abandonment includes stable operation/review identity, reason and accounting availability |

Keep dependencies acyclic: inventory, then plan, then patch/target marker,
then selection. Plan contains no self digest, patch digest, target-prefix digest
or exact encoded patch size. Final encoded-size/capacity results belong in
selection/private preflight, avoiding a plan-reference size dependency.
Nothing references selection. Plans follow only typed input/checkpoint leaves;
source hashes bind exact events. Diagnostics have a separate bounded reader
and never enter the ordinary artifact graph through arbitrary JSON traversal.

## Ownership and storage port

Proposed synchronous storage API; structs have private fields and no Clone or
Deserialize. The authority is a trusted internal adapter boundary. Honest
fixture implementations are allowed; DTO deserialization never grants authority.

~~~rust,ignore
pub trait RecoveryAuthority: Send + 'static {
    fn binding(&self) -> &RecoveryBindingV1;
    fn attempt(&self) -> &RecoveryAttemptV1;
    fn acquired_generation(&self) -> u64;
    fn confirmed_bounds(&self) -> &ConfirmedBoundsV1;
    fn begin_history(&self) -> BeginHistory;
}
pub enum BeginHistory { ProvenNeverAuthorized, AuthorizedOrUncertain }
pub struct RecoveryHandle<A: RecoveryAuthority> { /* held guards */ }
pub struct RecoveryInspection { /* validated, handle-bound scan */ }
pub struct ValidatedFinalization { /* exact private preflight result */ }
pub struct RecoveryReceipt { /* recovery publication confirmation */ }

impl<A: RecoveryAuthority> RecoveryHandle<A> {
    pub fn acquire(root: &Path, authority: A, limits: StorageLimits,
        observer: Box<dyn PublicationObserver>) -> storage::Result<Self>;
    pub fn authority(&self) -> &A;
    pub fn inspect(&mut self) -> storage::Result<RecoveryInspection>;
    pub fn events<'a>(&'a self, scan: &RecoveryInspection)
        -> storage::Result<RecoveryEvents<'a>>;
    pub fn preflight(&mut self, scan: &RecoveryInspection, plan: RecoveryPlanV1)
        -> storage::Result<ValidatedFinalization>;
    pub fn finalize(&mut self, plan: ValidatedFinalization)
        -> storage::Result<RecoveryReceipt>;
    pub fn adopt_selected(&mut self) -> storage::Result<RecoveryReceipt>;
}
~~~

RecoveryEvents yields bounded Result<StoredEvent> items with exact JSONL bytes.
Scan/event access checks private handle identity and base fingerprint. The runner
consumes scan/events into protocol plan data. Storage independently checks allowed
record kinds, identity/base, typed references, quotas and terminal consistency;
storage never imports runner/server logic or trusts a serialized validated plan.

Hold service -> run -> archive writer exclusion. Production authority owns a
Ledger lease, run flock and supervisor fence. Before trying the run flock, #34
must settle/cancel any pending original start, revoke Begin under the grant lock,
prove original invocation/cgroup termination and prevent delayed start. Serialize
against the launch lane. Revocation alone cannot stop an already-started worker;
a missing manager/PID/socket cannot prove death. Recheck durable identity and
generation under exclusion.

Persist attempt ID/time and new generation before recovery mutation; reuse the
same attempt on restart. A bounded owned blocking job retains authority through
I/O and ledger adoption even if the caller times out/disconnects. No second job
or slot release while it can write. Uncertain mutation stops the handle;
retry needs a new handle and freshly proved authority.

Private #34 coordinator signatures:

~~~rust,ignore
async fn acquire_recovery(&self, run: RunId, expected_revision: u64)
    -> lifecycle::Result<HeldRecoveryAuthority>;
async fn accept_recovery(&self, authority: &HeldRecoveryAuthority,
    receipt: &RecoveryReceipt) -> lifecycle::Result<PublishedRecovery>;
~~~

#34 extends its trusted supervisor port with an owned fence:

~~~rust,ignore
async fn fence_and_confirm_exit(&self, launch: &PreparedLaunch)
    -> lifecycle::Result<Box<dyn InvocationFence>>;

pub trait InvocationFence: Send + Sync + 'static {
    fn binding(&self) -> &Binding;
    fn evidence(&self) -> &TerminationEvidenceV1;
}
~~~

The fence is non-Clone/non-deserializable and returned only when the exact
invocation has no possible pending start or local writers. Evidence distinguishes
an owned fixture child's reaped exit from verified systemd job/cgroup termination;
it is not a reusable PID/heartbeat claim. Dropping a caller cannot cancel the
owned fencing operation or release its launch/admission exclusion. The production
authority retains the fence. Unknown manager/job/identity state returns uncertainty.

The ledger stores a stable recovery attempt, selected-plan identity when known,
latest accepted recovery frontier and separately latest published frontier,
blocked status, plus any explicit resolution operation. Migration must preserve
service/key/run/active reservation and require explicit new fields in the new
record version. Do not synthesize a full archive frontier from an old fixture
Status or marker. Existing owner_generation/revision remain required; failed
or ambiguous migration cannot reset admission history.

Persist verified frontier/provenance under the acquired generation in a short
FULL-synchronous transaction before public reads/cursors. Retain worker
confirmation separately. Lost acknowledgment adopts the same selected result.
Slot release separately requires confirmed cleanup/resource exclusion.
No DB transaction spans filesystem/supervisor work.

Terminal preservation/never-started use separately validated resolution actions
under the held handle. Abandonment additionally needs #34's private
HeldResolutionAuthority: durable operation/review identity, unchanged evidence
and renewed cleanup proof. Ordinary authority or a boolean cannot authorize it.

## Canonical planner rules

Return Finalize(plan), PreserveTerminal(frontier), NeverStarted or
Blocked(reason, verified_evidence). Bound state by frozen event/reference limits.
Begin history comes from held authority, never archive JSON.

R1 exposes a read-only runner function:

~~~rust,ignore
pub fn plan_recovery(
    input: &PlanningInput,
    events: impl Iterator<Item = playtime_storage::Result<StoredEvent>>,
) -> runner::Result<RecoveryDecision>;
~~~

PlanningInput carries immutable contract, attempt, base/bounds and Begin-history
data; it confers no authority and supports synthetic tests without R2. R2 builds
it from the held validated scan and independently checks the proposed plan.
R1 must not depend on an unimplemented recovery-handle constructor or introduce
a placeholder mutation API merely to satisfy the planner's input type.

1. Verify identity/contract, complete committed prefix, references and both saved
   boundaries. No search backward for a convenient valid marker.
2. Require unique effect/model request IDs; bind intent session/request/nonzero
   cap. Settlements follow and match the intent. Duplicate/conflicting facts,
   wrong session/effect/cap and arithmetic overflow block.
3. A matching request_not_dispatched is sufficient to release its exact original
   cap. The current controller emits it before the generic effect fact, so a
   prefix ending there must remain uncharged. A generic effect_not_dispatched
   alone does not release a model allowance. If both exist, their identities
   must agree; do not require their joint presence or reverse their order.
4. Preserve measured usage, including zero and overruns. Unknown settlements
   retain the original cap. Add one unknown settlement per unresolved intent in
   intent-sequence order; none for a fully proved non-dispatched request.
   Raw observations never create/release charges.
5. Preserve first canonical stop_decided, else interrupted, and last committed
   checkpoint including a lost acknowledgment. Emit missing request_finished
   records then one run_finished, cleanup unknown, same RecoveryOrigin and
   persisted attempt UTC, sequence base+1 onward. No invented controller
   observations, command results, session transitions or successful cleanup.
6. Base log cost is exact JSONL including LF plus each retained observation
   artifact payload occurrence's verified size. Inline payload is already in
   JSONL. Omission size is diagnostic. Checkpoint/plan/diagnostic bytes obey
   their quotas but are not observation output.
7. Base cost plus exact patch must fit the frozen log budget, the one-batch
   bound (default 4 MiB), record/event/reference/content bounds. The finalization
   reserve is not another allocation.

Preserve an existing valid terminal byte-for-byte and leave its suffix.
Empty file/zero marker means never_started only with complete prepared identity
and ProvenNeverAuthorized. Empty-but-authorized/uncertain remains incomplete.
Unsupported required semantics block; optional observations stay inspectable.

## Publication and reentry

Preserve every suffix byte before truncation. Sync initial inventory, immutable
plan, exact patch/marker and all affected directories; no-replace publish/sync
selection last. Add fault hooks for diagnostics write/sync/publication,
selection publication, truncation and truncation sync alongside current
event/marker/directory hooks. Finalization is one batch; no intermediate
settlement commit is valid.

Maintenance defaults: 128 MiB newly created files, 256 new entries, 4 MiB per
plan/inventory/chunk, 64 KiB per selection/resolution, 10,000 inventoried
existing entries. Reserve 128 KiB/four entries for lifecycle resolution.
Count staged/orphan recovery files; no retry expansion or deletion for space.
The initial inventory describes preexisting evidence outside its own attempt's
generated files; no recursive plan/selection inventory. Reuse partial staging
only after exact name/identity/bytes verification. Conflicts block. Preserve
other orphan objects in place and do not follow special files/symlinks.

| Reentry evidence under fresh exclusion | Allowed action |
| --- | --- |
| No selection, unchanged original base/suffix, verifiable owned partial staging | Finish same attempt preparation; no prior destructive step allowed |
| Exact selected base marker; absent/original preserved suffix or exact partial/full patch | Verify preservation, truncate/sync to base and publish the same complete patch |
| Exact target marker and valid complete patch/references | Reconfirm durability and adopt receipt; append nothing |
| Missing/corrupt selection after possible destructive work; different/intermediate marker; shortened prefix; unexpected patch suffix | Preserve evidence and block; no new plan/charges or suffix promotion |
| Existing terminal without a selected patch | Preserve terminal; lifecycle resolution only |
| Adopted abandoned_incomplete | Inspection only; automatic recovery disabled |

External cleanup uncertainty can coexist with finalized evidence but blocks
conflicting admission. A stopped handle stays stopped even when bytes look full.

## Validation and readiness

[Semantic vectors](recovery-v1-vectors.json) pin accounting, identity conflicts,
empty authorization ambiguity, stop/checkpoint preservation, retained-payload
cost and reentry decisions. They omit real manifest/executable hashes and are
not valid archives. R1 must deliver actual DTO/JSONL goldens and exact hashes;
R2/R4 exercise real storage and synchronized process barriers.

R1 preserves old event bytes when recovery_origin is absent and independently
compares totals/cost to the driver. Later slices cover the full #43 matrix:
ENOSPC, uncertain sync, late checkpoints, live worker after coordinator loss,
pending start, PID reuse, stale generation and lost ledger acknowledgment.
Implementation requires workspace fmt, locked Clippy/tests and Python checks.
No model, Docker or GPU is needed; systemd is separately opt-in.
