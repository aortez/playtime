use serde::{Deserialize, Serialize};

/// Every admitted inference consumes the same run allocation and call ordinal.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestPurpose {
    Work,
    Handoff,
    Compaction,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RequestScope {
    Run,
    Session { session_id: crate::SessionId },
}

impl RequestScope {
    pub fn session_id(&self) -> Option<&crate::SessionId> {
        match self {
            Self::Run => None,
            Self::Session { session_id } => Some(session_id),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunMode {
    Experiment,
    ScriptedSmoke,
    LiveSmoke,
    Simulation,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunPhase {
    Admitted,
    StartingSession,
    Working,
    HandingOff,
    Checkpointing,
    CapturingWorkspace,
    CleaningUp,
    Terminal,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryCondition {
    #[default]
    #[serde(rename = "self")]
    SelfWritten,
    SelfPlusEvidence,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    AgentFinished,
    SessionLimit,
    TokenLimit,
    WallLimit,
    LogLimit,
    ModelTimeout,
    ModelConnectionError,
    ModelResponseByteLimit,
    IncompleteModelResponse,
    MissingTokenUsage,
    ToolParseRetryLimit,
    ToolParseErrorLimit,
    TooManyToolsInOneResponse,
    TokenLimitExceededByServer,
    Interrupted,
    InfrastructureFailure,
    ContextLimit,
    CompactionFailed,
    CompactionNoProgress,
    InvalidCompletion,
}

/// A dispatched command with no result is distinct from a successful exit.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CommandOutcome {
    Exited { exit_code: i32 },
    TimedOut { exit_code: Option<i32> },
    Unknown { reason: String },
}
