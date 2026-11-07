//! Internal conversation policy. Proposals carry no effect IDs, token grants,
//! deadlines, accounting mutations, or publication authority.
//!
//! Memento and continuous compaction share runtime enforcement. The separate
//! feature-gated workspace probe remains a CPU-only test policy.

mod compaction;
mod failures;
mod memento;
pub(crate) mod memento_contract;
#[cfg(feature = "test-harness")]
mod probe;
mod retry_attempt;

use playtime_core::{RequestPurpose, StopReason};
use serde_json::Value;

use crate::{
    ControllerConfig, Memory, ModelFailure, RuntimeConfig, ToolRequest,
    context::{Calibration, ContextWindow},
    evidence::Evidence,
};

pub(crate) use failures::{ModelErrorSource, ToolResultSource};
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
    /// Wall time left before the run's deadline.
    pub remaining_ms: u64,
    /// Time spent inside dispatched model requests, settled known or unknown.
    pub model_ms: u64,
    pub known_generated_tokens: u64,
    /// The densest server prompt measurement so far, if any.
    pub calibration: Option<Calibration>,
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
    Stop(StopReason),
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
        } else if let Some(config) = &self.compaction {
            let (maximum_tokens, generation_reserve, context_reserve, tools) = match purpose {
                RequestPurpose::Work => (
                    self.parameters.limits.tokens_per_call.get(),
                    0,
                    config.work_reserve(),
                    crate::compaction::tools(config.settings.allow_finish_run),
                ),
                RequestPurpose::Compaction => (
                    config.settings.summary_tokens.get(),
                    self.parameters.limits.tokens_per_call.get(),
                    crate::compaction::CompactionSettings::safety_margin(
                        self.parameters.limits.context_tokens.get(),
                    ),
                    Vec::new(),
                ),
                RequestPurpose::Handoff => {
                    return Err(playtime_core::InvalidValue(
                        "continuous conversation has no memento handoff",
                    ));
                }
            };
            Ok(RequestContract {
                maximum_tokens,
                generation_reserve,
                context_reserve,
                tools,
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
    Compaction(Box<compaction::Compaction>),
}

impl Policy {
    #[cfg(feature = "test-harness")]
    pub fn probe() -> Self {
        Self::Probe(probe::Probe::default())
    }
    pub fn compaction(
        config: &crate::compaction::CompactionConfig,
    ) -> Result<Self, playtime_core::InvalidValue> {
        let policy = compaction::Compaction::new(config);
        if !policy.initial_fits(config) {
            return Err(playtime_core::InvalidValue(
                "initial compaction context cannot fit below the trigger with its required reserves",
            ));
        }
        Ok(Self::Compaction(Box::new(policy)))
    }
    pub fn propose(&mut self, view: View<'_>, _context: &ContextWindow) -> Proposal {
        match self {
            Self::Memento(policy) => policy.propose(view.memento()),
            #[cfg(feature = "test-harness")]
            Self::Probe(policy) => policy.propose(view),
            Self::Compaction(policy) => policy.propose(view, _context),
        }
    }
    pub fn session_starting(&mut self) {
        match self {
            Self::Memento(policy) => policy.session_starting(),
            _ => {
                unreachable!("run-scoped policy cannot start a session")
            }
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
            _ => {
                unreachable!("run-scoped policy has no memory delivery")
            }
        }
    }
    pub fn workspace_started(&mut self) {
        match self {
            Self::Memento(_) => unreachable!("memento requires a session"),
            #[cfg(feature = "test-harness")]
            Self::Probe(policy) => policy.started(),
            Self::Compaction(policy) => policy.started(),
        }
    }
    pub fn begin_handoff(&mut self, view: View<'_>) {
        match self {
            Self::Memento(policy) => policy.begin_handoff(view.memento()),
            _ => unreachable!("test policy has no handoff"),
        }
    }
    pub fn context_rejected(&mut self) {
        if let Self::Memento(policy) = self {
            policy.context_rejected();
        }
    }
    pub fn model_response(
        &mut self,
        _view: View<'_>,
        _request_id: playtime_core::RequestId,
        response: crate::ModelResponse,
    ) {
        match self {
            Self::Memento(policy) => policy.model_response(response.message),
            #[cfg(feature = "test-harness")]
            Self::Probe(policy) => policy.model_response(response.message),
            Self::Compaction(policy) => policy.model_response(_view, _request_id, response),
        }
    }
    pub fn recovery(&self, view: View<'_>, error: &ModelFailure) -> Recovery {
        match self {
            Self::Memento(policy) => policy.recovery(view.memento(), error),
            Self::Compaction(policy) if policy.retries(view, error) => {
                Recovery::Retry { end_session: false }
            }
            _ => Recovery::Stop(match error {
                ModelFailure::Parse(_) => StopReason::ToolParseRetryLimit,
                ModelFailure::SummaryTools(_) => StopReason::CompactionFailed,
                ModelFailure::ToolBatchLimit(_) => StopReason::TooManyToolsInOneResponse,
                ModelFailure::Connection(_) => StopReason::ModelConnectionError,
                ModelFailure::Incomplete(_) => StopReason::IncompleteModelResponse,
                ModelFailure::MissingUsage(_) => StopReason::MissingTokenUsage,
                ModelFailure::ByteLimit(_) => StopReason::ModelResponseByteLimit,
                ModelFailure::Timeout(_) => StopReason::ModelTimeout,
                ModelFailure::Infrastructure(_) => StopReason::InfrastructureFailure,
            }),
        }
    }
    pub fn retry(&mut self, view: View<'_>, error: &ModelFailure, generated_tokens: Option<u64>) {
        match self {
            Self::Memento(policy) => policy.retry(view.memento(), error, generated_tokens),
            Self::Compaction(policy) => policy.retry(error),
            #[cfg(feature = "test-harness")]
            Self::Probe(_) => unreachable!("test policy never retries"),
        }
    }
    pub fn model_error(
        &mut self,
        request: &crate::RequestContext,
        error: &ModelFailure,
        source: ModelErrorSource,
    ) {
        if let Self::Compaction(policy) = self {
            policy.model_error(request, error, source);
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
            Self::Compaction(policy) => policy.fit_result(view, name, original),
            #[cfg(feature = "test-harness")]
            Self::Probe(_) => crate::context::fit_tool_result(
                name,
                original.clone(),
                view.config.parameters.limits.output_bytes.get(),
            ),
        }
    }
    pub fn tool_result(
        &mut self,
        tool: &ToolRequest,
        original: &Value,
        result: &Value,
        source: ToolResultSource,
    ) {
        match self {
            Self::Memento(policy) => policy.tool_result(&tool.name, result),
            #[cfg(feature = "test-harness")]
            Self::Probe(policy) => policy.tool_result(&tool.name, result),
            Self::Compaction(policy) => policy.tool_result(tool, original, result, source),
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
            Self::Compaction(policy) => {
                policy.after_response(view);
                None
            }
        }
    }
    pub fn published(&mut self) {
        match self {
            Self::Memento(policy) => policy.published(),
            #[cfg(feature = "test-harness")]
            Self::Probe(policy) => policy.published(),
            Self::Compaction(policy) => policy.published(),
        }
    }
    pub fn take_observations(&mut self) -> Vec<(&'static str, Value)> {
        match self {
            Self::Compaction(policy) => policy.take_observations(),
            _ => Vec::new(),
        }
    }
    pub fn stopped(&mut self) {
        match self {
            Self::Memento(policy) => policy.stopped(),
            #[cfg(feature = "test-harness")]
            Self::Probe(policy) => policy.stopped(),
            Self::Compaction(policy) => policy.stopped(),
        }
    }
}

/// The bytes of the server's reason that a parse retry notice keeps.
const REASON_BYTES: usize = 512;

/// Batch-limit recovery names the controller's bound. Parse recovery names the
/// server's reason: the text after the last `, err=` of a message in Ollama's
/// form, which puts its reason after the raw call, or else the whole message,
/// its start within `REASON_BYTES`. The worker keeps the end of a message it
/// cuts, so Ollama's reason survives unless the reason itself is longer than
/// that end. Memento adds its budget sentence.
pub(crate) fn parse_retry_notice(error: &ModelFailure) -> String {
    if let ModelFailure::ToolBatchLimit(message) = error {
        return format!(
            "The controller rejected your last tool batch. None of that response's tools ran. {message}"
        );
    }
    // Summary retries have no notice and preserve their original input.
    let message = match error {
        ModelFailure::Parse(message) => message.as_str(),
        _ => "",
    };
    let reason = message
        .rsplit_once(", err=")
        .map_or(message, |(_, reason)| reason)
        .trim();
    let mut end = reason.len().min(REASON_BYTES);
    while !reason.is_char_boundary(end) {
        end -= 1;
    }
    let named = match (reason[..end].trim_end(), end < reason.len()) {
        ("", _) => String::new(),
        (kept, true) => format!(": {kept}…"),
        (kept, false) => format!(": {kept}"),
    };
    format!(
        "The server rejected your last response because a tool call could not be parsed{named}. None of that response's tools ran. Try again with valid JSON object arguments."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notice(message: &str) -> String {
        parse_retry_notice(&ModelFailure::Parse(message.into()))
    }
    fn named(reason: &str) -> String {
        format!(
            "The server rejected your last response because a tool call could not be parsed{reason}. None of that response's tools ran. Try again with valid JSON object arguments."
        )
    }

    #[test]
    fn the_notice_names_the_reason_that_follows_the_raw_call() {
        // Ollama's form, as a Framework run recorded it.
        assert_eq!(
            notice(
                "error parsing tool call: raw='{\"command\":\"echo ok\"]', err=invalid character ']' after object key:value pair"
            ),
            named(": invalid character ']' after object key:value pair")
        );
        // The raw call may contain the separator itself.
        assert_eq!(
            notice(
                "error parsing tool call: raw='print(\", err=1\")', err=unexpected end of JSON input"
            ),
            named(": unexpected end of JSON input")
        );
        // A long message keeps its end, with a note of the bytes left out.
        assert_eq!(
            notice(
                "error parsing tool call: raw='{\"content\":\"ab [… 5000 bytes omitted …] yz\"]', err=invalid character '\\'' in string escape code "
            ),
            named(": invalid character '\\'' in string escape code")
        );
    }

    #[test]
    fn another_message_is_the_reason_itself_within_the_bound() {
        assert_eq!(
            notice("invalid assistant message"),
            named(": invalid assistant message")
        );
        assert_eq!(notice("  "), named(""));
        let long = "é".repeat(300);
        assert_eq!(notice(&long), named(&format!(": {}…", "é".repeat(256))));
        // The cut steps back to a character boundary, 510 here.
        let wide = "€".repeat(200);
        assert_eq!(notice(&wide), named(&format!(": {}…", "€".repeat(170))));
        let spaced = format!("{} {}", "a".repeat(511), "b".repeat(100));
        assert_eq!(notice(&spaced), named(&format!(": {}…", "a".repeat(511))));
        let exact = "x".repeat(512);
        assert_eq!(notice(&exact), named(&format!(": {exact}")));
        assert_eq!(
            parse_retry_notice(&ModelFailure::SummaryTools("tools in a summary".into())),
            named("")
        );
    }
}
