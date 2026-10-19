use std::collections::BTreeMap;

use playtime_core::{
    RequestId, RunId, RunMode, RunParameters, SessionId, Sha256Digest, StopReason,
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

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ModelRequest {
    pub request_id: RequestId,
    pub call: u64,
    pub handoff: bool,
    pub sampling_seed: u32,
    pub maximum_tokens: u64,
    pub messages: Vec<Value>,
    pub tools: Vec<Value>,
    pub context_input_bound: u64,
    pub context_basis: String,
    pub context_reserve: u64,
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
    Started(Memory),
    Model(ModelResponse),
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
    pub diagnostics: Vec<String>,
}
