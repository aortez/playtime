//! Lossless controller facts, independent of the raw observation byte budget.

use playtime_core::{
    CommandOutcome, GenerationUsage, RequestId, RequestPurpose, RequestScope, RunPhase, SessionId,
    StopReason,
};
use playtime_protocol::{archive::ContentRef, observations::ObservationIdentity, v1::UsageSummary};

use crate::{Memory, Record, evidence::Evidence};

pub const CONTROL_QUEUE_CAPACITY: usize = 128;
// A transition may reject all eight calls and emit their result/omission facts,
// then stop and arrange cleanup. Leave room for that whole atomic decision.
pub(crate) const CONTROL_TRANSITION_RESERVE: usize = 64;

#[derive(Clone, Debug, PartialEq)]
pub struct BoundRecord {
    pub record: Record,
    pub identity: ObservationIdentity,
    pub request: Option<RequestContext>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EffectInfo {
    pub id: u64,
    pub kind: &'static str,
    pub session_id: Option<SessionId>,
    pub identity: ObservationIdentity,
    pub maximum_tokens: Option<u64>,
    pub deadline_ms: u64,
    pub request: Option<RequestContext>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RequestContext {
    pub effect_id: u64,
    pub scope: RequestScope,
    pub purpose: RequestPurpose,
    pub call: u64,
    pub request_id: RequestId,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ControlUpdate {
    pub elapsed_ms: u64,
    pub session_id: Option<SessionId>,
    pub fact: ControlFact,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ControlFact {
    WorkspaceStarted {
        workspace: ContentRef,
    },
    CaptureCommitted {
        effect_id: u64,
    },
    PhaseChanged(RunPhase),
    EffectPending(EffectInfo),
    EffectNotDispatched(EffectInfo),
    RequestSettled {
        request: RequestContext,
        usage: GenerationUsage,
        totals: UsageSummary,
    },
    RequestNotDispatched {
        request: RequestContext,
        released_tokens: u64,
    },
    SessionStarted {
        ordinal: u32,
        workspace: ContentRef,
        memory: Memory,
        evidence: Option<Evidence>,
    },
    SessionClosed {
        finish_run: bool,
        handoff_saved: bool,
    },
    ExecResult {
        request: RequestContext,
        effect_id: u64,
        tool_request_sequence: u64,
        outcome: CommandOutcome,
    },
    CheckpointCommitted {
        effect_id: u64,
        session_id: SessionId,
    },
    StopDecided(StopReason),
    /// Read result() after all publication operations have settled; a late
    /// checkpoint acknowledgment can update it without changing its stop reason.
    Finished,
    ObservationOmitted {
        kind: String,
        size_bytes: u64,
        identity: ObservationIdentity,
        request: Option<RequestContext>,
    },
}
