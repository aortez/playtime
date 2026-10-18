//! Schema mirrors keep schema dependencies out of the core crate. Tests compare
//! actual core serialization against these definitions.
#![allow(dead_code)]
use utoipa::ToSchema;

#[derive(ToSchema)]
#[schema(rename_all = "snake_case")]
pub enum RunMode {
    Experiment,
    ScriptedSmoke,
    LiveSmoke,
    Simulation,
}
#[derive(ToSchema)]
#[schema(rename_all = "snake_case")]
pub enum RunPhase {
    Admitted,
    StartingSession,
    Working,
    HandingOff,
    Checkpointing,
    CleaningUp,
    Terminal,
}
#[derive(ToSchema)]
#[schema(rename_all = "snake_case")]
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
#[derive(ToSchema)]
#[schema(rename_all = "snake_case")]
pub enum MemoryCondition {
    #[schema(rename = "self")]
    SelfWritten,
    SelfPlusEvidence,
}
#[derive(ToSchema)]
pub struct RunParameters {
    pub sampling_seed: u32,
    pub memory_condition: MemoryCondition,
    pub limits: ResourceLimits,
    pub workshop: WorkshopLimits,
}
#[derive(ToSchema)]
pub struct ResourceLimits {
    pub sessions: u32,
    pub work_turns_per_session: u32,
    pub wall_seconds: u64,
    pub generated_tokens: u64,
    pub tokens_per_call: u64,
    pub handoff_tokens: u64,
    pub context_tokens: u64,
    pub model_seconds: u64,
    pub command_seconds: u64,
    pub output_bytes: u64,
    pub memento_bytes: u64,
    pub log_bytes: u64,
    pub tool_parse_retries: u32,
    pub tool_parse_error_limit: Option<u32>,
}
#[derive(ToSchema)]
pub struct WorkshopLimits {
    pub cpu_cores: f64,
    pub memory_bytes: u64,
    pub workspace_bytes: u64,
    pub tmp_bytes: u64,
    pub pids: u32,
}
