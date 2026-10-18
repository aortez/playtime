//! Controller observations for inspection. These are source records, not
//! execution receipts or a second controller policy. Canonical archive events
//! retain authority for accounting, checkpoints, and the terminal outcome.

use std::num::{NonZeroU32, NonZeroU64};

use playtime_core::{
    GenerationUsage, MemoryCondition, RequestId, RunId, RunMode, RunPhase, SessionId, Sha256Digest,
    StopReason,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    ReadError,
    archive::ContentRef,
    events::{CleanupOutcome, ContextStart},
    read::require_version,
    v1::UsageSummary,
};

pub const CONTROLLER_OBSERVATION_KIND: &str = "controller_observation";
pub const CONTROLLER_OBSERVATION_VERSION: u32 = 1;

/// The original controller record, including its independent sequence domain.
/// The decoder also retains the source JSON, including future optional fields.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ControllerRecord {
    pub sequence: NonZeroU64,
    pub elapsed_ms: u64,
    pub session_id: Option<SessionId>,
    pub kind: String,
    pub data: Value,
}

/// Associations captured by the driver. None means not recorded, never an
/// inferred identifier. A tool's stable identity is its original request record
/// sequence; a rejected tool may have no external effect at all.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ObservationIdentity {
    pub effect_id: Option<NonZeroU64>,
    pub request_id: Option<RequestId>,
    pub tool_request_sequence: Option<NonZeroU64>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ControllerObservation {
    pub schema_version: u32,
    pub record: ControllerRecord,
    pub identity: ObservationIdentity,
}

/// A derived index entry. The digest covers the exact archive JSONL record,
/// including its terminating LF; it cannot be embedded in the record it hashes.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ObservationLocation {
    pub controller_sequence: NonZeroU64,
    pub archive_sequence: NonZeroU64,
    pub record_sha256: Sha256Digest,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct RecordedModelRequest {
    pub request_id: RequestId,
    pub call: NonZeroU64,
    pub handoff: bool,
    pub sampling_seed: u32,
    pub maximum_tokens: NonZeroU64,
    pub messages: Vec<Value>,
    pub tools: Vec<Value>,
    pub context_input_bound: u64,
    pub context_basis: String,
    pub context_reserve: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct RecordedModelResponse {
    pub message: Value,
    pub done: bool,
    pub generated_tokens: Option<u64>,
    pub prompt_tokens: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RecordedFailure {
    /// Open vocabulary: a future failure kind remains inspectable.
    pub kind: String,
    pub message: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RecordedMemory {
    /// Preserve exact status and delivered text, including an available empty
    /// memento versus empty content caused by a missing/oversized file.
    pub status: String,
    pub content: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TextStatus {
    Partial,
    Complete,
    Truncated,
}

/// Complete denotes a recorded final field, not experimental success. Missing
/// thinking stays NotRecorded; it is never reconstructed from other fields.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RecordedText {
    NotRecorded,
    Recorded { text: String, status: TextStatus },
}

/// The reader supplies status from observed completion/limit events. This helper
/// never guesses completeness or reconstructs fields absent from the source.
pub fn recorded_text(
    message: &Value,
    field: &str,
    status: TextStatus,
) -> Result<RecordedText, ReadError> {
    if !message.is_object() {
        return Err(ReadError::malformed(
            "recorded model message must be an object",
        ));
    }
    match message.get(field) {
        None => Ok(RecordedText::NotRecorded),
        Some(Value::String(text)) => Ok(RecordedText::Recorded {
            text: text.clone(),
            status,
        }),
        Some(_) => Err(ReadError::malformed("recorded model text must be a string")),
    }
}

/// Typed source data for transcript readers. Tool and memento writes are
/// intents in controller revision 0d781da; neither proves external execution.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum KnownObservation {
    ControllerStarted {
        run_id: RunId,
        mode: RunMode,
        parameters: Value,
    },
    PhaseChanged {
        phase: RunPhase,
        session_id: Option<SessionId>,
    },
    MemoryReceived {
        ordinal: NonZeroU32,
        context: ContextStart,
        memory: RecordedMemory,
        evidence: Option<Value>,
        memory_condition: MemoryCondition,
    },
    ModelRequest(RecordedModelRequest),
    ModelChunk {
        call: NonZeroU64,
        chunk: Value,
    },
    ModelChunkRejected {
        bytes: u64,
        response_bytes: u64,
    },
    ModelResponse {
        call: NonZeroU64,
        response: RecordedModelResponse,
    },
    ModelError {
        error: RecordedFailure,
        recovery_allowed: bool,
        token_budget_charge: u64,
        consecutive_errors: u32,
        tool_parse_errors: u32,
        usage: UsageSummary,
    },
    ModelUsageOverflow {
        reported_generated_tokens: u64,
        call: NonZeroU64,
    },
    RequestSettled {
        call: NonZeroU64,
        usage: GenerationUsage,
        totals: UsageSummary,
    },
    RequestNotDispatched {
        call: NonZeroU64,
        released_tokens: NonZeroU64,
    },
    ToolRequest {
        name: String,
        arguments: Value,
    },
    #[serde(rename = "tool_dispatch")]
    ToolIntent {
        name: String,
        request_sequence: NonZeroU64,
    },
    #[serde(rename = "memento_write")]
    MementoWriteIntent {
        name: String,
        request_sequence: NonZeroU64,
    },
    ToolNotDispatched {
        request_sequence: NonZeroU64,
    },
    ToolResult {
        name: String,
        request_sequence: NonZeroU64,
        result: Value,
        unabridged_result: Option<Value>,
    },
    ControllerCheckpoint {
        ordinal: NonZeroU32,
        workspace: ContentRef,
        memory: RecordedMemory,
        evidence: Option<Value>,
        handoff_saved: bool,
        interrupted: bool,
    },
    ControllerStop {
        reason: StopReason,
        usage: UsageSummary,
    },
    ControllerFinished {
        run_id: RunId,
        mode: RunMode,
        reason: StopReason,
        usage: UsageSummary,
        cleanup: CleanupOutcome,
        diagnostics: Vec<String>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub enum ObservationPayload {
    Known(Box<KnownObservation>),
    Unknown { kind: String, data: Value },
}

#[derive(Clone, Debug, PartialEq)]
pub struct DecodedObservation {
    source: Value,
    observation: ControllerObservation,
    payload: ObservationPayload,
}

impl DecodedObservation {
    /// Complete original wrapper data, including unknown optional fields.
    pub fn source(&self) -> &Value {
        &self.source
    }
    pub fn observation(&self) -> &ControllerObservation {
        &self.observation
    }
    pub fn payload(&self) -> &ObservationPayload {
        &self.payload
    }
}

pub fn decode_observation(json: &str) -> Result<DecodedObservation, ReadError> {
    require_version(
        json,
        "controller observation",
        CONTROLLER_OBSERVATION_VERSION,
    )?;
    let source: Value = serde_json::from_str(json).map_err(ReadError::malformed)?;
    let observation: ControllerObservation =
        serde_json::from_value(source.clone()).map_err(ReadError::malformed)?;
    if observation.record.kind.is_empty() {
        return Err(ReadError::malformed(
            "controller record kind must not be empty",
        ));
    }
    let record = &observation.record;
    let payload = match record.kind.as_str() {
        "controller_started"
        | "phase_changed"
        | "memory_received"
        | "model_request"
        | "model_chunk"
        | "model_chunk_rejected"
        | "model_response"
        | "model_error"
        | "model_usage_overflow"
        | "request_settled"
        | "request_not_dispatched"
        | "tool_request"
        | "tool_dispatch"
        | "memento_write"
        | "tool_not_dispatched"
        | "tool_result"
        | "controller_checkpoint"
        | "controller_stop"
        | "controller_finished" => {
            let known: KnownObservation =
                serde_json::from_value(json!({"kind": record.kind, "data": record.data}))
                    .map_err(ReadError::malformed)?;
            validate(&observation, &known)?;
            ObservationPayload::Known(Box::new(known))
        }
        _ => ObservationPayload::Unknown {
            kind: record.kind.clone(),
            data: record.data.clone(),
        },
    };
    Ok(DecodedObservation {
        source,
        observation,
        payload,
    })
}

fn validate(
    observation: &ControllerObservation,
    known: &KnownObservation,
) -> Result<(), ReadError> {
    let identity = &observation.identity;
    if let KnownObservation::PhaseChanged { session_id, .. } = known
        && *session_id != observation.record.session_id
    {
        return Err(ReadError::malformed(
            "phase session disagrees with source record",
        ));
    }
    match known {
        KnownObservation::ModelRequest(request) => {
            if identity.request_id.as_ref() != Some(&request.request_id) {
                return Err(ReadError::malformed(
                    "model request identity disagrees with source",
                ));
            }
        }
        KnownObservation::ToolRequest { .. } => {
            if identity.tool_request_sequence != Some(observation.record.sequence) {
                return Err(ReadError::malformed(
                    "tool identity must name its original request record",
                ));
            }
        }
        KnownObservation::ToolIntent {
            request_sequence, ..
        }
        | KnownObservation::MementoWriteIntent {
            request_sequence, ..
        }
        | KnownObservation::ToolNotDispatched { request_sequence }
        | KnownObservation::ToolResult {
            request_sequence, ..
        } => {
            if identity.tool_request_sequence != Some(*request_sequence)
                || *request_sequence >= observation.record.sequence
            {
                return Err(ReadError::malformed(
                    "tool association must name an earlier source request",
                ));
            }
        }
        _ => {}
    }
    if matches!(
        known,
        KnownObservation::ToolRequest { .. }
            | KnownObservation::ToolIntent { .. }
            | KnownObservation::MementoWriteIntent { .. }
            | KnownObservation::ToolNotDispatched { .. }
            | KnownObservation::ToolResult { .. }
    ) && (identity.request_id.is_none() || observation.record.session_id.is_none())
    {
        return Err(ReadError::malformed(
            "tool observations require session and originating model request identity",
        ));
    }
    if matches!(
        known,
        KnownObservation::ToolIntent { .. } | KnownObservation::MementoWriteIntent { .. }
    ) && identity.effect_id.is_none()
    {
        return Err(ReadError::malformed(
            "external tool intent requires effect identity",
        ));
    }
    match known {
        KnownObservation::ModelRequest(_)
        | KnownObservation::ModelChunk { .. }
        | KnownObservation::ModelChunkRejected { .. }
        | KnownObservation::ModelResponse { .. }
        | KnownObservation::ModelError { .. }
        | KnownObservation::ModelUsageOverflow { .. }
        | KnownObservation::RequestSettled { .. }
        | KnownObservation::RequestNotDispatched { .. } => {
            if identity.request_id.is_none()
                || identity.effect_id.is_none()
                || observation.record.session_id.is_none()
            {
                return Err(ReadError::malformed(
                    "model observations require session, request, and effect identity",
                ));
            }
        }
        _ => {}
    }
    if let KnownObservation::ControllerFinished {
        cleanup: CleanupOutcome::Failed { errors },
        ..
    } = known
        && errors.is_empty()
    {
        return Err(ReadError::malformed("failed cleanup must include errors"));
    }
    match known {
        KnownObservation::ModelResponse { response, .. } => validate_text(&response.message)?,
        KnownObservation::RequestSettled { totals, .. } => {
            totals.validate().map_err(ReadError::malformed)?
        }
        KnownObservation::ModelError { usage, .. }
        | KnownObservation::ControllerStop { usage, .. }
        | KnownObservation::ControllerFinished { usage, .. } => {
            usage.validate().map_err(ReadError::malformed)?
        }
        _ => {}
    }
    Ok(())
}

fn validate_text(message: &Value) -> Result<(), ReadError> {
    recorded_text(message, "content", TextStatus::Partial)?;
    recorded_text(message, "thinking", TextStatus::Partial)?;
    Ok(())
}
