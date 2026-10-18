use crate::{Error, Result, prepare::reference};
use playtime_core::{GenerationUsage, Sha256Digest, StopReason};
use playtime_protocol::{
    events::{CleanupOutcome, EventEnvelope, KnownEvent},
    recovery::*,
    storage::{CommitMarker, EventFrontier},
};
use std::num::NonZeroU64;

/// Exact proposed bytes in memory. This is not a storage receipt or write handle.
#[derive(Clone, Debug)]
pub struct EncodedPatch {
    pub plan_bytes: Vec<u8>,
    pub origin: RecoveryOriginV1,
    pub bytes: Vec<u8>,
}
/// Deterministic: only the persisted attempt supplies time/identity. The plan
/// cannot depend on this patch's digest or encoded size.
pub fn encode_patch(plan: &RecoveryPlanV1) -> Result<EncodedPatch> {
    plan.validate()?;
    let plan_bytes = serde_json::to_vec(plan)?;
    if plan_bytes.len() as u64 > plan.attempt.maintenance_limits.plan_bytes {
        return Err(Error::Capacity(
            "recovery plan exceeds maintenance bound".into(),
        ));
    }
    let origin = RecoveryOriginV1 {
        schema_version: 1,
        attempt_id: plan.attempt.attempt_id.clone(),
        plan: reference(&plan_path(&plan.attempt.attempt_id), &plan_bytes),
    };
    let mut bytes = Vec::new();
    let mut sequence = plan.base.committed_sequence;
    let mut append = |event: KnownEvent| -> Result<()> {
        sequence = sequence
            .checked_add(1)
            .ok_or_else(|| Error::Invalid("recovery sequence overflow".into()))?;
        let tagged = serde_json::to_value(event)?;
        let envelope = EventEnvelope {
            schema_version: 1,
            run_id: plan.archive.run_id.clone(),
            sequence: NonZeroU64::new(sequence).unwrap(),
            time: plan.attempt.chosen_time.clone(),
            kind: tagged["kind"].as_str().expect("tagged known event").into(),
            data: tagged["data"].clone(),
        };
        let line = serde_json::to_vec(&envelope)?;
        let size = (bytes.len() as u64)
            .checked_add(line.len() as u64)
            .and_then(|n| n.checked_add(1))
            .ok_or_else(|| Error::Capacity("recovery patch length overflow".into()))?;
        if size
            > plan
                .available_log_bytes
                .min(playtime_storage::StorageLimits::default().batch_bytes)
        {
            return Err(Error::Capacity(
                "recovery patch exceeds frozen capacity/one-batch bound".into(),
            ));
        }
        bytes.extend(line);
        bytes.push(b'\n');
        Ok(())
    };
    for intent in &plan.unresolved {
        append(KnownEvent::RequestFinished {
            session_id: intent.session_id.clone(),
            request_id: intent.request_id.clone(),
            usage: GenerationUsage::Unknown {
                reserved_tokens: intent.maximum_tokens,
            },
            recovery_origin: Some(origin.clone()),
        })?;
    }
    append(KnownEvent::RunFinished {
        reason: plan
            .first_stop
            .as_ref()
            .map_or(StopReason::Interrupted, |s| s.reason),
        usage: plan.resulting_usage.clone(),
        cleanup: CleanupOutcome::Unknown,
        last_checkpoint: plan.last_checkpoint.as_ref().map(|c| c.checkpoint.clone()),
        recovery_origin: Some(origin.clone()),
    })?;
    Ok(EncodedPatch {
        plan_bytes,
        origin,
        bytes,
    })
}

/// Read-only encoding preview. The supplied digest is computed by the planner's
/// full prefix scan plus these patch bytes. R2 must independently revalidate it.
pub fn selection_for_patch(
    plan: &RecoveryPlanV1,
    patch: &EncodedPatch,
    target_prefix: Sha256Digest,
    reference_count: u64,
    referenced_content_bytes: u64,
) -> Result<(RecoverySelectionV1, Vec<u8>)> {
    let expected = encode_patch(plan)?;
    if patch.plan_bytes != expected.plan_bytes
        || patch.origin != expected.origin
        || patch.bytes != expected.bytes
    {
        return Err(Error::Invalid(
            "selection must name the exact deterministic plan and patch".into(),
        ));
    }
    let add = |a: u64, b: u64| {
        a.checked_add(b)
            .ok_or_else(|| Error::Capacity("recovery capacity overflow".into()))
    };
    let sequence = add(
        plan.base.committed_sequence,
        add(plan.unresolved.len() as u64, 1)?,
    )?;
    let bytes = add(plan.base.committed_bytes, patch.bytes.len() as u64)?;
    let marker = CommitMarker {
        schema_version: 1,
        run_id: plan.archive.run_id.clone(),
        committed_sequence: sequence,
        committed_bytes: bytes,
        prefix_sha256: target_prefix.clone(),
    };
    let marker_bytes = serde_json::to_vec(&marker)?;
    let final_cost = add(plan.base_accounted_log_bytes, patch.bytes.len() as u64)?;
    let frozen = add(plan.base_accounted_log_bytes, plan.available_log_bytes)?;
    let remaining = frozen
        .checked_sub(final_cost)
        .ok_or_else(|| Error::Capacity("patch exceeds frozen allocation".into()))?;
    let selection = RecoverySelectionV1 {
        schema_version: 1,
        attempt_id: plan.attempt.attempt_id.clone(),
        archive: plan.archive.clone(),
        base: plan.base.clone(),
        plan: patch.origin.plan.clone(),
        inventory: plan.initial_inventory.clone(),
        patch: reference(
            &private_path(&plan.attempt.attempt_id, "patch.jsonl"),
            &patch.bytes,
        ),
        target_marker: reference(
            &private_path(&plan.attempt.attempt_id, "marker.json"),
            &marker_bytes,
        ),
        final_frontier: EventFrontier {
            archive: plan.archive.clone(),
            marker: reference("events.commit.json", &marker_bytes),
            committed_sequence: sequence,
            committed_bytes: bytes,
            prefix_sha256: target_prefix,
        },
        capacity: FinalizationCapacityV1 {
            base_accounted_log_bytes: plan.base_accounted_log_bytes,
            patch_bytes: patch.bytes.len() as u64,
            final_accounted_log_bytes: final_cost,
            frozen_log_bytes: frozen,
            remaining_log_bytes: remaining,
            event_count: sequence,
            reference_count,
            referenced_content_bytes,
        },
    };
    selection.validate()?;
    Ok((selection, marker_bytes))
}
