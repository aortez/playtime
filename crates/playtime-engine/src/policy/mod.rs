//! Internal conversation policy. Proposals carry no effect IDs, token grants,
//! deadlines, accounting mutations, or publication authority.
//!
//! There is one concrete policy. Session-neutral purposes and record contracts
//! are a separate change; this boundary deliberately preserves memento v1.

mod memento;
pub(crate) mod memento_contract;

use playtime_core::StopReason;
use serde_json::Value;

use crate::{ControllerConfig, ToolRequest};

pub(crate) use memento::Memento;
pub use memento::SessionInput;

/// Read-only facts maintained by the runtime, including counts of admitted
/// attempts (not just successful responses). A policy cannot reset these.
#[derive(Clone, Copy)]
pub(crate) struct View<'a> {
    pub config: &'a ControllerConfig,
    pub facts: Facts,
}

#[derive(Clone, Copy)]
pub(crate) struct Facts {
    pub remaining_tokens: u64,
    pub session: u32,
    pub turns: u32,
    pub handoff_attempts: u32,
    pub current_handoff: bool,
    pub parse_streak: u32,
    pub session_parse_errors: u32,
    pub ended: bool,
    pub finished: bool,
}

pub(crate) struct Inference {
    pub handoff: bool,
    pub messages: Vec<Value>,
    pub tools: Vec<Value>,
}

pub(crate) enum Proposal {
    StartSession,
    BeginHandoff,
    Infer(Inference),
    Tool(ToolRequest),
    ResponseComplete,
    Checkpoint { missing_handoff: bool },
    Finish(StopReason),
}

pub(crate) enum Recovery {
    Stop(StopReason),
    Retry { end_session: bool },
}
