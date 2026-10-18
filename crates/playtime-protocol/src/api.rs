//! HTTP v1 contracts, independent of the runtime and archive implementation.

pub mod cursor;
pub mod schema;

use playtime_core::{RunId, RunMode, RunParameters, RunPhase, SessionId, StopReason, UtcTime};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::ToSchema;

use crate::{
    archive::{ContentRef, WorkerIdentity},
    events::EventEnvelope,
    v1::UsageSummary,
};
pub use cursor::{EventCursor, Frontier, Prefix};

pub const SERVICE_HEADER: &str = "x-playtime-service-id";
pub const REQUEST_HEADER: &str = "x-request-id";
pub const API_VERSION: u32 = 1;
pub const DEFAULT_PORT: u16 = 8766;

/// New HTTP identifiers are deliberately separate from archive paths.
pub fn valid_id(value: &str) -> bool {
    (1..=128).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

macro_rules! id {
    ($name:ident) => {
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize, ToSchema)]
        #[serde(try_from = "String", into = "String")]
        #[schema(value_type = String, min_length = 1, max_length = 128, pattern = "^[A-Za-z0-9_-]+$")]
        pub struct $name(String);
        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, playtime_core::InvalidValue> {
                let value = value.into();
                if valid_id(&value) { Ok(Self(value)) } else { Err(playtime_core::InvalidValue("invalid API identifier")) }
            }
            pub fn as_str(&self) -> &str { &self.0 }
        }
        impl TryFrom<String> for $name {
            type Error = playtime_core::InvalidValue;
            fn try_from(value: String) -> Result<Self, Self::Error> { Self::new(value) }
        }
        impl From<$name> for String { fn from(value: $name) -> Self { value.0 } }
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(&self.0) }
        }
    };
}
id!(ServiceId);
id!(RecipeId);
id!(ArtifactId);
id!(PreviewRevision);
id!(IdempotencyKey);

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct ApiLimits {
    pub request_bytes: usize,
    pub json_bytes: usize,
    pub event_bytes: usize,
    pub sse_framing_bytes: usize,
    pub artifact_bytes: u64,
    pub cursor_bytes: usize,
    pub default_page_items: u32,
    pub max_page_items: u32,
    pub event_streams: usize,
    pub artifact_transfers: usize,
    pub stream_pending_records: usize,
    pub stream_pending_bytes: usize,
    pub handler_seconds: u64,
    pub connect_seconds: u64,
    pub heartbeat_seconds: u64,
    pub stream_idle_seconds: u64,
}
impl Default for ApiLimits {
    fn default() -> Self {
        Self {
            request_bytes: 1 << 20,
            json_bytes: 4 << 20,
            event_bytes: 1 << 20,
            sse_framing_bytes: 8 << 10,
            artifact_bytes: 64 << 20,
            cursor_bytes: 2048,
            default_page_items: 200,
            max_page_items: 1000,
            event_streams: 8,
            artifact_transfers: 4,
            stream_pending_records: 64,
            stream_pending_bytes: 4 << 20,
            handler_seconds: 30,
            connect_seconds: 5,
            heartbeat_seconds: 15,
            stream_idle_seconds: 45,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, ToSchema)]
pub struct Capabilities {
    pub api_versions: Vec<u32>,
    pub archive_versions: Vec<u32>,
    pub event_versions: Vec<u32>,
    pub service_id: ServiceId,
    pub service_build: String,
    pub backend: BackendIdentity,
    #[schema(value_type = Vec<schema::RunMode>)]
    pub modes: Vec<RunMode>,
    pub features: Vec<String>,
    pub unsupported_adapters: Vec<String>,
    pub limits: ApiLimits,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct BackendIdentity {
    pub kind: BackendKind,
    pub description: String,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum BackendKind {
    Fixture,
    Controller,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Connection {
    pub schema_version: u32,
    pub endpoint: String,
    pub service_id: ServiceId,
    pub token_file: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, ToSchema)]
pub struct Recipe {
    pub recipe_id: RecipeId,
    pub name: String,
    #[schema(value_type = Vec<schema::RunMode>)]
    pub modes: Vec<RunMode>,
}
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PreviewRequest {
    #[schema(value_type = schema::RunMode)]
    pub mode: RunMode,
}
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, ToSchema)]
pub struct Preview {
    pub recipe_id: RecipeId,
    #[schema(value_type = schema::RunMode)]
    pub mode: RunMode,
    pub preview_revision: PreviewRevision,
    #[schema(value_type = Object)]
    pub effective_configuration: Value,
    #[schema(value_type = schema::RunParameters)]
    pub parameters: RunParameters,
    pub prompt: String,
    pub protocol: String,
    #[schema(value_type = Object)]
    pub model_identity: Value,
    #[schema(value_type = Object)]
    pub workshop_identity: Value,
    pub seeds: SeedSummary,
    pub readiness: Readiness,
    pub warnings: Vec<String>,
    pub backend: BackendIdentity,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct SeedSummary {
    pub files: u64,
    pub bytes: u64,
    pub digests: Vec<String>,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct Readiness {
    pub ready: bool,
    pub reasons: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct LaunchRequest {
    pub recipe_id: RecipeId,
    #[schema(value_type = schema::RunMode)]
    pub mode: RunMode,
    pub preview_revision: PreviewRevision,
}
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, ToSchema)]
pub struct LaunchResponse {
    #[schema(value_type = String)]
    pub run_id: RunId,
    #[schema(value_type = String, format = DateTime)]
    pub accepted_at: UtcTime,
    pub replayed: bool,
    pub run: RunView,
}
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CancelRequest {}
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, ToSchema)]
pub struct CancelResponse {
    pub pending: bool,
    pub intent: Option<Cancellation>,
    pub run: RunView,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum OrchestrationState {
    Preparing,
    Starting,
    Running,
    Cancelling,
    Reconciling,
    Terminal,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct Orchestration {
    pub state: OrchestrationState,
    pub reason: Option<String>,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Enforcement {
    Requested,
    StopAccepted,
    Stopped,
    Uncertain,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct Cancellation {
    #[schema(value_type = String, format = DateTime)]
    pub requested_at: UtcTime,
    pub enforcement: Enforcement,
    #[schema(value_type = Option<String>, format = DateTime)]
    pub stop_accepted_at: Option<UtcTime>,
    #[schema(value_type = Option<String>, format = DateTime)]
    pub grace_deadline: Option<UtcTime>,
    pub uncertainty: Option<String>,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct ExperimentalOutcome {
    #[schema(value_type = schema::StopReason)]
    pub reason: StopReason,
    pub event_sequence: u64,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum EvidenceAvailability {
    Available,
    Unavailable { reason: String },
    OwnershipUnresolved { reason: String },
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Cleanup {
    Complete,
    Failed { errors: Vec<String> },
    Unknown,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, ToSchema)]
pub struct RunView {
    #[schema(value_type = String)]
    pub run_id: RunId,
    #[schema(value_type = schema::RunMode)]
    pub mode: RunMode,
    #[schema(value_type = String, format = DateTime)]
    pub accepted_at: UtcTime,
    #[schema(value_type = String, format = DateTime)]
    pub updated_at: UtcTime,
    pub recipe_id: RecipeId,
    pub preview_revision: PreviewRevision,
    pub backend: BackendIdentity,
    pub orchestration: Orchestration,
    #[schema(value_type = Option<schema::RunPhase>)]
    pub controller_phase: Option<RunPhase>,
    pub outcome: Option<ExperimentalOutcome>,
    pub usage: Option<UsageSummary>,
    pub checkpoint: Option<CheckpointSummary>,
    pub cancellation: Option<Cancellation>,
    pub cleanup: Cleanup,
    pub evidence: EvidenceAvailability,
    pub input_manifest: Option<ContentRef>,
    pub worker: Option<WorkerIdentity>,
    pub last_confirmed_frontier: Option<Frontier>,
    pub event_anchors: Option<EventAnchors>,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct EventAnchors {
    pub frontier: Frontier,
    pub history_cursor: String,
    pub live_cursor: String,
}
impl EventAnchors {
    pub fn new(frontier: Frontier) -> Result<Self, crate::ReadError> {
        Ok(Self {
            history_cursor: EventCursor::new(frontier.clone(), 0)?.encode()?,
            live_cursor: EventCursor::new(frontier.clone(), frontier.prefix.sequence)?.encode()?,
            frontier,
        })
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, ToSchema)]
pub struct EventPage {
    pub events: Vec<EventEnvelope>,
    pub frontier: Frontier,
    pub has_more: bool,
    pub next_cursor: Option<String>,
    pub resume_cursor: String,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum StreamEndStatus {
    CommittedCompletion,
    Incomplete,
    Interrupted,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct StreamEnd {
    pub status: StreamEndStatus,
    pub frontier: Frontier,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct CheckpointSummary {
    #[schema(value_type = String)]
    pub session_id: SessionId,
    pub published_sequence: u64,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ContentAvailability {
    Available {
        artifact_id: ArtifactId,
        content: ContentRef,
    },
    Unavailable {
        reason: String,
    },
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct FileVersion {
    pub name: String,
    pub kind: String,
    pub content: ContentAvailability,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct Checkpoint {
    pub checkpoint: CheckpointSummary,
    pub incoming_memory: ContentAvailability,
    pub outgoing_memory: ContentAvailability,
    pub evidence: ContentAvailability,
    pub workspace: ContentAvailability,
    pub files: Vec<FileVersion>,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct Artifact {
    pub artifact_id: ArtifactId,
    pub name: String,
    pub kind: String,
    pub content: ContentRef,
    #[schema(value_type = Option<String>)]
    pub session_id: Option<SessionId>,
    pub published_sequence: u64,
    pub frozen_input: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, ToSchema)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next_cursor: Option<String>,
    /// Checkpoint/artifact pages describe exactly this committed prefix.
    pub frontier: Option<Frontier>,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PageQuery {
    pub cursor: Option<String>,
    pub limit: Option<u32>,
}
impl PageQuery {
    pub fn limit(&self, limits: &ApiLimits) -> Result<u32, crate::ReadError> {
        let limit = self.limit.unwrap_or(limits.default_page_items);
        if limit == 0
            || limit > limits.max_page_items
            || self
                .cursor
                .as_ref()
                .is_some_and(|c| c.len() > limits.cursor_bytes)
        {
            return Err(crate::ReadError::malformed("invalid page bounds"));
        }
        Ok(limit)
    }
}
