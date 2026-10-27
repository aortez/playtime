use std::collections::BTreeMap;

use playtime_core::{
    RequestId, RequestPurpose, RunId, RunMode, RunParameters, SessionId, Sha256Digest, StopReason,
};
use playtime_protocol::{archive::ContentRef, events::CleanupOutcome, v1::UsageSummary};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::evidence::Evidence;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ControllerConfig {
    pub parameters: RunParameters,
    pub protocol: String,
    pub prompt: String,
    pub inventory: String,
    pub initial_workspace: ContentRef,
    pub initial_files: FileIndex,
}

/// Run-scoped configuration has no dummy sessions, handoff or memory settings.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceConfig {
    pub parameters: playtime_core::CommonParameters,
    pub protocol: String,
    pub prompt: String,
    pub context_reserve: u64,
    pub initial_workspace: ContentRef,
    pub initial_files: FileIndex,
}

pub(crate) struct RuntimeConfig {
    pub parameters: playtime_core::CommonParameters,
    pub initial_workspace: ContentRef,
    pub memento: Option<ControllerConfig>,
    pub workspace: Option<WorkspaceConfig>,
    pub compaction: Option<crate::compaction::CompactionConfig>,
}

impl RuntimeConfig {
    pub fn memento(config: ControllerConfig) -> Self {
        Self {
            parameters: config.parameters.common(),
            initial_workspace: config.initial_workspace.clone(),
            memento: Some(config),
            workspace: None,
            compaction: None,
        }
    }
    #[cfg(feature = "test-harness")]
    pub fn workspace(config: WorkspaceConfig) -> Self {
        Self {
            parameters: config.parameters.clone(),
            initial_workspace: config.initial_workspace.clone(),
            memento: None,
            workspace: Some(config),
            compaction: None,
        }
    }
    pub fn compaction(config: crate::compaction::CompactionConfig) -> Self {
        Self {
            parameters: config.parameters.clone(),
            initial_workspace: config.initial_workspace.clone(),
            memento: None,
            workspace: None,
            compaction: Some(config),
        }
    }
}

pub type FileIndex = BTreeMap<String, FileInfo>;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FileInfo {
    pub size_bytes: u64,
    pub mode: u32,
    /// Archive entry type, including links and non-regular entries as data.
    pub entry_type: String,
    pub sha256: Option<Sha256Digest>,
    pub link: Option<String>,
}

/// Exact delivered memory. An absent or unreadable file has empty content and
/// an explicit status, never fabricated successful memory.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Memory {
    pub status: String,
    pub content: String,
}

impl Memory {
    pub fn loaded(content: String) -> Self {
        Self {
            status: "loaded".into(),
            content,
        }
    }

    pub(crate) fn bounded(mut self, limit: u64) -> Self {
        if self.content.len() as u64 > limit {
            self.status = "oversized; inspect MEMENTO.md using tools and shorten it".into();
            self.content.clear();
        }
        self.status = self.status.chars().take(512).collect();
        self
    }
}

/// A stopped workshop's immutable artifact, not yet a published checkpoint.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub workspace: ContentRef,
    pub files: FileIndex,
    pub memory: Memory,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkspaceSnapshot {
    pub workspace: ContentRef,
    pub files: FileIndex,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Capture {
    pub snapshot: WorkspaceSnapshot,
    pub interrupted: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Checkpoint {
    pub session_id: SessionId,
    pub ordinal: u32,
    pub snapshot: Snapshot,
    pub evidence: Option<Evidence>,
    pub handoff_saved: bool,
    pub interrupted: bool,
}

/// External tools need a confirmed bridge reply; expiration is infrastructure
/// uncertainty, never synthetic process-termination evidence. Simulation retains
/// its historical timer behavior. The owner freezes this policy with its profile.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ToolPolicy {
    #[default]
    Simulation,
    ConfirmedExternal {
        return_overhead_ms: u64,
    },
}

/// ID creation is injected; sampling randomness is solely the configured seed
/// plus the admitted request ordinal (modulo 2^31).
pub trait IdSource {
    fn session(&mut self, ordinal: u32) -> SessionId;
    fn request(&mut self, ordinal: u64) -> RequestId;
}

#[derive(Clone, Debug, PartialEq)]
pub struct ModelRequest {
    pub request_id: RequestId,
    pub call: u64,
    pub purpose: RequestPurpose,
    pub sampling_seed: u32,
    pub maximum_tokens: u64,
    pub messages: Vec<Value>,
    pub tools: Vec<Value>,
    pub context_input_bound: u64,
    pub context_basis: String,
    pub context_reserve: u64,
}

// Existing memento records retain their exact handoff representation. A
// compaction request has no legacy representation and uses an explicit purpose.
impl Serialize for ModelRequest {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(10))?;
        map.serialize_entry("request_id", &self.request_id)?;
        map.serialize_entry("call", &self.call)?;
        match self.purpose {
            RequestPurpose::Work => map.serialize_entry("handoff", &false)?,
            RequestPurpose::Handoff => map.serialize_entry("handoff", &true)?,
            RequestPurpose::Compaction => map.serialize_entry("purpose", &self.purpose)?,
        }
        map.serialize_entry("sampling_seed", &self.sampling_seed)?;
        map.serialize_entry("maximum_tokens", &self.maximum_tokens)?;
        map.serialize_entry("messages", &self.messages)?;
        map.serialize_entry("tools", &self.tools)?;
        map.serialize_entry("context_input_bound", &self.context_input_bound)?;
        map.serialize_entry("context_basis", &self.context_basis)?;
        map.serialize_entry("context_reserve", &self.context_reserve)?;
        map.end()
    }
}

impl<'de> Deserialize<'de> for ModelRequest {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Wire {
            request_id: RequestId,
            call: u64,
            handoff: Option<bool>,
            purpose: Option<RequestPurpose>,
            sampling_seed: u32,
            maximum_tokens: u64,
            messages: Vec<Value>,
            tools: Vec<Value>,
            context_input_bound: u64,
            context_basis: String,
            context_reserve: u64,
        }
        let wire = Wire::deserialize(deserializer)?;
        let purpose = match (wire.handoff, wire.purpose) {
            (Some(false), None) => RequestPurpose::Work,
            (Some(true), None) => RequestPurpose::Handoff,
            (None, Some(purpose)) => purpose,
            _ => {
                return Err(serde::de::Error::custom(
                    "request must have exactly one purpose representation",
                ));
            }
        };
        Ok(Self {
            request_id: wire.request_id,
            call: wire.call,
            purpose,
            sampling_seed: wire.sampling_seed,
            maximum_tokens: wire.maximum_tokens,
            messages: wire.messages,
            tools: wire.tools,
            context_input_bound: wire.context_input_bound,
            context_basis: wire.context_basis,
            context_reserve: wire.context_reserve,
        })
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelResponse {
    pub message: Value,
    pub done: bool,
    pub generated_tokens: Option<u64>,
    #[serde(default)]
    pub prompt_tokens: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "message", rename_all = "snake_case")]
pub enum ModelFailure {
    Parse(String),
    /// A summary request answered with tool calls although it offered no
    /// tools. The response settled with known usage; the policy decides
    /// whether the compaction is planned again.
    SummaryTools(String),
    Connection(String),
    Incomplete(String),
    MissingUsage(String),
    ByteLimit(String),
    Timeout(String),
    Infrastructure(String),
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ToolRequest {
    pub name: String,
    pub arguments: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Effect {
    pub id: u64,
    /// Absolute milliseconds in the driver's monotonic time domain.
    pub deadline_ms: u64,
    pub kind: EffectKind,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum EffectKind {
    StartWorkspace,
    CaptureWorkspace,
    PublishCapture(Box<Capture>),
    StartSession {
        session_id: SessionId,
        ordinal: u32,
    },
    Model(ModelRequest),
    Tool(ToolRequest),
    /// Stop session processes before reading workspace and memento bytes.
    Snapshot {
        session_id: SessionId,
    },
    /// Acknowledge only once the complete checkpoint is durably published.
    PublishCheckpoint(Box<Checkpoint>),
    Cleanup,
}

#[derive(Clone, Debug)]
pub enum Outcome {
    WorkspaceStarted,
    WorkspaceCaptured(WorkspaceSnapshot),
    Started(Memory),
    Model(ModelResponse),
    /// Explicit backend termination metadata; absent legacy metadata is not
    /// evidence that a compaction summary completed normally.
    ModelCompleted {
        response: ModelResponse,
        finish_reason: String,
    },
    ModelError(ModelFailure),
    Tool(Value),
    Snapshot(Snapshot),
    Published,
    Cleaned(Vec<String>),
    Failed(String),
}

#[derive(Clone, Debug)]
pub enum Input {
    Tick,
    Cancel,
    /// After persisting the outbox and immediately before starting the effect,
    /// acknowledge dispatch. Recheck pending(): a deadline may have replaced it.
    Dispatched {
        effect_id: u64,
    },
    Chunk {
        effect_id: u64,
        chunk: Value,
    },
    Complete {
        effect_id: u64,
        outcome: Outcome,
    },
    /// The driver must not dispatch an effect whose preceding events failed
    /// to persist. Only an already dispatched request retains an unknown charge.
    PersistenceFailed(String),
    /// Actual archive accounting includes envelopes and externalized payloads.
    ExternalLogBudgetExhausted,
    /// A durable receipt for an exact requested checkpoint, including a receipt
    /// arriving after its ordinary effect timed out. Never rewinds the clock.
    PublicationCommitted {
        effect_id: u64,
    },
}

/// Internal observation; #19 supplies UTC timestamps and durable envelopes.
/// Monotonic elapsed time and sequence suffice for deterministic comparisons.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Record {
    pub sequence: u64,
    pub elapsed_ms: u64,
    pub session_id: Option<SessionId>,
    pub kind: String,
    pub data: Value,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct RunResult {
    pub run_id: RunId,
    pub mode: RunMode,
    pub reason: StopReason,
    pub usage: UsageSummary,
    pub model_calls: u64,
    pub tool_parse_errors: u32,
    pub sessions_started: u32,
    pub elapsed_ms: u64,
    pub cleanup: CleanupOutcome,
    pub last_checkpoint: Option<Checkpoint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_capture: Option<Capture>,
    pub diagnostics: Vec<String>,
}
