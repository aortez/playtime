use serde::{Deserialize, Serialize};

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
}

/// A dispatched command with no result is distinct from a successful exit.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CommandOutcome {
    Exited { exit_code: i32 },
    TimedOut { exit_code: Option<i32> },
    Unknown { reason: String },
}
