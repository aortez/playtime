use std::collections::VecDeque;

use playtime_core::{MemoryCondition, StopReason};
use serde_json::{Value, json};

use crate::{
    Checkpoint, ControllerConfig, Memory, ModelFailure, Snapshot, ToolRequest,
    context::{ContextWindow, canonical, fit_tool_result, message, tool_message},
    evidence::{self, CommandObservation, Evidence},
};

use super::{Inference, Proposal, Recovery, View, memento_contract as contract};

#[derive(Default)]
enum Stage {
    #[default]
    Starting,
    Work,
    Handoff,
    Calls,
    Checkpoint,
    Published,
}

/// Conversation and progression only. Accounting, IDs, dispatch, deadlines,
/// measured context and checkpoint receipts remain in Controller.
#[derive(Default)]
pub(crate) struct Memento {
    stage: Stage,
    messages: Vec<Value>,
    calls: VecDeque<Value>,
    response_had_calls: bool,
}

impl Memento {
    pub fn propose(&mut self, view: View<'_>) -> Proposal {
        let limits = &view.config.parameters.limits;
        match self.stage {
            Stage::Starting => Proposal::StartSession,
            Stage::Work
                if view.facts.turns >= limits.work_turns_per_session.get()
                    || view.facts.remaining_tokens <= limits.handoff_tokens.get() =>
            {
                Proposal::BeginHandoff
            }
            Stage::Handoff
                if view.facts.handoff_attempts >= 2 || view.facts.remaining_tokens == 0 =>
            {
                Proposal::Checkpoint {
                    missing_handoff: true,
                }
            }
            Stage::Work | Stage::Handoff => {
                let handoff = matches!(self.stage, Stage::Handoff);
                Proposal::Infer(Inference {
                    handoff,
                    messages: self.messages.clone(),
                    tools: contract::tools(handoff),
                })
            }
            Stage::Calls => self
                .calls
                .pop_front()
                .map_or(Proposal::ResponseComplete, |call| {
                    Proposal::Tool(ToolRequest {
                        name: call["function"]["name"]
                            .as_str()
                            .unwrap_or("unknown")
                            .into(),
                        arguments: call["function"]["arguments"].clone(),
                    })
                }),
            Stage::Checkpoint => Proposal::Checkpoint {
                missing_handoff: false,
            },
            Stage::Published if view.facts.finished => Proposal::Finish(StopReason::AgentFinished),
            Stage::Published if view.facts.remaining_tokens == 0 => {
                Proposal::Finish(StopReason::TokenLimit)
            }
            Stage::Published if view.facts.session >= limits.sessions.get() => {
                Proposal::Finish(StopReason::SessionLimit)
            }
            Stage::Published => Proposal::StartSession,
        }
    }

    pub fn session_starting(&mut self) {
        self.messages.clear();
        self.calls.clear();
    }

    pub fn session_started(
        &mut self,
        view: View<'_>,
        memory: &Memory,
        evidence: Option<&Evidence>,
    ) {
        self.messages = SessionInput {
            parameters: &view.config.parameters,
            protocol: &view.config.protocol,
            prompt: &view.config.prompt,
            inventory: &view.config.inventory,
        }
        .messages(view.facts.session, view.facts.remaining_tokens, memory);
        if let Some(evidence) = evidence {
            self.messages
                .push(message("user", evidence.content.clone()));
        }
        self.stage = Stage::Work;
    }

    pub fn begin_handoff(&mut self, view: View<'_>) {
        self.stage = Stage::Handoff;
        self.messages.push(message("user", format!(
            "This session is ending. Call end_session now with the memento your next session needs. Only end_session is available. Maximum {} UTF-8 bytes. Budget: {}",
            view.config.parameters.limits.memento_bytes, canonical(&budget(view)))));
    }

    pub fn context_rejected(&mut self) {
        self.stage = Stage::Checkpoint;
    }

    pub fn valid_message(message: &Value) -> bool {
        message.is_object()
            && message["role"] == "assistant"
            && ["content", "thinking"]
                .iter()
                .all(|key| message.get(key).is_none_or(Value::is_string))
            && message.get("tool_calls").is_none_or(Value::is_array)
    }

    /// Receives a complete, byte-bounded response after runtime settlement.
    pub fn model_response(&mut self, message: Value) {
        self.calls = message["tool_calls"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into();
        self.response_had_calls = !self.calls.is_empty();
        self.messages.push(message);
        self.stage = Stage::Calls;
    }

    pub fn recovery(&self, view: View<'_>, error: &ModelFailure) -> Recovery {
        let limits = &view.config.parameters.limits;
        let reason = match error {
            ModelFailure::Parse(_) if view.facts.parse_streak <= limits.tool_parse_retries => {
                return Recovery::Retry {
                    end_session: view.facts.session_parse_errors >= contract::parse_ceiling(limits)
                        && !view.facts.current_handoff,
                };
            }
            ModelFailure::Parse(_) => StopReason::ToolParseRetryLimit,
            ModelFailure::Connection(_) => StopReason::ModelConnectionError,
            ModelFailure::Incomplete(_) => StopReason::IncompleteModelResponse,
            ModelFailure::MissingUsage(_) => StopReason::MissingTokenUsage,
            ModelFailure::ByteLimit(_) => StopReason::ModelResponseByteLimit,
            ModelFailure::Timeout(_) => StopReason::ModelTimeout,
            ModelFailure::Infrastructure(_) => StopReason::InfrastructureFailure,
        };
        Recovery::Stop(reason)
    }

    pub fn retry(&mut self, view: View<'_>) {
        self.messages.push(message("user", format!(
            "The server rejected your last response because a tool call could not be parsed. None of that response's tools ran. Try again with valid JSON object arguments. The failed request's full generation allowance was charged to the budget; its actual usage is unknown. Budget: {}", canonical(&budget(view)))));
    }

    pub fn fit_result(
        &self,
        view: View<'_>,
        context: &ContextWindow,
        name: &str,
        original: &Value,
    ) -> Value {
        if view.facts.ended || view.facts.current_handoff {
            return original.clone();
        }
        let limits = &view.config.parameters.limits;
        let context = context.estimate(&self.messages, &contract::tools(true)).0;
        let reserve = context
            .saturating_add(limits.handoff_tokens.get())
            .saturating_add(8192)
            .saturating_add(self.calls.len() as u64 * 512);
        let available = limits.context_tokens.get().saturating_sub(reserve).max(512);
        fit_tool_result(name, original.clone(), available)
    }

    pub fn tool_result(&mut self, name: &str, result: &Value) {
        self.messages.push(tool_message(name, result));
        self.stage = Stage::Calls;
    }

    /// The optional feedback is a typed observation for the runtime to record.
    pub fn after_response(&mut self, view: View<'_>, complete: bool) -> Option<&'static str> {
        if view.facts.ended {
            self.stage = Stage::Checkpoint;
            return None;
        }
        if view.facts.current_handoff {
            self.stage = Stage::Handoff;
            if complete && view.facts.handoff_attempts == 1 && !self.response_had_calls {
                let feedback = "Your last response did not call a tool, so it did not save a memento. Call the end_session tool with your memento now. Returning JSON as ordinary text does not invoke the tool.";
                self.messages.push(message("user", feedback));
                return Some(feedback);
            }
        } else {
            self.messages.push(message(
                "user",
                format!(
                    "Continue independently. Budget: {}. Work turns left this session: {}.",
                    canonical(&budget(view)),
                    view.config.parameters.limits.work_turns_per_session.get() - view.facts.turns
                ),
            ));
            self.stage = Stage::Work;
        }
        None
    }

    pub fn checkpoint_evidence(
        config: &ControllerConfig,
        session: u32,
        previous: Option<&Checkpoint>,
        snapshot: &Snapshot,
        commands: &[CommandObservation],
    ) -> Option<Evidence> {
        (config.parameters.memory_condition == MemoryCondition::SelfPlusEvidence).then(|| {
            let previous = previous.map(|checkpoint| &checkpoint.snapshot);
            evidence::build(
                session,
                previous.map_or(&config.initial_files, |snapshot| &snapshot.files),
                &snapshot.files,
                commands,
                previous
                    .map_or(&config.initial_workspace, |snapshot| &snapshot.workspace)
                    .clone(),
                snapshot.workspace.clone(),
            )
        })
    }

    pub fn published(&mut self) {
        self.stage = Stage::Published;
    }

    pub fn stopped(&mut self) {
        self.calls.clear();
    }
}

fn budget(view: View<'_>) -> Value {
    json!({"generated_tokens_remaining": view.facts.remaining_tokens,
        "wall_seconds_limit": view.config.parameters.limits.wall_seconds,
        "session": view.facts.session, "sessions_total": view.config.parameters.limits.sessions})
}

/// Shared prompt construction and conservative initial-context validation. This
/// estimates bytes, not exact tokenizer usage or available GPU memory.
pub struct SessionInput<'a> {
    pub parameters: &'a playtime_core::RunParameters,
    pub protocol: &'a str,
    pub prompt: &'a str,
    pub inventory: &'a str,
}
impl SessionInput<'_> {
    pub(crate) fn messages(
        &self,
        session: u32,
        remaining: u64,
        memory: &crate::Memory,
    ) -> Vec<Value> {
        let l = &self.parameters.limits;
        let budget = json!({"generated_tokens_remaining": remaining,
            "wall_seconds_limit": l.wall_seconds, "session": session, "sessions_total": l.sessions});
        vec![
            message("system", self.protocol),
            message(
                "user",
                format!(
                    "{}\nSession {}/{}.\nBudget: {}\nMemento limit: {} UTF-8 bytes.\n\n{}\n\nMemento status: {}\n{}",
                    self.prompt,
                    session,
                    l.sessions,
                    canonical(&budget),
                    l.memento_bytes,
                    self.inventory,
                    memory.status,
                    memory.content
                ),
            ),
        ]
    }
    pub fn initial_context_required(&self) -> u64 {
        let l = &self.parameters.limits;
        let messages = self.messages(
            1,
            l.generated_tokens.get(),
            &crate::Memory {
                status: "missing".into(),
                content: String::new(),
            },
        );
        let (input, _) = ContextWindow::default().estimate(&messages, &contract::TOOLS);
        let output = l.tokens_per_call.get().min(
            l.generated_tokens
                .get()
                .saturating_sub(l.handoff_tokens.get()),
        );
        input
            .saturating_add(output)
            .saturating_add(contract::request_context_reserve(l, false))
            .saturating_add(1)
    }
}
