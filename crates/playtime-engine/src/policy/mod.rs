//! Internal conversation policy. Proposals carry no effect IDs, token grants,
//! deadlines, accounting mutations, or publication authority.
//!
//! Memento is the only production policy. The feature-gated probe proves the
//! run-scoped contract without exposing a production compaction harness.

mod memento;
pub(crate) mod memento_contract;
#[cfg(feature = "test-harness")]
mod probe;

use playtime_core::{RequestPurpose, StopReason};
use serde_json::Value;

use crate::{
    ControllerConfig, Memory, ModelFailure, RuntimeConfig, ToolRequest, context::ContextWindow,
    evidence::Evidence,
};

pub(crate) use memento::Memento;
pub use memento::SessionInput;

/// Read-only facts maintained by the runtime, including counts of admitted
/// attempts (not just successful responses). A policy cannot reset these.
#[derive(Clone, Copy)]
pub(crate) struct MementoView<'a> {
    pub config: &'a ControllerConfig,
    pub facts: Facts,
}

#[derive(Clone, Copy)]
pub(crate) struct View<'a> {
    pub config: &'a RuntimeConfig,
    pub facts: Facts,
}
impl<'a> View<'a> {
    fn memento(self) -> MementoView<'a> {
        MementoView {
            config: self
                .config
                .memento
                .as_ref()
                .expect("memento policy configuration"),
            facts: self.facts,
        }
    }
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
    pub purpose: RequestPurpose,
    pub messages: Vec<Value>,
    pub tools: Vec<Value>,
}

pub(crate) enum Proposal {
    #[cfg_attr(not(feature = "test-harness"), allow(dead_code))]
    StartWorkspace,
    #[cfg_attr(not(feature = "test-harness"), allow(dead_code))]
    CaptureWorkspace,
    StartSession,
    BeginHandoff,
    Infer(Inference),
    Tool(ToolRequest),
    ResponseComplete,
    Checkpoint {
        missing_handoff: bool,
    },
    Finish(StopReason),
}

pub(crate) enum Recovery {
    Stop(StopReason),
    Retry { end_session: bool },
}

/// Fixed capabilities of the run-scoped test profile. This does not install
/// a policy or make it selectable in a production worker.
pub(crate) fn workspace_tools(purpose: RequestPurpose) -> Vec<Value> {
    if purpose != RequestPurpose::Work {
        return Vec::new();
    }
    let mut tool = memento_contract::TOOLS[0].clone();
    tool["function"]["description"] =
        serde_json::json!("Execute a bounded Bash command in the owned workshop.");
    vec![tool]
}

pub(crate) struct RequestContract {
    pub maximum_tokens: u64,
    pub generation_reserve: u64,
    pub context_reserve: u64,
    pub tools: Vec<Value>,
}

impl RuntimeConfig {
    pub fn request_contract(
        &self,
        purpose: RequestPurpose,
    ) -> Result<RequestContract, playtime_core::InvalidValue> {
        if let Some(config) = &self.memento {
            let handoff = match purpose {
                RequestPurpose::Work => false,
                RequestPurpose::Handoff => true,
                RequestPurpose::Compaction => {
                    return Err(playtime_core::InvalidValue(
                        "memento does not support compaction requests",
                    ));
                }
            };
            let limits = &config.parameters.limits;
            Ok(RequestContract {
                maximum_tokens: if handoff {
                    limits.handoff_tokens.get()
                } else {
                    limits.tokens_per_call.get()
                },
                generation_reserve: if handoff {
                    0
                } else {
                    limits.handoff_tokens.get()
                },
                context_reserve: memento_contract::request_context_reserve(limits, handoff),
                tools: memento_contract::tools(handoff),
            })
        } else {
            let config = self
                .workspace
                .as_ref()
                .ok_or(playtime_core::InvalidValue("missing workspace profile"))?;
            if purpose == RequestPurpose::Handoff {
                return Err(playtime_core::InvalidValue(
                    "run-scoped profile does not support memento handoff",
                ));
            }
            Ok(RequestContract {
                maximum_tokens: self.parameters.limits.tokens_per_call.get(),
                generation_reserve: 0,
                context_reserve: config.context_reserve,
                tools: workspace_tools(purpose),
            })
        }
    }
}

pub(crate) enum Policy {
    Memento(Memento),
    #[cfg(feature = "test-harness")]
    Probe(probe::Probe),
}

impl Policy {
    #[cfg(feature = "test-harness")]
    pub fn probe() -> Self {
        Self::Probe(probe::Probe::default())
    }
    pub fn propose(&mut self, view: View<'_>) -> Proposal {
        match self {
            Self::Memento(policy) => policy.propose(view.memento()),
            #[cfg(feature = "test-harness")]
            Self::Probe(policy) => policy.propose(view),
        }
    }
    pub fn session_starting(&mut self) {
        match self {
            Self::Memento(policy) => policy.session_starting(),
            #[cfg(feature = "test-harness")]
            Self::Probe(_) => unreachable!("run-scoped policy cannot start a session"),
        }
    }
    pub fn session_started(
        &mut self,
        view: View<'_>,
        memory: &Memory,
        evidence: Option<&Evidence>,
    ) {
        match self {
            Self::Memento(policy) => policy.session_started(view.memento(), memory, evidence),
            #[cfg(feature = "test-harness")]
            Self::Probe(_) => unreachable!("run-scoped policy has no memory delivery"),
        }
    }
    pub fn workspace_started(&mut self) {
        match self {
            Self::Memento(_) => unreachable!("memento requires a session"),
            #[cfg(feature = "test-harness")]
            Self::Probe(policy) => policy.started(),
        }
    }
    pub fn begin_handoff(&mut self, view: View<'_>) {
        match self {
            Self::Memento(policy) => policy.begin_handoff(view.memento()),
            #[cfg(feature = "test-harness")]
            Self::Probe(_) => unreachable!("test policy has no handoff"),
        }
    }
    pub fn context_rejected(&mut self) {
        match self {
            Self::Memento(policy) => policy.context_rejected(),
            #[cfg(feature = "test-harness")]
            Self::Probe(_) => {}
        }
    }
    pub fn model_response(&mut self, message: Value) {
        match self {
            Self::Memento(policy) => policy.model_response(message),
            #[cfg(feature = "test-harness")]
            Self::Probe(policy) => policy.model_response(message),
        }
    }
    pub fn recovery(&self, view: View<'_>, error: &ModelFailure) -> Recovery {
        match self {
            Self::Memento(policy) => policy.recovery(view.memento(), error),
            #[cfg(feature = "test-harness")]
            Self::Probe(_) => Recovery::Stop(match error {
                ModelFailure::Parse(_) => StopReason::ToolParseRetryLimit,
                ModelFailure::Connection(_) => StopReason::ModelConnectionError,
                ModelFailure::Incomplete(_) => StopReason::IncompleteModelResponse,
                ModelFailure::MissingUsage(_) => StopReason::MissingTokenUsage,
                ModelFailure::ByteLimit(_) => StopReason::ModelResponseByteLimit,
                ModelFailure::Timeout(_) => StopReason::ModelTimeout,
                ModelFailure::Infrastructure(_) => StopReason::InfrastructureFailure,
            }),
        }
    }
    pub fn retry(&mut self, view: View<'_>) {
        match self {
            Self::Memento(policy) => policy.retry(view.memento()),
            #[cfg(feature = "test-harness")]
            Self::Probe(_) => unreachable!("test policy never retries"),
        }
    }
    pub fn fit_result(
        &self,
        view: View<'_>,
        context: &ContextWindow,
        name: &str,
        original: &Value,
    ) -> Value {
        match self {
            Self::Memento(policy) => policy.fit_result(view.memento(), context, name, original),
            #[cfg(feature = "test-harness")]
            Self::Probe(_) => crate::context::fit_tool_result(
                name,
                original.clone(),
                view.config.parameters.limits.output_bytes.get(),
            ),
        }
    }
    pub fn tool_result(&mut self, name: &str, result: &Value) {
        match self {
            Self::Memento(policy) => policy.tool_result(name, result),
            #[cfg(feature = "test-harness")]
            Self::Probe(policy) => policy.tool_result(name, result),
        }
    }
    pub fn after_response(&mut self, view: View<'_>, complete: bool) -> Option<&'static str> {
        match self {
            Self::Memento(policy) => policy.after_response(view.memento(), complete),
            #[cfg(feature = "test-harness")]
            Self::Probe(policy) => {
                policy.after_response();
                None
            }
        }
    }
    pub fn published(&mut self) {
        match self {
            Self::Memento(policy) => policy.published(),
            #[cfg(feature = "test-harness")]
            Self::Probe(policy) => policy.published(),
        }
    }
    pub fn stopped(&mut self) {
        match self {
            Self::Memento(policy) => policy.stopped(),
            #[cfg(feature = "test-harness")]
            Self::Probe(policy) => policy.stopped(),
        }
    }
}
