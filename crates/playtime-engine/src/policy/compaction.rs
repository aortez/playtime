//! Complete-exchange compaction policy. It proposes work; the controller alone
//! admits requests, settles usage, dispatches tools and accepts publication.

use std::collections::VecDeque;

use playtime_core::{RequestId, RequestPurpose, StopReason};
use serde_json::{Value, json};

use super::failures::{Failures, ModelErrorSource, ToolResultSource};
use super::{Inference, Proposal, View, parse_retry_notice};
use crate::{
    ModelFailure, ModelResponse, ToolRequest,
    compaction::{CompactionConfig, summary_transcript},
    context::{ContextWindow, encoded_size, fit_tool_result_with_notice, message, tool_message},
    conversation::{Budget, ContextPolicy, History, Plan},
};

/// Follows a response that called no tool. Otherwise the next request would end
/// with the model's own message and leave it nothing to answer.
const CONTINUE_NOTICE: &str = "Your last response did not call a tool, so the run continues. Continue independently, or call finish_run alone if the task is finished; ordinary prose does not finish the run.";
/// The same notice for a run that offers no finish_run.
const CONTINUE_TO_LIMIT_NOTICE: &str = "Your last response did not call a tool, so the run continues. Continue independently; the run ends when its time or token budget runs out.";

/// Controller notices, then a response and its tool results.
#[derive(Clone)]
struct Exchange {
    request_id: RequestId,
    messages: Vec<Value>,
    failures: Failures,
}

enum Stage {
    Starting,
    Ready,
    Calls,
    Summary { older: usize, before_bytes: u64 },
    Failed(StopReason),
    Published,
}

pub(crate) struct Compaction {
    context_policy: ContextPolicy,
    stage: Stage,
    root: Vec<Value>,
    summary: Option<Value>,
    /// Only failures from exchanges already replaced by a summary. Pending
    /// exchanges retain their own bounded observations until that transition.
    failures: Failures,
    exchanges: Vec<Exchange>,
    /// Failed attempts have no response exchange. They travel with the next
    /// successful work exchange, like retry notices, even across compaction.
    pending_failures: Failures,
    current: Option<Exchange>,
    retry_attempt: Option<Value>,
    /// Notices for the next work request. They open its exchange.
    notices: Vec<Value>,
    calls: VecDeque<Value>,
    ordinal: u64,
    observations: VecDeque<(&'static str, Value)>,
}

impl Compaction {
    pub fn new(config: &CompactionConfig) -> Result<Self, playtime_core::InvalidValue> {
        Ok(Self {
            context_policy: ContextPolicy::new(
                config.parameters.limits.clone(),
                config.settings.clone(),
                crate::compaction::tools(config.settings.allow_finish_run),
            )?,
            stage: Stage::Starting,
            root: vec![
                message(
                    "system",
                    format!("{}\n\n{}", config.protocol, config.inventory),
                ),
                message("user", &config.prompt),
            ],
            summary: None,
            failures: Failures::default(),
            exchanges: Vec::new(),
            pending_failures: Failures::default(),
            current: None,
            retry_attempt: None,
            notices: Vec::new(),
            calls: VecDeque::new(),
            ordinal: 0,
            observations: VecDeque::new(),
        })
    }

    fn messages(
        &self,
        exchanges: &[Exchange],
        summary: Option<&Value>,
        failures: &Failures,
    ) -> Vec<Value> {
        let mut messages = self.root.clone();
        messages.extend(summary.cloned());
        messages.extend(failures.message());
        messages.extend(
            exchanges
                .iter()
                .flat_map(|exchange| exchange.messages.clone()),
        );
        messages
    }

    /// The next work input. Pending notices belong to the next exchange, so a
    /// summary never covers them.
    fn work_messages(
        &self,
        exchanges: &[Exchange],
        summary: Option<&Value>,
        failures: &Failures,
    ) -> Vec<Value> {
        let mut messages = self.messages(exchanges, summary, failures);
        messages.extend(self.notices.iter().cloned());
        messages
    }

    fn summary_messages(&self, older: usize, config: &CompactionConfig) -> Vec<Value> {
        if config.settings.recent_exchanges == 0 {
            // Nothing bounds the bytes of the next response, which joins this
            // summary. Extending the measured work prefix lets server usage
            // bound the request instead.
            let mut messages = self.messages(
                &self.exchanges[..older],
                self.summary.as_ref(),
                &self.failures,
            );
            messages.push(message("user", &config.settings.instructions));
            return messages;
        }
        let conversation: Vec<Value> = self
            .failures
            .message()
            .into_iter()
            .chain(
                self.exchanges[..older]
                    .iter()
                    .flat_map(|exchange| exchange.messages.clone()),
            )
            .collect();
        summary_transcript(
            &config.prompt,
            self.summary
                .as_ref()
                .and_then(|summary| summary["content"].as_str()),
            &conversation,
            &config.settings.instructions,
        )
    }

    pub fn initial_fits(&self) -> bool {
        self.context_policy.initial_fits(&self.root)
    }

    pub fn propose(&mut self, view: View<'_>, context: &ContextWindow) -> Proposal {
        match self.stage {
            Stage::Starting => Proposal::StartWorkspace,
            Stage::Ready => self.next_request(view, context),
            Stage::Calls => self
                .calls
                .pop_front()
                .map_or(Proposal::ResponseComplete, |call| {
                    Proposal::Tool(ToolRequest {
                        name: call["function"]["name"].as_str().unwrap().into(),
                        arguments: call["function"]["arguments"].clone(),
                    })
                }),
            Stage::Summary { .. } => unreachable!("summary has a pending model effect"),
            Stage::Failed(reason) => Proposal::Stop(reason),
            Stage::Published => Proposal::Finish(StopReason::AgentFinished),
        }
    }

    fn next_request(&mut self, view: View<'_>, context: &ContextWindow) -> Proposal {
        let history = CompactionHistory {
            policy: self,
            config: view.config.compaction.as_ref().unwrap(),
        };
        let facts = view.facts;
        let budget = Budget {
            remaining_tokens: facts.remaining_tokens,
            remaining_ms: facts.remaining_ms,
            model_ms: facts.model_ms,
            known_generated_tokens: facts.known_generated_tokens,
        };
        match self.context_policy.plan(&history, context, budget) {
            Plan::Work { messages, deferred } => {
                if let Some(deferred) = deferred {
                    self.observations.push_back((
                        "compaction_deferred",
                        json!({
                            "ordinal":self.ordinal + 1, "remaining_ms":facts.remaining_ms,
                            "expected_summary_ms":deferred.expected_ms,
                            "summary_input_bound":deferred.summary.input_tokens,
                            "summary_estimate_basis":deferred.summary.basis,
                            "input_bound":deferred.work.input_tokens,
                            "estimate_basis":deferred.work.basis,
                            "projected_tokens":deferred.projected_tokens,
                            "model_ms":facts.model_ms,
                            "known_generated_tokens":facts.known_generated_tokens
                        }),
                    ));
                }
                Proposal::Infer(Inference {
                    purpose: RequestPurpose::Work,
                    messages,
                    tools: self.context_policy.tools().to_vec(),
                })
            }
            Plan::Summarize(plan) => {
                self.ordinal += 1;
                let older = plan.older_exchanges;
                self.observations.push_back(("compaction_planned", json!({
                    "ordinal":self.ordinal,
                    "source_requests":self.exchanges[..older].iter().map(|e| &e.request_id).collect::<Vec<_>>(),
                    "retained_requests":self.exchanges[older..].iter().map(|e| &e.request_id).collect::<Vec<_>>(),
                    "previous_summary":self.summary,
                    "before_messages":plan.before_messages,
                    "input_bound":plan.work.input_tokens,
                    "estimate_basis":plan.work.basis,
                    "projected_tokens":plan.projected_tokens,
                    "summary_input_bound":plan.summary.input_tokens,
                    "summary_estimate_basis":plan.summary.basis,
                    "progress_input_bytes":plan.before_bytes,
                    "calibration":facts.calibration
                })));
                self.stage = Stage::Summary {
                    older,
                    before_bytes: plan.before_bytes,
                };
                Proposal::Infer(Inference {
                    purpose: RequestPurpose::Compaction,
                    messages: plan.messages,
                    tools: Vec::new(),
                })
            }
            Plan::Stop {
                reason,
                detail: Some(detail),
            } => self.reject(reason, detail),
            Plan::Stop {
                reason,
                detail: None,
            } => Proposal::Stop(reason),
        }
    }

    fn reject(&mut self, reason: StopReason, detail: &'static str) -> Proposal {
        self.observations.push_back((
            "compaction_rejected",
            json!({"ordinal":self.ordinal,"reason":reason,"detail":detail}),
        ));
        self.stage = Stage::Failed(reason);
        Proposal::Stop(reason)
    }

    pub fn model_response(
        &mut self,
        view: View<'_>,
        request_id: RequestId,
        response: ModelResponse,
    ) {
        if let Stage::Summary {
            older,
            before_bytes,
        } = self.stage
        {
            let summary = response.message["content"].as_str().unwrap_or("");
            if summary.trim().is_empty() {
                self.reject(
                    StopReason::CompactionFailed,
                    "summary has no final-answer content",
                );
                return;
            }
            let candidate = message(
                "assistant",
                format!(
                    "Derived memory from earlier exchanges; retain uncertainty and verify claims:\n{summary}"
                ),
            );
            let mut failures = self.failures.clone();
            for exchange in &self.exchanges[..older] {
                failures.extend(&exchange.failures);
            }
            let after = self.work_messages(&self.exchanges[older..], Some(&candidate), &failures);
            let replacement =
                match self
                    .context_policy
                    .replacement(before_bytes, &after, view.facts.calibration)
                {
                    Ok(replacement) => replacement,
                    Err(error) => {
                        self.reject(StopReason::CompactionNoProgress, error.0);
                        return;
                    }
                };
            self.observations.push_back((
                "compaction_applied",
                json!({
                    "ordinal":self.ordinal, "summary_request_id":request_id,
                    "summary":summary, "messages":after, "input_bound":replacement.input_tokens,
                    "estimate_basis":replacement.basis, "input_bytes":replacement.input_bytes,
                    "projected_tokens":replacement.projected_tokens, "before_progress_input_bytes":before_bytes
                }),
            ));
            self.exchanges.drain(..older);
            self.summary = Some(candidate);
            self.failures = failures;
            self.stage = Stage::Ready;
        } else {
            self.calls = response.message["tool_calls"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into();
            self.current = Some(Exchange {
                request_id,
                messages: self.notices.drain(..).chain([response.message]).collect(),
                failures: std::mem::take(&mut self.pending_failures),
            });
            self.stage = Stage::Calls;
        }
    }

    pub fn fit_result(&self, view: View<'_>, name: &str, original: &Value) -> Value {
        let used: u64 = self
            .current
            .as_ref()
            .unwrap()
            .messages
            .iter()
            .filter(|message| message["role"] == "tool")
            .map(encoded_size)
            .sum();
        let available = view
            .config
            .parameters
            .limits
            .output_bytes
            .get()
            .saturating_sub(used);
        let quota = available / (self.calls.len() as u64 + 1);
        fit_tool_result_with_notice(
            name,
            original.clone(),
            quota,
            "Tool output shortened to fit the exchange allowance.",
        )
    }
    pub fn tool_result(
        &mut self,
        tool: &ToolRequest,
        original: &Value,
        result: &Value,
        source: ToolResultSource,
    ) {
        let exchange = self.current.as_mut().unwrap();
        exchange
            .failures
            .observe(&exchange.request_id, tool, original, source);
        exchange.messages.push(tool_message(&tool.name, result));
    }
    /// A parse failure is retried, as in memento, for a work request or a
    /// summary. A summary answered with tool calls is retried the same way:
    /// parsed or not, the model took the summary request for a work turn.
    /// Other summary failures still stop.
    pub fn retries(&self, view: View<'_>, error: &ModelFailure) -> bool {
        let recoverable = match error {
            ModelFailure::Parse(_) => matches!(self.stage, Stage::Ready | Stage::Summary { .. }),
            ModelFailure::SummaryTools(_) => matches!(self.stage, Stage::Summary { .. }),
            ModelFailure::ToolBatchLimit(_) => matches!(self.stage, Stage::Ready),
            _ => false,
        };
        recoverable && view.facts.parse_streak <= crate::compaction::TOOL_PARSE_RETRIES
    }
    /// Work retries include bounded rejected-attempt data. Summary retries
    /// preserve their tool-free input and compaction ordinal.
    pub fn retry(&mut self, error: &ModelFailure) {
        if let Stage::Summary { .. } = self.stage {
            // The compaction is planned again, so its checks see the failed
            // attempt's charge, and keeps its ordinal. The work notice would
            // describe a response the next exchange never had.
            self.ordinal -= 1;
        } else {
            self.notices.push(
                self.retry_attempt
                    .take()
                    .unwrap_or_else(|| message("user", parse_retry_notice(error))),
            );
        }
    }
    pub fn model_error(
        &mut self,
        request: &crate::RequestContext,
        error: &ModelFailure,
        source: ModelErrorSource,
    ) {
        self.pending_failures.observe_model(request, error, source);
        if request.purpose == RequestPurpose::Work {
            self.retry_attempt = super::retry_attempt::message(request, error);
        }
    }
    pub fn after_response(&mut self, view: View<'_>) {
        // A retried request left no response, so it adds no exchange.
        if let Some(exchange) = self.current.take() {
            if exchange.messages.last().unwrap()["role"] == "assistant" {
                let config = view.config.compaction.as_ref().unwrap();
                self.notices.push(message(
                    "user",
                    if config.settings.allow_finish_run {
                        CONTINUE_NOTICE
                    } else {
                        CONTINUE_TO_LIMIT_NOTICE
                    },
                ));
            }
            self.exchanges.push(exchange);
        }
        self.stage = Stage::Ready;
    }
    pub fn started(&mut self) {
        self.stage = Stage::Ready;
    }
    pub fn published(&mut self) {
        self.stage = Stage::Published;
    }
    pub fn stopped(&mut self) {
        self.calls.clear();
    }
    pub fn take_observations(&mut self) -> Vec<(&'static str, Value)> {
        self.observations.drain(..).collect()
    }
}

/// Supplies complete exchanges and preserved facts to the shared planner.
struct CompactionHistory<'a> {
    policy: &'a Compaction,
    config: &'a CompactionConfig,
}

impl History for CompactionHistory<'_> {
    fn complete_exchanges(&self) -> usize {
        self.policy.exchanges.len()
    }

    fn work_messages(&self) -> Vec<Value> {
        self.policy.work_messages(
            &self.policy.exchanges,
            self.policy.summary.as_ref(),
            &self.policy.failures,
        )
    }

    fn summary_messages(&self, older: usize) -> Vec<Value> {
        self.policy.summary_messages(older, self.config)
    }
}
