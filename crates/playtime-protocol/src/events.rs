use std::{
    collections::BTreeMap,
    num::{NonZeroU32, NonZeroU64},
};

use playtime_core::{
    CommandOutcome, GenerationUsage, RequestId, RunId, RunMode, RunPhase, SessionId, StopReason,
    UtcTime,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use utoipa::ToSchema;

use crate::{
    ReadError,
    archive::ContentRef,
    observations::{
        CONTROLLER_OBSERVATION_KIND, DecodedObservation, KnownObservation, ObservationPayload,
        decode_observation,
    },
    read::require_version,
    v1::UsageSummary,
};

pub const EVENT_SCHEMA_VERSION: u32 = 1;

/// Raw wire data. Use decode_event() before interpreting its version or payload.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, ToSchema)]
pub struct EventEnvelope {
    pub schema_version: u32,
    #[schema(value_type = String, pattern = "^[A-Za-z0-9][A-Za-z0-9_-]{0,127}$")]
    pub run_id: RunId,
    #[schema(value_type = u64, minimum = 1)]
    pub sequence: NonZeroU64,
    #[schema(value_type = String, format = DateTime)]
    pub time: UtcTime,
    pub kind: String,
    #[schema(schema_with = any_json_schema)]
    pub data: Value,
}

fn any_json_schema() -> utoipa::openapi::schema::Object {
    use utoipa::openapi::schema::{ObjectBuilder, SchemaType, Type};

    // An explicit union accepts arbitrary JSON and round-trips through utoipa
    // 6.0, whose Object reader rejects the typeless {} schema it generates.
    ObjectBuilder::new()
        .schema_type(SchemaType::Array(vec![
            Type::Object,
            Type::Array,
            Type::String,
            Type::Number,
            Type::Integer,
            Type::Boolean,
            Type::Null,
        ]))
        .build()
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextStart {
    Fresh,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum MemoryRef {
    Available { content: ContentRef },
    Missing,
    Unavailable { reason: String },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CheckpointRef {
    pub session_id: SessionId,
    pub workspace: ContentRef,
    pub memento: MemoryRef,
    pub evidence: Option<ContentRef>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CleanupOutcome {
    Complete,
    Failed { errors: Vec<String> },
    Unknown,
}

/// This vocabulary describes observations, not a second controller state machine.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum KnownEvent {
    RunStarted {
        mode: RunMode,
        input_manifest: ContentRef,
    },
    PhaseChanged {
        phase: RunPhase,
        session_id: Option<SessionId>,
    },
    SessionStarted {
        session_id: SessionId,
        ordinal: NonZeroU32,
        context: ContextStart,
        workspace: ContentRef,
        memento: MemoryRef,
        evidence: Option<ContentRef>,
    },
    RequestFinished {
        session_id: SessionId,
        request_id: RequestId,
        usage: GenerationUsage,
    },
    CommandFinished {
        session_id: SessionId,
        request_id: RequestId,
        outcome: CommandOutcome,
    },
    SessionEnded {
        session_id: SessionId,
        finish_run: bool,
        handoff_saved: bool,
    },
    CheckpointPublished {
        checkpoint: CheckpointRef,
    },
    RunFinished {
        reason: StopReason,
        usage: UsageSummary,
        cleanup: CleanupOutcome,
        last_checkpoint: Option<CheckpointRef>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub enum EventPayload {
    Known(Box<KnownEvent>),
    Observation(Box<DecodedObservation>),
    /// Unknown is visible data, never silently dropped or treated as executable.
    Unknown {
        kind: String,
        data: Value,
    },
}

impl EventPayload {
    pub fn known(&self) -> Option<&KnownEvent> {
        match self {
            Self::Known(event) => Some(event),
            Self::Observation(_) | Self::Unknown { .. } => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct DecodedEvent {
    envelope: EventEnvelope,
    payload: EventPayload,
}

impl DecodedEvent {
    /// Retains the original payload, including new optional fields on known kinds.
    pub fn envelope(&self) -> &EventEnvelope {
        &self.envelope
    }
    pub fn payload(&self) -> &EventPayload {
        &self.payload
    }
}

pub fn decode_event(json: &str) -> Result<DecodedEvent, ReadError> {
    require_version(json, "event", EVENT_SCHEMA_VERSION)?;
    let envelope: EventEnvelope = serde_json::from_str(json).map_err(ReadError::malformed)?;
    if envelope.kind.is_empty() {
        return Err(ReadError::malformed("event kind must not be empty"));
    }
    let payload = match envelope.kind.as_str() {
        "run_started"
        | "phase_changed"
        | "session_started"
        | "request_finished"
        | "command_finished"
        | "session_ended"
        | "checkpoint_published"
        | "run_finished" => {
            let event: KnownEvent =
                serde_json::from_value(json!({"kind": envelope.kind, "data": envelope.data}))
                    .map_err(ReadError::malformed)?;
            if let KnownEvent::RunFinished { usage, cleanup, .. } = &event {
                usage.validate().map_err(ReadError::malformed)?;
                if matches!(cleanup, CleanupOutcome::Failed { errors } if errors.is_empty()) {
                    return Err(ReadError::malformed("failed cleanup must include errors"));
                }
            }
            EventPayload::Known(Box::new(event))
        }
        CONTROLLER_OBSERVATION_KIND => {
            let observation = decode_observation(
                &serde_json::to_string(&envelope.data).map_err(ReadError::malformed)?,
            )?;
            if let ObservationPayload::Known(known) = observation.payload()
                && let KnownObservation::ControllerStarted { run_id, .. }
                | KnownObservation::ControllerFinished { run_id, .. } = known.as_ref()
                && *run_id != envelope.run_id
            {
                return Err(ReadError::malformed(
                    "controller observation run disagrees with envelope",
                ));
            }
            EventPayload::Observation(Box::new(observation))
        }
        _ => EventPayload::Unknown {
            kind: envelope.kind.clone(),
            data: envelope.data.clone(),
        },
    };
    Ok(DecodedEvent { envelope, payload })
}

/// Checks ordered observations per run. Gaps are allowed for filtered pages;
/// playtime-storage (#33) separately checks complete durable log continuity.
#[derive(Default)]
pub struct EventOrder {
    last: BTreeMap<RunId, NonZeroU64>,
}

impl EventOrder {
    pub fn observe(&mut self, event: &DecodedEvent) -> Result<(), ReadError> {
        let envelope = event.envelope();
        if self
            .last
            .get(&envelope.run_id)
            .is_some_and(|last| envelope.sequence <= *last)
        {
            return Err(ReadError::malformed(
                "event sequence must increase within its run",
            ));
        }
        self.last.insert(envelope.run_id.clone(), envelope.sequence);
        Ok(())
    }
}
