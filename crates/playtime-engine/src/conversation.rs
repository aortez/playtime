//! Context planning shared by experiment policies and future interactive callers.
//!
//! This component has no workspace, archive or inference-server dependency. It
//! decides when complete exchanges need a summary and validates the replacement.
//! The caller still owns dispatch, usage settlement, deadlines and tool execution.

use playtime_core::{CommonLimits, InvalidValue, StopReason};
use serde_json::{Value, json};

use crate::{
    compaction::{CompactionSettings, work_reserve},
    context::{calibrated_tokens, encoded_size},
};

pub use crate::context::{Calibration, ContextWindow};

const PROMPT_SPEEDUP: u64 = 10;
const PACE_MINIMUM_MS: u64 = 10_000;

/// A caller-owned transcript. An exchange includes an assistant response and
/// all its tool results; a pending tool batch must never appear in this view.
/// System/task instructions and pending user input remain in `work_messages`.
pub trait History {
    fn complete_exchanges(&self) -> usize;
    fn work_messages(&self) -> Vec<Value>;
    /// Summarize the oldest `older` complete exchanges, with the original task,
    /// previous summary and summary instructions included by the caller.
    fn summary_messages(&self, older: usize) -> Vec<Value>;
}

/// Runtime observations, not grants to spend tokens or execute effects.
#[derive(Clone, Copy, Debug)]
pub struct Budget {
    pub remaining_tokens: u64,
    pub remaining_ms: u64,
    pub model_ms: u64,
    pub known_generated_tokens: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Estimate {
    pub input_tokens: u64,
    pub basis: &'static str,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DeferredSummary {
    pub expected_ms: u64,
    pub work: Estimate,
    pub projected_tokens: u64,
    pub summary: Estimate,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SummaryPlan {
    pub older_exchanges: usize,
    pub before_messages: Vec<Value>,
    pub messages: Vec<Value>,
    pub work: Estimate,
    pub projected_tokens: u64,
    pub summary: Estimate,
    pub before_bytes: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Plan {
    Work {
        messages: Vec<Value>,
        deferred: Option<DeferredSummary>,
    },
    Summarize(SummaryPlan),
    Stop {
        reason: StopReason,
        /// Token exhaustion alone needs no compaction rejection observation.
        detail: Option<&'static str>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Replacement {
    pub input_bytes: u64,
    pub input_tokens: u64,
    pub basis: &'static str,
    pub projected_tokens: u64,
}

/// Frozen context configuration. Both input estimation and progress validation
/// use these exact tools and allowances; the caller must not swap them mid-plan.
pub struct ContextPolicy {
    limits: CommonLimits,
    settings: CompactionSettings,
    tools: Vec<Value>,
}

impl ContextPolicy {
    pub fn new(
        limits: CommonLimits,
        settings: CompactionSettings,
        tools: Vec<Value>,
    ) -> Result<Self, InvalidValue> {
        settings.validate_limits(&limits)?;
        Ok(Self {
            limits,
            settings,
            tools,
        })
    }

    pub fn tools(&self) -> &[Value] {
        &self.tools
    }

    pub fn initial_fits(&self, messages: &[Value]) -> bool {
        let input = encoded_size(&json!({"messages":messages, "tools":self.tools}));
        let projected = input.saturating_add(self.limits.tokens_per_call.get());
        projected < self.settings.threshold(self.limits.context_tokens.get())
            && self.fits_reserve(projected)
    }

    pub fn plan(&self, history: &impl History, context: &ContextWindow, budget: Budget) -> Plan {
        if budget.remaining_tokens == 0 {
            return Plan::Stop {
                reason: StopReason::TokenLimit,
                detail: None,
            };
        }
        let messages = history.work_messages();
        let work = estimate(context, &messages, &self.tools);
        let allowance = self
            .limits
            .tokens_per_call
            .get()
            .min(budget.remaining_tokens);
        let projected = work.input_tokens.saturating_add(allowance);
        let fits_reserve = self.fits_reserve(projected);
        let fits_now =
            projected < self.settings.threshold(self.limits.context_tokens.get()) && fits_reserve;
        let summary_fits_after_work =
            self.summary_fits_after_next_work(history, context.calibration());
        if fits_now && summary_fits_after_work {
            return Plan::Work {
                messages,
                deferred: None,
            };
        }
        let older = history
            .complete_exchanges()
            .saturating_sub(usize::from(self.settings.recent_exchanges));
        // Only the preference trigger can be deferred. Deferring a hard fit
        // boundary could make the next exchange too large to summarize at all.
        if older > 0 && fits_reserve && summary_fits_after_work {
            let summary = estimate(context, &history.summary_messages(older), &[]);
            if let Some(expected_ms) = self.summary_cannot_finish(budget, summary.input_tokens) {
                return Plan::Work {
                    messages,
                    deferred: Some(DeferredSummary {
                        expected_ms,
                        work,
                        projected_tokens: projected,
                        summary,
                    }),
                };
            }
        }
        if older == 0 {
            return rejected(
                StopReason::ContextLimit,
                "preserved context leaves no older complete exchange to compact",
            );
        }
        if budget.remaining_tokens
            < self
                .settings
                .summary_tokens
                .get()
                .saturating_add(self.limits.tokens_per_call.get())
        {
            return rejected(
                StopReason::TokenLimit,
                "insufficient budget for summary and continued work",
            );
        }
        let summary_messages = history.summary_messages(older);
        let summary = estimate(context, &summary_messages, &[]);
        if !self.summary_fits(summary.input_tokens) {
            return rejected(
                StopReason::ContextLimit,
                "summary request cannot fit with its allowance and safety margin",
            );
        }
        Plan::Summarize(SummaryPlan {
            older_exchanges: older,
            before_bytes: encoded_size(&json!({"messages":messages,"tools":self.tools})),
            before_messages: messages,
            messages: summary_messages,
            work,
            projected_tokens: projected,
            summary,
        })
    }

    /// Check the caller's proposed replacement before it removes any history.
    /// The caller must retain the root instructions, complete recent exchanges
    /// and recorded failure facts, and accept only a normally completed summary.
    pub fn replacement(
        &self,
        before_bytes: u64,
        messages: &[Value],
        calibration: Option<Calibration>,
    ) -> Result<Replacement, InvalidValue> {
        let input_bytes = encoded_size(&json!({"messages":messages,"tools":self.tools}));
        let (input_tokens, basis) = calibrated_tokens(input_bytes, calibration);
        let projected_tokens = input_tokens.saturating_add(self.limits.tokens_per_call.get());
        if input_bytes >= before_bytes
            || projected_tokens >= self.settings.threshold(self.limits.context_tokens.get())
            || !self.fits_reserve(projected_tokens)
        {
            return Err(InvalidValue(
                "replacement does not shrink below the trigger with room for continued work",
            ));
        }
        Ok(Replacement {
            input_bytes,
            input_tokens,
            basis,
            projected_tokens,
        })
    }

    fn fits_reserve(&self, projected: u64) -> bool {
        projected.saturating_add(work_reserve(&self.limits, &self.settings))
            < self.limits.context_tokens.get()
    }

    fn summary_fits(&self, input: u64) -> bool {
        input
            .saturating_add(self.settings.summary_tokens.get())
            .saturating_add(CompactionSettings::safety_margin(
                self.limits.context_tokens.get(),
            ))
            < self.limits.context_tokens.get()
    }

    fn summary_fits_after_next_work(
        &self,
        history: &impl History,
        calibration: Option<Calibration>,
    ) -> bool {
        let retained = usize::from(self.settings.recent_exchanges);
        if retained == 0 {
            return true; // The work reserve covers the measured prefix continuation.
        }
        // The next response stays in the retained suffix. The older prefix is
        // known now, but dropping that suffix invalidates its measured anchor.
        let older = history
            .complete_exchanges()
            .saturating_add(1)
            .saturating_sub(retained);
        let messages = history.summary_messages(older);
        let input = calibrated_tokens(
            encoded_size(&json!({"messages":messages,"tools":[]})),
            calibration,
        )
        .0;
        self.summary_fits(input)
    }

    fn summary_cannot_finish(&self, budget: Budget, input: u64) -> Option<u64> {
        if budget.model_ms < PACE_MINIMUM_MS || budget.known_generated_tokens == 0 {
            return None;
        }
        // Use the whole summary cap and conservative prompt throughput, as the
        // experiment controller does. This estimate is not a deadline guarantee.
        let ms_per_token = budget.model_ms.div_ceil(budget.known_generated_tokens);
        let expected_ms = input
            .saturating_mul(ms_per_token)
            .div_ceil(PROMPT_SPEEDUP)
            .saturating_add(
                self.settings
                    .summary_tokens
                    .get()
                    .saturating_mul(ms_per_token),
            );
        (budget.remaining_ms < expected_ms).then_some(expected_ms)
    }
}

fn estimate(context: &ContextWindow, messages: &[Value], tools: &[Value]) -> Estimate {
    let (input_tokens, basis) = context.estimate(messages, tools);
    Estimate {
        input_tokens,
        basis,
    }
}

fn rejected(reason: StopReason, detail: &'static str) -> Plan {
    Plan::Stop {
        reason,
        detail: Some(detail),
    }
}
