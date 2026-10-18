//! Bounded, read-only recovery proposals. No mutation, execution or takeover.
mod contract;
mod patch;
mod planner;
pub(crate) use contract::verify_worker_contract;
pub use contract::{contract_for, verify_bundle_support};
pub use patch::{EncodedPatch, encode_patch, selection_for_patch};
pub use planner::{
    BeginHistory, PlanningContents, PlanningInput, RecoveryDecision, VerifiedEvidence,
    plan_recovery,
};
mod reentry;
pub use reentry::{ReentryDecision, SelectedFiles, inspect_reentry};
