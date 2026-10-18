//! Read-only interpretation of an already selected transaction. This neither
//! retries nor adopts it. R2 must preserve evidence and validate held ownership,
//! the complete prefix and all selected files before performing either action.
use playtime_protocol::{
    recovery::*,
    storage::{EventFrontier, decode_commit_marker},
};
use playtime_storage::content_ref;

pub struct SelectedFiles<'a> {
    pub selection: &'a [u8],
    pub plan: &'a [u8],
    pub inventory: &'a [u8],
    pub patch: &'a [u8],
    pub target_marker: &'a [u8],
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReentryDecision {
    NoSelection,
    PreserveSelectedPatch,
    VerifySelectedTerminal,
    Blocked(RecoveryBlockedReasonV1),
}

/// `from_base` is the observed event-file tail starting at the selected base,
/// including committed patch bytes when the marker is the target. No clock,
/// new attempt, writer, truncation or ledger operation is involved.
pub fn inspect_reentry(
    files: Option<SelectedFiles<'_>>,
    marker: &EventFrontier,
    from_base: &[u8],
    destructive_work_possible: bool,
) -> ReentryDecision {
    let Some(files) = files else {
        return if destructive_work_possible {
            ReentryDecision::Blocked(RecoveryBlockedReasonV1::EvidenceConflict)
        } else {
            ReentryDecision::NoSelection
        };
    };
    inspect(&files, marker, from_base).unwrap_or_else(ReentryDecision::Blocked)
}
fn inspect(
    files: &SelectedFiles<'_>,
    marker: &EventFrontier,
    from_base: &[u8],
) -> Result<ReentryDecision, RecoveryBlockedReasonV1> {
    use RecoveryBlockedReasonV1::EvidenceConflict;
    let selection: RecoverySelectionV1 =
        decode(files.selection, 65_536).map_err(|_| EvidenceConflict)?;
    let plan: RecoveryPlanV1 = decode(files.plan, MaintenanceLimitsV1::default().plan_bytes)
        .map_err(|_| EvidenceConflict)?;
    let limits = &plan.attempt.maintenance_limits;
    if files.inventory.len() as u64 > limits.inventory_bytes
        || files.selection.len() as u64 > limits.selection_bytes
        || files.plan.len() as u64 > limits.plan_bytes
        || files.target_marker.len() > 65_536
        || files.patch.len() as u64
            > playtime_protocol::storage::StorageLimits::default().batch_bytes
        || selection.attempt_id != plan.attempt.attempt_id
        || selection.archive != plan.archive
        || selection.base != plan.base
        || selection.inventory != plan.initial_inventory
    {
        return Err(EvidenceConflict);
    }
    for (reference, bytes) in [
        (&selection.plan, files.plan),
        (&selection.inventory, files.inventory),
        (&selection.patch, files.patch),
        (&selection.target_marker, files.target_marker),
    ] {
        if content_ref(reference.path.clone(), bytes) != *reference {
            return Err(EvidenceConflict);
        }
    }
    let patch = super::encode_patch(&plan).map_err(|_| EvidenceConflict)?;
    if patch.bytes != files.patch || patch.plan_bytes != files.plan {
        return Err(EvidenceConflict);
    }
    let target = decode_commit_marker(
        std::str::from_utf8(files.target_marker).map_err(|_| EvidenceConflict)?,
    )
    .map_err(|_| EvidenceConflict)?;
    let (expected, target_bytes) = super::selection_for_patch(
        &plan,
        &patch,
        target.prefix_sha256,
        selection.capacity.reference_count,
        selection.capacity.referenced_content_bytes,
    )
    .map_err(|_| EvidenceConflict)?;
    if selection != expected || files.target_marker != target_bytes {
        return Err(EvidenceConflict);
    }
    if marker == &selection.base && files.patch.starts_with(from_base) {
        Ok(ReentryDecision::PreserveSelectedPatch)
    } else if marker == &selection.final_frontier && from_base == files.patch {
        Ok(ReentryDecision::VerifySelectedTerminal)
    } else {
        Err(EvidenceConflict)
    }
}
