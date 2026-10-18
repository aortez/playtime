use super::{contract_for, verify_bundle_support};
use playtime_core::{ArchivePath, GenerationUsage, RequestId, UsageTotals};
use playtime_protocol::{
    archive::{ContentRef, decode_input_manifest},
    bridge::{BridgeEvent, EffectIntent, decode_checkpoint_index, decode_source_map},
    events::{CheckpointRef, EventPayload, KnownEvent, MemoryRef, decode_event},
    observations::{ObservationData, ObservationLocation},
    recovery::*,
    storage::{EventFrontier, StorageLimits, decode_archive_descriptor, decode_commit_marker},
    v1::UsageSummary,
};
use playtime_storage::{StoredEvent, content_ref, sha256};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

type Check<T> = std::result::Result<T, RecoveryBlockedReasonV1>;
use RecoveryBlockedReasonV1::{
    EvidenceConflict, FinalizationLimit, Incomplete, IoUncertain, PreservationLimit,
    UnsupportedContract,
};

/// Read-only bytes supplied by a held scan in R2, or an explicit test provider.
/// Implementations must enforce `limit` before allocating and never write.
pub trait PlanningContents {
    fn read(&self, reference: &ContentRef, limit: u64) -> playtime_storage::Result<Vec<u8>>;
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BeginHistory {
    ProvenNeverAuthorized,
    AuthorizedOrUncertain,
}

/// Immutable observations, with no authority to mutate or acquire ownership.
/// Begin history/acquired generation must come from the caller's held authority
/// in R2; they are deliberately not deserialized from archived worker JSON.
pub struct PlanningInput<'a> {
    pub attempt: &'a RecoveryAttemptV1,
    pub acquired_generation: u64,
    pub base: &'a EventFrontier,
    pub confirmed: &'a ConfirmedBoundsV1,
    pub descriptor_bytes: &'a [u8],
    pub manifest_bytes: &'a [u8],
    pub marker_bytes: &'a [u8],
    pub initial_inventory: &'a ContentRef,
    pub inventory_bytes: &'a [u8],
    /// Count from the caller's bounded diagnostic inventory walk. R2 validates
    /// its entries separately; the planner never traverses diagnostic JSON.
    pub inventory_entries: u64,
    pub event_file_bytes: u64,
    pub begin_history: BeginHistory,
    pub limits: &'a StorageLimits,
    pub contents: &'a dyn PlanningContents,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedEvidence {
    pub attempt_id: playtime_protocol::worker::ControlId,
    /// Only present after the complete prefix and all its typed leaves verify.
    pub frontier: Option<EventFrontier>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RecoveryDecision {
    Finalize(Box<RecoveryPlanV1>),
    PreserveTerminal(Box<EventFrontier>),
    NeverStarted,
    Blocked {
        reason: RecoveryBlockedReasonV1,
        verified_evidence: Box<VerifiedEvidence>,
    },
}

pub fn plan_recovery(
    input: &PlanningInput<'_>,
    events: impl Iterator<Item = playtime_storage::Result<StoredEvent>>,
) -> crate::Result<RecoveryDecision> {
    let mut verified = None;
    Ok(match plan(input, events, &mut verified) {
        Ok(decision) => decision,
        Err(reason) => RecoveryDecision::Blocked {
            reason,
            verified_evidence: Box::new(VerifiedEvidence {
                attempt_id: input.attempt.attempt_id.clone(),
                frontier: verified,
            }),
        },
    })
}
fn ensure(ok: bool, why: RecoveryBlockedReasonV1) -> Check<()> {
    if ok { Ok(()) } else { Err(why) }
}
fn utf8(bytes: &[u8]) -> Check<&str> {
    std::str::from_utf8(bytes).map_err(|_| EvidenceConflict)
}
fn exact(reference: &ContentRef, bytes: &[u8], limit: u64) -> Check<()> {
    ensure(
        bytes.len() as u64 <= limit && reference.size_bytes <= limit,
        FinalizationLimit,
    )?;
    ensure(
        content_ref(reference.path.clone(), bytes) == *reference,
        EvidenceConflict,
    )
}
fn digest(hash: &Sha256) -> playtime_core::Sha256Digest {
    playtime_core::Sha256Digest::new(
        hash.clone()
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>(),
    )
    .expect("SHA-256 hex")
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Role {
    Input,
    Leaf,
    Checkpoint,
    SourceMap,
    Observation,
    RecoveryPlan,
}
struct Contents<'a> {
    source: &'a dyn PlanningContents,
    limits: &'a StorageLimits,
    refs: BTreeMap<ArchivePath, (ContentRef, Role)>,
    total: u64,
}
impl Contents<'_> {
    fn load(&mut self, reference: &ContentRef, role: Role, bound: u64) -> Check<Vec<u8>> {
        ensure(
            ["inputs/", "sessions/", "artifacts/"]
                .iter()
                .any(|p| reference.path.as_str().starts_with(p)),
            EvidenceConflict,
        )?;
        ensure(
            reference.size_bytes <= self.limits.content_bytes.min(bound),
            FinalizationLimit,
        )?;
        if let Some((old, old_role)) = self.refs.get(&reference.path) {
            ensure(
                old == reference
                    && (*old_role == role
                        || matches!(
                            (*old_role, role),
                            (Role::Input, Role::Leaf) | (Role::Leaf, Role::Input)
                        )),
                EvidenceConflict,
            )?;
        } else {
            ensure(
                (self.refs.len() as u64) < self.limits.references,
                FinalizationLimit,
            )?;
            self.total = self
                .total
                .checked_add(reference.size_bytes)
                .ok_or(EvidenceConflict)?;
            ensure(
                self.total <= self.limits.verified_content_bytes,
                FinalizationLimit,
            )?;
            self.refs
                .insert(reference.path.clone(), (reference.clone(), role));
        }
        let bytes = self
            .source
            .read(reference, self.limits.content_bytes.min(bound))
            .map_err(|_| IoUncertain)?;
        exact(reference, &bytes, bound)?;
        Ok(bytes)
    }
    fn leaf(&mut self, reference: &ContentRef) -> Check<()> {
        self.load(reference, Role::Leaf, self.limits.content_bytes)
            .map(|_| ())
    }
    fn memory(&mut self, memory: &MemoryRef, evidence: &Option<ContentRef>) -> Check<()> {
        if let MemoryRef::Available { content } = memory {
            self.leaf(content)?;
        }
        if let Some(content) = evidence {
            self.leaf(content)?;
        }
        Ok(())
    }
    fn checkpoint(
        &mut self,
        checkpoint: &CheckpointRef,
        run: &playtime_core::RunId,
        observations: &BTreeMap<u64, ObservationLocation>,
    ) -> Check<()> {
        self.leaf(&checkpoint.workspace)?;
        self.memory(&checkpoint.memento, &checkpoint.evidence)?;
        if let Some(reference) = &checkpoint.index {
            let bytes = self.load(reference, Role::Checkpoint, self.limits.manifest_bytes)?;
            let index = decode_checkpoint_index(utf8(&bytes)?).map_err(|_| EvidenceConflict)?;
            ensure(
                index.run_id == *run
                    && index.session_id == checkpoint.session_id
                    && index.workspace == checkpoint.workspace
                    && index.outgoing_memory.memory_ref() == checkpoint.memento
                    && index.outgoing_evidence.as_ref().map(|e| &e.text)
                        == checkpoint.evidence.as_ref(),
                EvidenceConflict,
            )?;
            ensure(
                index.files.len() as u64 <= self.limits.references
                    && index.changes.len() as u64 <= self.limits.references,
                FinalizationLimit,
            )?;
            let bytes = self.load(
                &index.source_map,
                Role::SourceMap,
                self.limits.manifest_bytes,
            )?;
            let map = decode_source_map(utf8(&bytes)?).map_err(|_| EvidenceConflict)?;
            ensure(map.run_id == *run, EvidenceConflict)?;
            ensure(
                map.entries.len() as u64 <= self.limits.events,
                FinalizationLimit,
            )?;
            for source in &map.entries {
                ensure(
                    observations.get(&source.controller_sequence.get()) == Some(source),
                    EvidenceConflict,
                )?;
            }
            for leaf in index.content_leaves() {
                self.leaf(leaf)?;
            }
        }
        Ok(())
    }
}

struct Intent {
    fact: EffectIntent,
    source: SourceEventV1,
    settled: bool,
    released: bool,
    generic_release: bool,
}
struct Accounting {
    effects: BTreeMap<u64, Intent>,
    requests: BTreeMap<RequestId, u64>,
    settlements: BTreeSet<RequestId>,
    usage: UsageTotals,
}
impl Accounting {
    fn new() -> Self {
        Self {
            effects: BTreeMap::new(),
            requests: BTreeMap::new(),
            settlements: BTreeSet::new(),
            usage: UsageTotals::default(),
        }
    }
    fn control(&mut self, event: &BridgeEvent, source: &SourceEventV1) -> Check<()> {
        match event {
            BridgeEvent::EffectIntent(fact) => {
                ensure(
                    EFFECT_KINDS.contains(&fact.effect_kind.as_str()),
                    UnsupportedContract,
                )?;
                ensure(
                    !self.effects.contains_key(&fact.effect_id.get()),
                    EvidenceConflict,
                )?;
                if fact.effect_kind == "model" {
                    let request = fact.request_id.as_ref().ok_or(EvidenceConflict)?;
                    ensure(
                        fact.session_id.is_some()
                            && fact.maximum_tokens.is_some()
                            && fact.tool_request_sequence.is_none(),
                        EvidenceConflict,
                    )?;
                    ensure(
                        self.requests
                            .insert(request.clone(), fact.effect_id.get())
                            .is_none(),
                        EvidenceConflict,
                    )?;
                }
                self.effects.insert(
                    fact.effect_id.get(),
                    Intent {
                        fact: fact.clone(),
                        source: source.clone(),
                        settled: false,
                        released: false,
                        generic_release: false,
                    },
                );
            }
            BridgeEvent::RequestNotDispatched(fact) => {
                let intent = self
                    .effects
                    .get_mut(&fact.effect_id.get())
                    .ok_or(EvidenceConflict)?;
                ensure(
                    intent.fact.effect_kind == "model"
                        && intent.fact.session_id.as_ref() == Some(&fact.session_id)
                        && intent.fact.request_id.as_ref() == Some(&fact.request_id)
                        && intent.fact.maximum_tokens == Some(fact.released_tokens)
                        && !intent.settled
                        && !intent.released,
                    EvidenceConflict,
                )?;
                intent.released = true;
            }
            BridgeEvent::EffectNotDispatched(fact) => {
                let intent = self
                    .effects
                    .get_mut(&fact.effect_id.get())
                    .ok_or(EvidenceConflict)?;
                ensure(
                    fact.session_id == intent.fact.session_id
                        && fact.identity.effect_id == Some(intent.fact.effect_id)
                        && fact.identity.request_id == intent.fact.request_id
                        && fact.identity.tool_request_sequence == intent.fact.tool_request_sequence
                        && !intent.generic_release
                        && !intent.settled,
                    EvidenceConflict,
                )?;
                intent.generic_release = true;
            }
            _ => (),
        }
        Ok(())
    }
    fn settle(
        &mut self,
        session: &playtime_core::SessionId,
        request: &RequestId,
        usage: GenerationUsage,
        strict: bool,
        recovery: bool,
    ) -> Check<()> {
        ensure(self.settlements.insert(request.clone()), EvidenceConflict)?;
        if strict {
            let effect = self.requests.get(request).ok_or(EvidenceConflict)?;
            let intent = self.effects.get_mut(effect).ok_or(EvidenceConflict)?;
            ensure(
                intent.fact.session_id.as_ref() == Some(session)
                    && !intent.settled
                    && !intent.released
                    && (!intent.generic_release || recovery),
                EvidenceConflict,
            )?;
            if let GenerationUsage::Unknown { reserved_tokens } = usage {
                ensure(
                    intent.fact.maximum_tokens == Some(reserved_tokens),
                    EvidenceConflict,
                )?;
            }
            intent.settled = true;
        }
        self.usage.record(usage).map_err(|_| EvidenceConflict)
    }
    fn unresolved(&self) -> Vec<UnresolvedIntentV1> {
        let mut unresolved: Vec<_> = self
            .effects
            .values()
            .filter(|i| i.fact.effect_kind == "model" && !i.settled && !i.released)
            .map(|i| UnresolvedIntentV1 {
                source: i.source.clone(),
                effect_id: i.fact.effect_id,
                session_id: i.fact.session_id.clone().expect("validated model intent"),
                request_id: i.fact.request_id.clone().expect("validated model intent"),
                maximum_tokens: i.fact.maximum_tokens.expect("validated allowance"),
            })
            .collect();
        unresolved.sort_by_key(|i| i.source.archive_sequence);
        unresolved
    }
}

fn plan(
    input: &PlanningInput<'_>,
    events: impl Iterator<Item = playtime_storage::Result<StoredEvent>>,
    verified: &mut Option<EventFrontier>,
) -> Check<RecoveryDecision> {
    input.attempt.validate().map_err(|_| EvidenceConflict)?;
    input.confirmed.validate().map_err(|_| EvidenceConflict)?;
    ensure(
        input.acquired_generation >= input.attempt.binding.superseding_generation.get(),
        RecoveryBlockedReasonV1::OwnershipUnresolved,
    )?;
    let base = input.base;
    let limits = input.limits;
    ensure(
        base.archive == input.attempt.binding.archive
            && base.committed_sequence <= limits.events
            && base.committed_bytes <= limits.event_log_bytes
            && input.event_file_bytes >= base.committed_bytes,
        EvidenceConflict,
    )?;
    exact(
        &base.archive.descriptor,
        input.descriptor_bytes,
        limits.manifest_bytes,
    )?;
    exact(
        &base.archive.input_manifest,
        input.manifest_bytes,
        limits.manifest_bytes,
    )?;
    exact(&base.marker, input.marker_bytes, 65_536)?;
    let descriptor =
        decode_archive_descriptor(utf8(input.descriptor_bytes)?).map_err(|_| EvidenceConflict)?;
    let manifest =
        decode_input_manifest(utf8(input.manifest_bytes)?).map_err(|_| EvidenceConflict)?;
    let marker = decode_commit_marker(utf8(input.marker_bytes)?).map_err(|_| EvidenceConflict)?;
    ensure(
        descriptor.run_id == base.archive.run_id
            && descriptor.input_manifest == base.archive.input_manifest
            && manifest.run_id == descriptor.run_id
            && manifest.worker.executable.sha256 == input.attempt.binding.worker.executable_sha256
            && marker.run_id == descriptor.run_id
            && marker.committed_sequence == base.committed_sequence
            && marker.committed_bytes == base.committed_bytes
            && marker.prefix_sha256 == base.prefix_sha256
            && base.marker.path.as_str() == "events.commit.json",
        EvidenceConflict,
    )?;
    let mut content = Contents {
        source: input.contents,
        limits,
        refs: BTreeMap::new(),
        total: 0,
    };
    // The manifest itself consumes a content/reference slot too.
    content.refs.insert(
        base.archive.input_manifest.path.clone(),
        (base.archive.input_manifest.clone(), Role::Input),
    );
    content.total = base.archive.input_manifest.size_bytes;
    ensure(
        content.total <= limits.verified_content_bytes && limits.references > 0,
        FinalizationLimit,
    )?;
    for reference in manifest.references() {
        content.load(reference, Role::Input, limits.content_bytes)?;
    }
    let contract_ref = manifest
        .files
        .iter()
        .find(|r| r.path.as_str() == EXECUTION_CONTRACT);
    let contract = match contract_ref {
        Some(reference) => {
            let bytes = content.load(reference, Role::Input, limits.manifest_bytes)?;
            playtime_protocol::recovery::decode::<ExecutionContractV1>(
                &bytes,
                limits.manifest_bytes,
            )
            .ok()
        }
        None => None,
    };
    let supported = if let Some(contract) = &contract {
        let bytes = content.load(&manifest.configuration, Role::Input, limits.content_bytes)?;
        let scenario = serde_json::from_slice::<playtime_engine::simulation::Scenario>(&bytes).ok();
        if scenario
            .as_ref()
            .is_some_and(|s| *contract == contract_for(&manifest, s, &contract.storage_limits))
        {
            let mut read_error = None;
            let result = verify_bundle_support(&manifest, |r| {
                content
                    .load(r, Role::Input, limits.manifest_bytes)
                    .map_err(|error| {
                        read_error = Some(error);
                        crate::Error::Invalid("capability read failed".into())
                    })
            });
            if let Some(error) = read_error {
                return Err(error);
            }
            result.is_ok()
        } else {
            false
        }
    } else {
        false
    };
    let frozen_limits = contract
        .as_ref()
        .filter(|_| supported)
        .map_or(limits, |c| &c.storage_limits);
    let effective = minimum_limits(limits, frozen_limits);
    content.limits = &effective;
    ensure(
        content.refs.len() as u64 <= effective.references
            && content.total <= effective.verified_content_bytes
            && content
                .refs
                .values()
                .all(|(r, _)| r.size_bytes <= effective.content_bytes)
            && base.archive.input_manifest.size_bytes <= effective.manifest_bytes
            && base.archive.descriptor.size_bytes <= effective.manifest_bytes
            && base.committed_bytes <= effective.event_log_bytes,
        FinalizationLimit,
    )?;
    let mut accounting = Accounting::new();
    let mut first_stop = None;
    let mut last_checkpoint = None;
    let mut terminal = false;
    let mut observations = BTreeMap::new();
    let mut hash = Sha256::new();
    let mut bytes = 0_u64;
    let mut sequence = 0_u64;
    let mut retained = 0_u64;
    let mut prefixes = BTreeMap::from([(0, (0, digest(&hash)))]);
    let mut recovery_patch: Option<(super::EncodedPatch, usize)> = None;
    let bounds: Vec<_> = input
        .confirmed
        .worker
        .iter()
        .chain(input.confirmed.public.iter().map(|p| &p.frontier))
        .collect();
    for bound in &bounds {
        ensure(
            bound.archive == base.archive
                && bound.committed_sequence <= base.committed_sequence
                && bound.committed_bytes <= base.committed_bytes,
            EvidenceConflict,
        )?;
    }
    check_bounds(&bounds, sequence, bytes, &hash)?;
    for item in events {
        let stored = item.map_err(|_| IoUncertain)?;
        ensure(!terminal, EvidenceConflict)?;
        ensure(
            sequence < limits.events.min(frozen_limits.events),
            FinalizationLimit,
        )?;
        ensure(
            stored.bytes.len() as u64 <= limits.record_bytes.min(frozen_limits.record_bytes),
            FinalizationLimit,
        )?;
        ensure(
            stored.bytes.last() == Some(&b'\n')
                && !stored.bytes[..stored.bytes.len().saturating_sub(1)].contains(&b'\n'),
            EvidenceConflict,
        )?;
        ensure(sha256(&stored.bytes) == stored.sha256, EvidenceConflict)?;
        let decoded = decode_event(utf8(&stored.bytes)?).map_err(|error| match error {
            playtime_protocol::ReadError::UnsupportedVersion { .. } => UnsupportedContract,
            _ => EvidenceConflict,
        })?;
        ensure(
            decoded.envelope() == &stored.envelope
                && stored.envelope.run_id == descriptor.run_id
                && sequence.checked_add(1) == Some(stored.envelope.sequence.get()),
            EvidenceConflict,
        )?;
        if supported {
            strict_control_fields(&decoded, &stored.bytes).map_err(|_| UnsupportedContract)?;
        }
        let origin = match decoded.payload().known() {
            Some(
                KnownEvent::RequestFinished {
                    recovery_origin, ..
                }
                | KnownEvent::RunFinished {
                    recovery_origin, ..
                },
            ) => recovery_origin.as_ref(),
            _ => None,
        };
        if let Some(origin) = origin {
            ensure(supported, UnsupportedContract)?;
            origin.validate().map_err(|_| EvidenceConflict)?;
            if recovery_patch.is_none() {
                let plan_bytes = content.load(
                    &origin.plan,
                    Role::RecoveryPlan,
                    input
                        .attempt
                        .maintenance_limits
                        .plan_bytes
                        .min(effective.manifest_bytes),
                )?;
                let old: RecoveryPlanV1 =
                    decode(&plan_bytes, effective.manifest_bytes).map_err(|_| EvidenceConflict)?;
                let cost = bytes.checked_add(retained).ok_or(EvidenceConflict)?;
                ensure(
                    old.attempt.attempt_id == origin.attempt_id
                        && old.archive == base.archive
                        && old.attempt.binding.worker == input.attempt.binding.worker
                        && old.attempt.binding.original_generation
                            == input.attempt.binding.original_generation
                        && old.base.committed_sequence == sequence
                        && old.base.committed_bytes == bytes
                        && old.base.prefix_sha256 == digest(&hash)
                        && Some(&old.execution_contract) == contract_ref
                        && old.existing_usage == UsageSummary::from(accounting.usage)
                        && old.unresolved == accounting.unresolved()
                        && old.first_stop == first_stop
                        && old.last_checkpoint == last_checkpoint
                        && old.base_accounted_log_bytes == cost
                        && cost.checked_add(old.available_log_bytes)
                            == Some(frozen_limits.event_log_bytes),
                    EvidenceConflict,
                )?;
                for bound in old
                    .prior_bounds
                    .worker
                    .iter()
                    .chain(old.prior_bounds.public.iter().map(|p| &p.frontier))
                {
                    ensure(
                        prefixes.get(&bound.committed_sequence)
                            == Some(&(bound.committed_bytes, bound.prefix_sha256.clone())),
                        EvidenceConflict,
                    )?;
                }
                if let Some(worker) = &input.confirmed.worker {
                    ensure(worker.committed_sequence <= sequence, EvidenceConflict)?;
                }
                if let Some(public) = &input.confirmed.public
                    && public.provenance == FrontierProvenanceV1::Worker
                {
                    ensure(
                        public.frontier.committed_sequence <= sequence,
                        EvidenceConflict,
                    )?;
                }
                let expected = super::encode_patch(&old).map_err(|_| FinalizationLimit)?;
                ensure(
                    expected.plan_bytes == plan_bytes
                        && &expected.origin == origin
                        && expected.bytes.len() as u64 <= effective.batch_bytes,
                    EvidenceConflict,
                )?;
                recovery_patch = Some((expected, 0));
            }
        }
        if let Some((patch, offset)) = &mut recovery_patch {
            ensure(origin == Some(&patch.origin), EvidenceConflict)?;
            let end = offset
                .checked_add(stored.bytes.len())
                .ok_or(EvidenceConflict)?;
            ensure(
                patch.bytes.get(*offset..end) == Some(stored.bytes.as_slice()),
                EvidenceConflict,
            )?;
            *offset = end;
        }
        let source = SourceEventV1 {
            archive_sequence: stored.envelope.sequence,
            record_sha256: stored.sha256.clone(),
        };
        if sequence == 0 {
            ensure(
                matches!(
                    decoded.payload().known(),
                    Some(KnownEvent::RunStarted { .. })
                ),
                EvidenceConflict,
            )?;
        }
        match decoded.payload() {
            EventPayload::Control(control) => {
                if supported {
                    accounting.control(control, &source)?;
                }
                if let BridgeEvent::EffectIntent(intent) = control.as_ref() {
                    ensure(
                        intent.sources.len() as u64 <= limits.references,
                        FinalizationLimit,
                    )?;
                    for source in &intent.sources {
                        ensure(
                            observations.get(&source.controller_sequence.get()) == Some(source),
                            EvidenceConflict,
                        )?;
                    }
                }
                if let BridgeEvent::StopDecided(stop) = control.as_ref() {
                    ensure(first_stop.is_none(), EvidenceConflict)?;
                    first_stop = Some(FirstStopV1 {
                        source: source.clone(),
                        reason: stop.reason,
                    });
                }
            }
            EventPayload::Observation(observation) => {
                let observed = observation.observation();
                ensure(
                    observations
                        .last_key_value()
                        .is_none_or(|(previous, _)| *previous < observed.controller_sequence.get()),
                    EvidenceConflict,
                )?;
                if let ObservationData::Artifact { content: reference } = &observed.data {
                    retained = retained
                        .checked_add(reference.size_bytes)
                        .ok_or(EvidenceConflict)?;
                    let payload =
                        content.load(reference, Role::Observation, limits.content_bytes)?;
                    observation
                        .as_ref()
                        .clone()
                        .resolve(&payload)
                        .map_err(|_| EvidenceConflict)?;
                }
                observations.insert(
                    observed.controller_sequence.get(),
                    ObservationLocation {
                        controller_sequence: observed.controller_sequence,
                        archive_sequence: stored.envelope.sequence,
                        record_sha256: stored.sha256.clone(),
                    },
                );
            }
            EventPayload::Unknown { .. } if supported => return Err(UnsupportedContract),
            _ => (),
        }
        match decoded.payload().known() {
            Some(KnownEvent::RunStarted {
                mode,
                input_manifest,
            }) => ensure(
                sequence == 0
                    && *mode == descriptor.mode
                    && *input_manifest == descriptor.input_manifest,
                EvidenceConflict,
            )?,
            Some(KnownEvent::SessionStarted {
                workspace,
                memento,
                evidence,
                delivery,
                ..
            }) => {
                content.leaf(workspace)?;
                content.memory(memento, evidence)?;
                if let Some(delivery) = delivery {
                    content.leaf(&delivery.memento.content)?;
                    if let Some(evidence) = &delivery.evidence {
                        content.leaf(&evidence.text)?;
                        content.leaf(&evidence.provenance)?;
                    }
                }
            }
            Some(KnownEvent::RequestFinished {
                session_id,
                request_id,
                usage,
                recovery_origin,
            }) => {
                accounting.settle(
                    session_id,
                    request_id,
                    *usage,
                    supported,
                    recovery_origin.is_some(),
                )?;
            }
            Some(KnownEvent::CheckpointPublished { checkpoint }) => {
                content.checkpoint(checkpoint, &descriptor.run_id, &observations)?;
                last_checkpoint = Some(LastCheckpointV1 {
                    source: source.clone(),
                    checkpoint: checkpoint.clone(),
                });
            }
            Some(KnownEvent::RunFinished {
                reason,
                usage,
                last_checkpoint: checkpoint,
                ..
            }) => {
                ensure(
                    *usage == UsageSummary::from(accounting.usage)
                        && checkpoint.as_ref() == last_checkpoint.as_ref().map(|c| &c.checkpoint),
                    EvidenceConflict,
                )?;
                if supported {
                    ensure(
                        accounting.unresolved().is_empty()
                            && first_stop
                                .as_ref()
                                .is_none_or(|stop| stop.reason == *reason),
                        EvidenceConflict,
                    )?;
                }
                terminal = true;
            }
            _ => (),
        }
        bytes = bytes
            .checked_add(stored.bytes.len() as u64)
            .ok_or(EvidenceConflict)?;
        ensure(bytes <= base.committed_bytes, EvidenceConflict)?;
        hash.update(&stored.bytes);
        sequence += 1;
        prefixes.insert(sequence, (bytes, digest(&hash)));
        check_bounds(&bounds, sequence, bytes, &hash)?;
    }
    ensure(
        sequence == base.committed_sequence
            && bytes == base.committed_bytes
            && digest(&hash) == base.prefix_sha256,
        EvidenceConflict,
    )?;
    if let Some((patch, offset)) = &recovery_patch {
        ensure(*offset == patch.bytes.len() && terminal, EvidenceConflict)?;
    }
    if let Some(PublishedFrontierV1 {
        provenance: FrontierProvenanceV1::Recovery { attempt_id },
        frontier,
    }) = &input.confirmed.public
    {
        ensure(
            recovery_patch
                .as_ref()
                .is_some_and(|(p, _)| &p.origin.attempt_id == attempt_id)
                && frontier == base,
            EvidenceConflict,
        )?;
    }
    *verified = Some(base.clone());
    if terminal {
        return Ok(RecoveryDecision::PreserveTerminal(Box::new(base.clone())));
    }
    if sequence == 0 {
        return if input.event_file_bytes == 0
            && input.begin_history == BeginHistory::ProvenNeverAuthorized
        {
            Ok(RecoveryDecision::NeverStarted)
        } else {
            Err(Incomplete)
        };
    }
    ensure(supported, UnsupportedContract)?;
    let contract = contract.as_ref().ok_or(UnsupportedContract)?;
    let maintenance = &input.attempt.maintenance_limits;
    ensure(
        input.inventory_entries <= maintenance.inventory_entries,
        PreservationLimit,
    )?;
    ensure(
        input.initial_inventory.path.as_str()
            == private_path(&input.attempt.attempt_id, "diagnostics.json"),
        EvidenceConflict,
    )?;
    exact(
        input.initial_inventory,
        input.inventory_bytes,
        maintenance.inventory_bytes,
    )
    .map_err(|e| {
        if e == FinalizationLimit {
            PreservationLimit
        } else {
            e
        }
    })?;
    let existing_usage = UsageSummary::from(accounting.usage);
    let unresolved = accounting.unresolved();
    for intent in &unresolved {
        accounting
            .usage
            .record(GenerationUsage::Unknown {
                reserved_tokens: intent.maximum_tokens,
            })
            .map_err(|_| EvidenceConflict)?;
    }
    let base_cost = log_cost(bytes, [retained])?;
    let available = contract
        .log_bytes
        .get()
        .checked_sub(base_cost)
        .ok_or(FinalizationLimit)?;
    let plan = RecoveryPlanV1 {
        schema_version: 1,
        attempt: input.attempt.clone(),
        archive: base.archive.clone(),
        base: base.clone(),
        prior_bounds: input.confirmed.clone(),
        execution_contract: contract_ref.ok_or(UnsupportedContract)?.clone(),
        initial_inventory: input.initial_inventory.clone(),
        unresolved,
        first_stop,
        last_checkpoint,
        existing_usage,
        resulting_usage: accounting.usage.into(),
        base_accounted_log_bytes: base_cost,
        available_log_bytes: available,
    };
    plan.validate().map_err(|_| EvidenceConflict)?;
    let patch = super::encode_patch(&plan).map_err(|_| FinalizationLimit)?;
    ensure(
        !content.refs.contains_key(&patch.origin.plan.path),
        EvidenceConflict,
    )?;
    ensure(
        log_cost(base_cost, [patch.bytes.len() as u64])? <= effective.event_log_bytes,
        FinalizationLimit,
    )?;
    ensure(
        patch.plan_bytes.len() as u64
            <= maintenance
                .plan_bytes
                .min(limits.manifest_bytes)
                .min(frozen_limits.manifest_bytes),
        FinalizationLimit,
    )?;
    ensure(
        patch.bytes.len() as u64
            <= available
                .min(limits.batch_bytes)
                .min(frozen_limits.batch_bytes),
        FinalizationLimit,
    )?;
    for line in patch.bytes.split_inclusive(|b| *b == b'\n') {
        ensure(
            line.len() as u64 <= limits.record_bytes.min(frozen_limits.record_bytes),
            FinalizationLimit,
        )?;
    }
    let event_count = sequence
        .checked_add(plan.unresolved.len() as u64)
        .and_then(|v| v.checked_add(1))
        .ok_or(EvidenceConflict)?;
    ensure(
        event_count <= limits.events.min(frozen_limits.events),
        FinalizationLimit,
    )?;
    let reference_count = (content.refs.len() as u64)
        .checked_add(1)
        .ok_or(EvidenceConflict)?;
    let referenced_bytes = content
        .total
        .checked_add(patch.plan_bytes.len() as u64)
        .ok_or(EvidenceConflict)?;
    ensure(
        reference_count <= limits.references.min(frozen_limits.references)
            && referenced_bytes
                <= limits
                    .verified_content_bytes
                    .min(frozen_limits.verified_content_bytes)
            && patch.plan_bytes.len() as u64
                <= limits.content_bytes.min(frozen_limits.content_bytes),
        FinalizationLimit,
    )?;
    hash.update(&patch.bytes);
    let (selection, marker_bytes) = super::selection_for_patch(
        &plan,
        &patch,
        digest(&hash),
        reference_count,
        referenced_bytes,
    )
    .map_err(|_| FinalizationLimit)?;
    let selection_bytes = serde_json::to_vec(&selection).map_err(|_| FinalizationLimit)?;
    ensure(
        selection_bytes.len() as u64 <= maintenance.selection_bytes,
        PreservationLimit,
    )?;
    let created_bytes = [
        input.inventory_bytes.len(),
        patch.plan_bytes.len(),
        patch.bytes.len(),
        marker_bytes.len(),
        selection_bytes.len(),
    ]
    .into_iter()
    .try_fold(0_u64, |n, v| n.checked_add(v as u64))
    .ok_or(EvidenceConflict)?;
    ensure(
        created_bytes <= maintenance.total_bytes - maintenance.resolution_reserve_bytes
            && 5 <= maintenance.entries - maintenance.resolution_reserve_entries,
        PreservationLimit,
    )?;
    Ok(RecoveryDecision::Finalize(Box::new(plan)))
}

fn log_cost(jsonl_bytes: u64, retained_occurrences: impl IntoIterator<Item = u64>) -> Check<u64> {
    retained_occurrences
        .into_iter()
        .try_fold(jsonl_bytes, |sum, size| {
            sum.checked_add(size).ok_or(EvidenceConflict)
        })
}

fn check_bounds(bounds: &[&EventFrontier], sequence: u64, bytes: u64, hash: &Sha256) -> Check<()> {
    for bound in bounds.iter().filter(|b| b.committed_sequence == sequence) {
        ensure(
            bound.committed_bytes == bytes && bound.prefix_sha256 == digest(hash),
            EvidenceConflict,
        )?;
    }
    Ok(())
}

fn minimum_limits(a: &StorageLimits, b: &StorageLimits) -> StorageLimits {
    StorageLimits {
        event_log_bytes: a.event_log_bytes.min(b.event_log_bytes),
        record_bytes: a.record_bytes.min(b.record_bytes),
        batch_bytes: a.batch_bytes.min(b.batch_bytes),
        content_bytes: a.content_bytes.min(b.content_bytes),
        manifest_bytes: a.manifest_bytes.min(b.manifest_bytes),
        verified_content_bytes: a.verified_content_bytes.min(b.verified_content_bytes),
        references: a.references.min(b.references),
        events: a.events.min(b.events),
        page_bytes: a.page_bytes.min(b.page_bytes),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_log_accounting_review_vectors() {
        let vectors: serde_json::Value = serde_json::from_str(include_str!(
            "../../../playtime-protocol/tests/fixtures/recovery/review-vectors.json"
        ))
        .unwrap();
        for vector in vectors["log_accounting"].as_array().unwrap() {
            let base = log_cost(
                vector["jsonl_bytes"].as_u64().unwrap(),
                vector["retained_payload_occurrences"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|v| v.as_u64().unwrap()),
            )
            .unwrap();
            let final_cost = log_cost(base, [vector["patch_bytes"].as_u64().unwrap()]).unwrap();
            assert_eq!(base, vector["expected_base_cost"].as_u64().unwrap());
            assert_eq!(final_cost, vector["expected_final_cost"].as_u64().unwrap());
            assert_eq!(
                final_cost <= vector["frozen_log_bytes"].as_u64().unwrap(),
                vector["expected_fits"].as_bool().unwrap()
            );
        }
        assert_eq!(log_cost(u64::MAX, [1]), Err(EvidenceConflict));
    }
}
