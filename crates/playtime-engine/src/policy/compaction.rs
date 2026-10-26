//! Complete-exchange compaction policy. It proposes work; the controller alone
//! admits requests, settles usage, dispatches tools and accepts publication.

use std::collections::VecDeque;

use playtime_core::{RequestId, RequestPurpose, StopReason};
use serde_json::{Value, json};

use super::{Inference, Proposal, View};
use crate::{
    ModelResponse, ToolRequest,
    compaction::{CompactionConfig, CompactionSettings},
    context::{ContextWindow, encoded_size, fit_tool_result_with_notice, message, tool_message},
};

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

    fn summary_messages(&self, older: usize, config: &CompactionConfig) -> Vec<Value> {
        let mut messages = self.messages(&self.exchanges[..older], self.summary.as_ref());
        messages.push(message("user", &config.settings.instructions));
        messages
    }

    fn summary_fits_after_next_work(&self, config: &CompactionConfig) -> bool {
        let retained = usize::from(config.settings.recent_exchanges);
        if retained == 0 {
            // Without a retained suffix, summarization extends the measured
            // prefix. The ordinary work reserve covers that continuation.
            return true;
        }
        // The next response and its tool results will be retained. Only the
        // oldest retained exchange can enter the next summary, and it is
        // already complete. Dropping the new suffix invalidates the next
        // server measurement, even if this prefix is measured right now.
        let older = (self.exchanges.len() + 1).saturating_sub(retained);
        let messages = self.summary_messages(older, config);
        encoded_size(&json!({"messages":messages,"tools":[]}))
            .saturating_add(config.settings.summary_tokens.get())
            .saturating_add(CompactionSettings::safety_margin(
                config.parameters.limits.context_tokens.get(),
            ))
            < config.parameters.limits.context_tokens.get()
    }

    pub fn initial_fits(&self, config: &CompactionConfig) -> bool {
        let input =
            encoded_size(&json!({"messages":self.root, "tools":crate::compaction::tools()}));
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
        let tools = crate::compaction::tools();
        let messages = self.messages(&self.exchanges, self.summary.as_ref());
        let (input, basis) = context.estimate(&messages, &tools);
        let allowance = limits
            .tokens_per_call
            .get()
            .min(view.facts.remaining_tokens);
        let projected = input.saturating_add(allowance);
        if projected < config.settings.threshold(limits.context_tokens.get())
            && projected.saturating_add(config.work_reserve()) < limits.context_tokens.get()
            && self.summary_fits_after_next_work(config)
        {
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
            "progress_input_bytes":before_bytes
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
            let after = self.messages(&self.exchanges[older..], Some(&candidate));
            let tools = crate::compaction::tools();
            // Rewritten input cannot borrow server measurements of another
            // prefix. Compare before/after bytes on the same conservative basis.
            let after_bytes = encoded_size(&json!({"messages":after,"tools":tools}));
            let projected =
                after_bytes.saturating_add(config.parameters.limits.tokens_per_call.get());
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
                    "summary":summary, "messages":after, "input_bound":after_bytes,
                    "estimate_basis":"input_bytes", "projected_tokens":projected,
                    "before_progress_input_bytes":before_bytes
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
                messages: vec![response.message],
            });
            self.stage = Stage::Calls;
        }
    }

    pub fn fit_result(&self, view: View<'_>, name: &str, original: &Value) -> Value {
        let used: u64 = self.current.as_ref().unwrap().messages[1..]
            .iter()
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
    pub fn after_response(&mut self) {
        self.exchanges.push(self.current.take().unwrap());
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
