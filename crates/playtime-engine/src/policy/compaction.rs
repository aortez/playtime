//! Complete-exchange compaction policy. It proposes work; the controller alone
//! admits requests, settles usage, dispatches tools and accepts publication.

use std::collections::VecDeque;

use playtime_core::{RequestId, RequestPurpose, StopReason};
use serde_json::{Value, json};

use super::{Inference, Proposal, View};
use crate::{
    ModelFailure, ModelResponse, ToolRequest,
    compaction::{CompactionConfig, CompactionSettings, summary_transcript},
    context::{
        Calibration, ContextWindow, calibrated_tokens, encoded_size, fit_tool_result_with_notice,
        message, tool_message,
    },
};

/// Prompt processing is assumed at least this many times faster than
/// generation when estimating how long a summary request takes. Batched
/// prompt evaluation ran 18 to 30 times faster than decoding on the Framework
/// and on an RTX 4090; the estimate stays pessimistic without hardware facts.
const PROMPT_SPEEDUP: u64 = 10;
/// Pace estimates need this much measured model time before a compaction can
/// be deferred for lack of wall time.
const PACE_MINIMUM_MS: u64 = 10_000;

/// Memento's parse retry notice, without its budget sentence: continuous
/// requests carry no budget.
const PARSE_RETRY_NOTICE: &str = "The server rejected your last response because a tool call could not be parsed. None of that response's tools ran. Try again with valid JSON object arguments.";
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
    stage: Stage,
    root: Vec<Value>,
    summary: Option<Value>,
    exchanges: Vec<Exchange>,
    current: Option<Exchange>,
    /// Notices for the next work request. They open its exchange.
    notices: Vec<Value>,
    calls: VecDeque<Value>,
    ordinal: u64,
    observations: VecDeque<(&'static str, Value)>,
}

impl Compaction {
    pub fn new(config: &CompactionConfig) -> Self {
        Self {
            stage: Stage::Starting,
            root: vec![
                message(
                    "system",
                    format!("{}\n\n{}", config.protocol, config.inventory),
                ),
                message("user", &config.prompt),
            ],
            summary: None,
            exchanges: Vec::new(),
            current: None,
            notices: Vec::new(),
            calls: VecDeque::new(),
            ordinal: 0,
            observations: VecDeque::new(),
        }
    }

    fn messages(&self, exchanges: &[Exchange], summary: Option<&Value>) -> Vec<Value> {
        let mut messages = self.root.clone();
        messages.extend(summary.cloned());
        messages.extend(
            exchanges
                .iter()
                .flat_map(|exchange| exchange.messages.clone()),
        );
        messages
    }

    /// The next work input. Pending notices belong to the next exchange, so a
    /// summary never covers them.
    fn work_messages(&self, exchanges: &[Exchange], summary: Option<&Value>) -> Vec<Value> {
        let mut messages = self.messages(exchanges, summary);
        messages.extend(self.notices.iter().cloned());
        messages
    }

    fn summary_messages(&self, older: usize, config: &CompactionConfig) -> Vec<Value> {
        if config.settings.recent_exchanges == 0 {
            // Nothing bounds the bytes of the next response, which joins this
            // summary. Extending the measured work prefix lets server usage
            // bound the request instead.
            let mut messages = self.messages(&self.exchanges[..older], self.summary.as_ref());
            messages.push(message("user", &config.settings.instructions));
            return messages;
        }
        let conversation: Vec<Value> = self.exchanges[..older]
            .iter()
            .flat_map(|exchange| exchange.messages.clone())
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

    fn summary_fits_after_next_work(
        &self,
        config: &CompactionConfig,
        calibration: Option<Calibration>,
    ) -> bool {
        let retained = usize::from(config.settings.recent_exchanges);
        if retained == 0 {
            // Without a retained suffix, summarization extends the measured
            // prefix. The ordinary work reserve covers that continuation.
            return true;
        }
        // The next response and its tool results will be retained. Only the
        // oldest retained exchange can enter the next summary, and it is
        // already complete. Dropping the new suffix invalidates the next
        // server measurement, even if this prefix is measured right now, so
        // the transcript is estimated from its bytes at the calibrated rate.
        let older = (self.exchanges.len() + 1).saturating_sub(retained);
        let messages = self.summary_messages(older, config);
        calibrated_tokens(
            encoded_size(&json!({"messages":messages,"tools":[]})),
            calibration,
        )
        .0
        .saturating_add(config.settings.summary_tokens.get())
        .saturating_add(CompactionSettings::safety_margin(
            config.parameters.limits.context_tokens.get(),
        )) < config.parameters.limits.context_tokens.get()
    }

    pub fn initial_fits(&self, config: &CompactionConfig) -> bool {
        let tools = crate::compaction::tools(config.settings.allow_finish_run);
        let input = encoded_size(&json!({"messages":self.root, "tools":tools}));
        let projected = input.saturating_add(config.parameters.limits.tokens_per_call.get());
        projected
            < config
                .settings
                .threshold(config.parameters.limits.context_tokens.get())
            && projected.saturating_add(config.work_reserve())
                < config.parameters.limits.context_tokens.get()
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
        let config = view.config.compaction.as_ref().unwrap();
        let limits = &config.parameters.limits;
        if view.facts.remaining_tokens == 0 {
            return Proposal::Stop(StopReason::TokenLimit);
        }
        let tools = crate::compaction::tools(config.settings.allow_finish_run);
        let messages = self.work_messages(&self.exchanges, self.summary.as_ref());
        let (input, basis) = context.estimate(&messages, &tools);
        let allowance = limits
            .tokens_per_call
            .get()
            .min(view.facts.remaining_tokens);
        let projected = input.saturating_add(allowance);
        // The trigger is where compaction is preferred; the reserve is where
        // work stops fitting.
        let fits_reserve =
            projected.saturating_add(config.work_reserve()) < limits.context_tokens.get();
        let fits_now =
            projected < config.settings.threshold(limits.context_tokens.get()) && fits_reserve;
        let summary_fits_after_work =
            self.summary_fits_after_next_work(config, view.facts.calibration);
        if fits_now && summary_fits_after_work {
            return Proposal::Infer(Inference {
                purpose: RequestPurpose::Work,
                messages,
                tools,
            });
        }
        let older = self
            .exchanges
            .len()
            .saturating_sub(usize::from(config.settings.recent_exchanges));
        // A summary the wall deadline cannot hold produces nothing and is
        // charged its whole allowance, so the run keeps working past the
        // trigger instead. Only the trigger is deferred: the reserve and the
        // summary-fit checks compact whatever the time left, since deferring
        // them lets work outgrow the summary, and the run would then stop at
        // its context limit with nothing to recover.
        if older > 0 && fits_reserve && summary_fits_after_work {
            let summary_messages = self.summary_messages(older, config);
            let (summary_input, summary_basis) = context.estimate(&summary_messages, &[]);
            if let Some(expected_ms) = self.summary_cannot_finish(view, summary_input) {
                self.observations.push_back((
                    "compaction_deferred",
                    json!({
                        "ordinal":self.ordinal + 1, "remaining_ms":view.facts.remaining_ms,
                        "expected_summary_ms":expected_ms, "summary_input_bound":summary_input,
                        "summary_estimate_basis":summary_basis, "input_bound":input,
                        "estimate_basis":basis, "projected_tokens":projected,
                        "model_ms":view.facts.model_ms,
                        "known_generated_tokens":view.facts.known_generated_tokens
                    }),
                ));
                return Proposal::Infer(Inference {
                    purpose: RequestPurpose::Work,
                    messages,
                    tools,
                });
            }
        }
        if older == 0 {
            return self.reject(
                StopReason::ContextLimit,
                "preserved context leaves no older complete exchange to compact",
            );
        }
        if view.facts.remaining_tokens
            < config
                .settings
                .summary_tokens
                .get()
                .saturating_add(limits.tokens_per_call.get())
        {
            return self.reject(
                StopReason::TokenLimit,
                "insufficient budget for summary and continued work",
            );
        }
        let summary_messages = self.summary_messages(older, config);
        let (summary_input, summary_basis) = context.estimate(&summary_messages, &[]);
        if summary_input
            .saturating_add(config.settings.summary_tokens.get())
            .saturating_add(CompactionSettings::safety_margin(
                limits.context_tokens.get(),
            ))
            >= limits.context_tokens.get()
        {
            return self.reject(
                StopReason::ContextLimit,
                "summary request cannot fit with its allowance and safety margin",
            );
        }
        let before_bytes = encoded_size(&json!({"messages":messages, "tools":tools}));
        self.ordinal += 1;
        self.observations.push_back(("compaction_planned", json!({
            "ordinal":self.ordinal,
            "source_requests":self.exchanges[..older].iter().map(|e| &e.request_id).collect::<Vec<_>>(),
            "retained_requests":self.exchanges[older..].iter().map(|e| &e.request_id).collect::<Vec<_>>(),
            "previous_summary":self.summary,
            "before_messages":messages,
            "input_bound":input, "estimate_basis":basis, "projected_tokens":projected,
            "summary_input_bound":summary_input, "summary_estimate_basis":summary_basis,
            "progress_input_bytes":before_bytes, "calibration":view.facts.calibration
        })));
        self.stage = Stage::Summary {
            older,
            before_bytes,
        };
        Proposal::Infer(Inference {
            purpose: RequestPurpose::Compaction,
            messages: summary_messages,
            tools: Vec::new(),
        })
    }

    /// The milliseconds a summary request is expected to take, when the run's
    /// measured pace says it cannot finish before the wall deadline. The
    /// generation pace counts the model time of requests with known usage
    /// against their tokens, and the summary is assumed to use its whole cap,
    /// so the estimate errs long.
    fn summary_cannot_finish(&self, view: View<'_>, summary_input: u64) -> Option<u64> {
        let facts = view.facts;
        if facts.model_ms < PACE_MINIMUM_MS || facts.known_generated_tokens == 0 {
            return None;
        }
        let ms_per_token = facts.model_ms.div_ceil(facts.known_generated_tokens);
        let config = view.config.compaction.as_ref().unwrap();
        let expected_ms = summary_input
            .saturating_mul(ms_per_token)
            .div_ceil(PROMPT_SPEEDUP)
            .saturating_add(
                config
                    .settings
                    .summary_tokens
                    .get()
                    .saturating_mul(ms_per_token),
            );
        (facts.remaining_ms < expected_ms).then_some(expected_ms)
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
            let config = view.config.compaction.as_ref().unwrap();
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
            let after = self.work_messages(&self.exchanges[older..], Some(&candidate));
            let tools = crate::compaction::tools(config.settings.allow_finish_run);
            // Rewritten input cannot borrow server measurements of another
            // prefix. Progress compares before/after bytes on the same basis;
            // the room for continued work uses the calibrated token estimate.
            let after_bytes = encoded_size(&json!({"messages":after,"tools":tools}));
            let (after_tokens, after_basis) =
                calibrated_tokens(after_bytes, view.facts.calibration);
            let projected =
                after_tokens.saturating_add(config.parameters.limits.tokens_per_call.get());
            if after_bytes >= before_bytes
                || projected
                    >= config
                        .settings
                        .threshold(config.parameters.limits.context_tokens.get())
                || projected.saturating_add(config.work_reserve())
                    >= config.parameters.limits.context_tokens.get()
            {
                self.reject(
                    StopReason::CompactionNoProgress,
                    "replacement does not shrink below the trigger with room for continued work",
                );
                return;
            }
            self.observations.push_back((
                "compaction_applied",
                json!({
                    "ordinal":self.ordinal, "summary_request_id":request_id,
                    "summary":summary, "messages":after, "input_bound":after_tokens,
                    "estimate_basis":after_basis, "input_bytes":after_bytes,
                    "projected_tokens":projected, "before_progress_input_bytes":before_bytes
                }),
            ));
            self.exchanges.drain(..older);
            self.summary = Some(candidate);
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
    pub fn tool_result(&mut self, name: &str, result: &Value) {
        self.current
            .as_mut()
            .unwrap()
            .messages
            .push(tool_message(name, result));
    }
    /// A parse failure is retried, as in memento, for a work request or a
    /// summary. A summary answered with tool calls is retried the same way:
    /// parsed or not, the model took the summary request for a work turn.
    /// Other summary failures still stop.
    pub fn retries(&self, view: View<'_>, error: &ModelFailure) -> bool {
        let recoverable = match error {
            ModelFailure::Parse(_) => matches!(self.stage, Stage::Ready | Stage::Summary { .. }),
            ModelFailure::SummaryTools(_) => matches!(self.stage, Stage::Summary { .. }),
            _ => false,
        };
        recoverable && view.facts.parse_streak <= crate::compaction::TOOL_PARSE_RETRIES
    }
    pub fn retry(&mut self) {
        if let Stage::Summary { .. } = self.stage {
            // The compaction is planned again, so its checks see the failed
            // attempt's charge, and keeps its ordinal. The work notice would
            // describe a response the next exchange never had.
            self.ordinal -= 1;
        } else {
            self.notices.push(message("user", PARSE_RETRY_NOTICE));
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
