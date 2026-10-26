mod admission;

use std::{collections::BTreeSet, num::NonZeroU64};

use playtime_core::{
    CommandOutcome, GenerationUsage, InvalidValue, RequestPurpose, RequestScope, RunId, RunMode,
    RunPhase, SessionId, StopReason, UsageTotals,
};
use playtime_protocol::{
    events::CleanupOutcome, observations::ObservationIdentity, v1::UsageSummary,
};
use serde_json::{Value, json};

use crate::{
    context::{ContextWindow, encoded_size},
    evidence::{CommandObservation, CommandSources},
    policy::{Inference, Memento, Policy, Recovery, View, memento_contract as contract},
    *,
};

const FINAL_LOG_RESERVE: u64 = 65536;
const CHECKPOINT_MS: u64 = 80000;
const LIFECYCLE_MS: u64 = 20000;
const MODEL_BYTES: u64 = 4 * 1024 * 1024;
const CHUNK_BYTES: u64 = 512 * 1024;

#[derive(Clone, Copy, PartialEq)]
enum Stage {
    Policy,
    Checkpoint,
    Publishing,
    Cleaning,
    Done,
}

#[derive(Clone)]
enum PublicationArtifact {
    Checkpoint(Box<Checkpoint>),
    Capture(Box<Capture>),
}
impl PublicationArtifact {
    fn effect(self) -> EffectKind {
        match self {
            Self::Checkpoint(checkpoint) => EffectKind::PublishCheckpoint(checkpoint),
            Self::Capture(capture) => EffectKind::PublishCapture(capture),
        }
    }
}

struct ActiveTool {
    request: ToolRequest,
    request_sequence: u64,
    command: Option<usize>,
    save_memory: bool,
    finish_run: bool,
    effect_id: Option<u64>,
}

/// Runtime enforcement and effect authority. The private memento policy proposes
/// conversation/lifecycle actions; adapters supply observations, never decisions
/// to retry, increase an allocation, or start another session.
pub struct Controller<I: IdSource> {
    config: RuntimeConfig,
    tool_policy: ToolPolicy,
    policy: Policy,
    run_id: RunId,
    mode: RunMode,
    ids: I,
    seen_sessions: BTreeSet<SessionId>,
    seen_requests: BTreeSet<playtime_core::RequestId>,
    started_ms: u64,
    now_ms: u64,
    deadline_ms: u64,
    stage: Stage,
    phase: RunPhase,
    pending: Option<Effect>,
    pending_started: bool,
    next_effect: u64,
    outbox: Vec<BoundRecord>,
    controls: Vec<ControlUpdate>,
    request_context: Option<RequestContext>,
    publication: Option<(u64, PublicationArtifact, bool)>,
    sequence: u64,
    log_used: u64,
    log_exhausted: bool,
    totals: UsageTotals,
    reservation: Option<NonZeroU64>,
    model_calls: u64,
    parse_errors: u32,
    session_parse_errors: u32,
    parse_streak: u32,
    response_bytes: u64,
    tool_result_bytes: u64,
    session: u32,
    session_id: Option<SessionId>,
    active: bool,
    checkpoint_attempted: bool,
    last_checkpoint: Option<Checkpoint>,
    last_capture: Option<Capture>,
    workspace_started: bool,
    context: ContextWindow,
    turns: u32,
    handoff_attempts: u32,
    current_handoff: bool,
    tool_calls_remaining: usize,
    tool: Option<ActiveTool>,
    commands: Vec<CommandObservation>,
    ended: bool,
    finished: bool,
    reason: Option<StopReason>,
    diagnostics: Vec<String>,
    result: Option<RunResult>,
}

impl<I: IdSource> Controller<I> {
    pub fn new(
        config: ControllerConfig,
        run_id: RunId,
        mode: RunMode,
        ids: I,
        now_ms: u64,
    ) -> Result<Self, InvalidValue> {
        Self::with_tool_policy(config, run_id, mode, ids, now_ms, ToolPolicy::Simulation)
    }

    pub fn with_tool_policy(
        config: ControllerConfig,
        run_id: RunId,
        mode: RunMode,
        ids: I,
        now_ms: u64,
        tool_policy: ToolPolicy,
    ) -> Result<Self, InvalidValue> {
        config.parameters.validate()?;
        Self::configured(
            RuntimeConfig::memento(config),
            Policy::Memento(Memento::default()),
            run_id,
            mode,
            ids,
            now_ms,
            tool_policy,
        )
    }

    #[cfg(feature = "test-harness")]
    pub fn test_workspace(
        config: WorkspaceConfig,
        run_id: RunId,
        ids: I,
        now_ms: u64,
    ) -> Result<Self, InvalidValue> {
        config.parameters.validate()?;
        if config.context_reserve >= config.parameters.limits.context_tokens.get() {
            return Err(InvalidValue("workspace context reserve exceeds window"));
        }
        Self::configured(
            RuntimeConfig::workspace(config),
            Policy::probe(),
            run_id,
            RunMode::Simulation,
            ids,
            now_ms,
            ToolPolicy::Simulation,
        )
    }

    #[cfg(feature = "test-harness")]
    pub fn test_compaction(
        config: crate::compaction::CompactionConfig,
        run_id: RunId,
        ids: I,
        now_ms: u64,
    ) -> Result<Self, InvalidValue> {
        config.settings.validate(&config.parameters)?;
        let policy = Policy::compaction(&config)?;
        Self::configured(
            RuntimeConfig::compaction(config),
            policy,
            run_id,
            RunMode::Simulation,
            ids,
            now_ms,
            ToolPolicy::Simulation,
        )
    }

    fn configured(
        config: RuntimeConfig,
        policy: Policy,
        run_id: RunId,
        mode: RunMode,
        ids: I,
        now_ms: u64,
        tool_policy: ToolPolicy,
    ) -> Result<Self, InvalidValue> {
        config.parameters.validate()?;
        if let ToolPolicy::ConfirmedExternal { return_overhead_ms } = tool_policy
            && (return_overhead_ms == 0
                || return_overhead_ms
                    >= config
                        .parameters
                        .limits
                        .command_seconds
                        .get()
                        .saturating_mul(1000))
        {
            return Err(InvalidValue("invalid external command return allowance"));
        }
        let limits = &config.parameters.limits;
        let deadline_ms = limits
            .wall_seconds
            .get()
            .checked_mul(1000)
            .and_then(|duration| now_ms.checked_add(duration))
            .ok_or(InvalidValue("wall deadline overflows milliseconds"))?;
        for seconds in [limits.model_seconds, limits.command_seconds] {
            seconds
                .get()
                .checked_mul(1000)
                .ok_or(InvalidValue("operation timeout overflows milliseconds"))?;
        }
        let mut controller = Self {
            config,
            tool_policy,
            policy,
            run_id,
            mode,
            ids,
            seen_sessions: BTreeSet::new(),
            seen_requests: BTreeSet::new(),
            started_ms: now_ms,
            now_ms,
            deadline_ms,
            stage: Stage::Policy,
            phase: RunPhase::Admitted,
            pending: None,
            pending_started: false,
            next_effect: 1,
            outbox: Vec::new(),
            controls: Vec::new(),
            request_context: None,
            publication: None,
            sequence: 1,
            log_used: 0,
            log_exhausted: false,
            totals: UsageTotals::default(),
            reservation: None,
            model_calls: 0,
            parse_errors: 0,
            session_parse_errors: 0,
            parse_streak: 0,
            response_bytes: 0,
            tool_result_bytes: 0,
            session: 0,
            session_id: None,
            active: false,
            checkpoint_attempted: false,
            last_checkpoint: None,
            last_capture: None,
            workspace_started: false,
            context: ContextWindow::default(),
            turns: 0,
            handoff_attempts: 0,
            current_handoff: false,
            tool_calls_remaining: 0,
            tool: None,
            commands: Vec::new(),
            ended: false,
            finished: false,
            reason: None,
            diagnostics: Vec::new(),
            result: None,
        };
        let parameters = controller.config.memento.as_ref().map_or_else(
            || json!(controller.config.parameters),
            |config| json!(config.parameters),
        );
        controller.emit(
            "controller_started",
            json!({"run_id": controller.run_id, "mode": mode, "parameters": parameters}),
        );
        if let Some(config) = &controller.config.compaction {
            controller.emit("compaction_configured", json!({
                "harness":crate::compaction::HARNESS,
                "settings":config.settings,
                "safety_margin":crate::compaction::CompactionSettings::safety_margin(config.parameters.limits.context_tokens.get()),
                "work_context_reserve":config.work_reserve()
            }));
        }
        controller.pump();
        Ok(controller)
    }

    pub fn pending(&self) -> Option<&Effect> {
        self.pending.as_ref()
    }
    pub fn next_wakeup_ms(&self) -> Option<u64> {
        self.pending.as_ref().map(|effect| {
            if self.reason.is_none() {
                effect.deadline_ms.min(self.deadline_ms)
            } else {
                effect.deadline_ms
            }
        })
    }
    pub fn result(&self) -> Option<&RunResult> {
        self.result.as_ref()
    }
    pub fn phase(&self) -> RunPhase {
        self.phase
    }
    pub fn terminal_decision(&self) -> Option<StopReason> {
        self.reason
    }
    pub fn usage(&self) -> UsageTotals {
        self.totals
    }
    pub fn reserved_tokens(&self) -> u64 {
        self.reservation.map_or(0, NonZeroU64::get)
    }
    pub fn take_records(&mut self) -> Vec<Record> {
        self.take_observations()
            .into_iter()
            .map(|entry| entry.record)
            .collect()
    }
    pub fn take_observations(&mut self) -> Vec<BoundRecord> {
        std::mem::take(&mut self.outbox)
    }
    pub fn take_control_updates(&mut self) -> Vec<ControlUpdate> {
        std::mem::take(&mut self.controls)
    }
    pub fn pending_info(&self) -> Option<EffectInfo> {
        self.pending.as_ref().map(|effect| self.effect_info(effect))
    }

    /// A mismatched/stale completion is rejected without changing state. The
    /// driver must still schedule ticks independently of completion delivery.
    pub fn advance(&mut self, now_ms: u64, input: Input) -> Result<(), InvalidValue> {
        if now_ms < self.now_ms {
            return Err(InvalidValue("monotonic clock moved backwards"));
        }
        if self.controls.len() > CONTROL_QUEUE_CAPACITY - CONTROL_TRANSITION_RESERVE {
            return Err(InvalidValue(
                "drain bounded control updates before advancing",
            ));
        }
        match &input {
            Input::PublicationCommitted { effect_id } => {
                if self
                    .publication
                    .as_ref()
                    .is_none_or(|(id, _, started)| id != effect_id || !started)
                {
                    return Err(InvalidValue(
                        "checkpoint receipt is unstarted, wrong, or already acknowledged",
                    ));
                }
            }
            Input::Dispatched { effect_id } => {
                if self.pending_started
                    || self
                        .pending
                        .as_ref()
                        .is_none_or(|effect| effect.id != *effect_id)
                {
                    return Err(InvalidValue("dispatch does not match an unstarted effect"));
                }
            }
            Input::Chunk { effect_id, .. } => {
                if !self.pending_started
                    || !matches!(self.pending.as_ref(), Some(Effect { id, kind: EffectKind::Model(_), .. }) if id == effect_id)
                {
                    return Err(InvalidValue("chunk does not match pending model effect"));
                }
            }
            Input::Complete { effect_id, outcome } => {
                let Some(effect) = &self.pending else {
                    return Err(InvalidValue("no pending effect"));
                };
                if !self.pending_started
                    || effect.id != *effect_id
                    || !Self::matches(&effect.kind, outcome)
                {
                    return Err(InvalidValue("completion does not match pending effect"));
                }
            }
            _ => {}
        }
        self.now_ms = now_ms;
        if self.stage == Stage::Done {
            if let Input::PublicationCommitted { effect_id } = input {
                self.publication_committed(effect_id);
                return Ok(());
            }
            if let Input::PersistenceFailed(error) = input {
                self.diagnostic(&format!("terminal event persistence: {error}"));
                self.result.as_mut().unwrap().diagnostics = self.diagnostics.clone();
            }
            return Ok(());
        }
        if matches!(input, Input::Cancel) {
            self.stop(StopReason::Interrupted);
        }
        if self.reason.is_none() && now_ms >= self.deadline_ms {
            self.stop(StopReason::WallLimit);
        }
        if self
            .pending
            .as_ref()
            .is_some_and(|effect| now_ms >= effect.deadline_ms)
        {
            self.timeout();
        }
        match input {
            Input::Dispatched { effect_id }
                if self
                    .pending
                    .as_ref()
                    .is_some_and(|effect| effect.id == effect_id) =>
            {
                self.pending_started = true;
                if let Some((id, _, started)) = &mut self.publication
                    && *id == effect_id
                {
                    *started = true;
                }
                if matches!(
                    self.pending.as_ref().unwrap().kind,
                    EffectKind::StartSession { .. } | EffectKind::StartWorkspace
                ) {
                    self.active = true;
                }
            }
            Input::Chunk { effect_id, chunk }
                if self
                    .pending
                    .as_ref()
                    .is_some_and(|effect| effect.id == effect_id) =>
            {
                let bytes = encoded_size(&chunk);
                self.response_bytes = self.response_bytes.saturating_add(bytes);
                if bytes > CHUNK_BYTES || self.response_bytes > MODEL_BYTES {
                    self.emit(
                        "model_chunk_rejected",
                        json!({"bytes": bytes, "response_bytes": self.response_bytes}),
                    );
                    self.model_error(ModelFailure::ByteLimit(
                        "bounded model stream exceeded its byte limit".into(),
                    ));
                } else {
                    self.emit(
                        "model_chunk",
                        json!({"call": self.model_calls, "chunk": chunk}),
                    );
                }
            }
            Input::Complete { effect_id, outcome }
                if self
                    .pending
                    .as_ref()
                    .is_some_and(|effect| effect.id == effect_id) =>
            {
                let effect = self.pending.take().unwrap();
                self.complete(effect.kind, outcome);
            }
            Input::PersistenceFailed(error) => {
                self.diagnostic(&format!("event persistence: {error}"));
                self.stop(StopReason::InfrastructureFailure);
            }
            Input::ExternalLogBudgetExhausted => self.stop(StopReason::LogLimit),
            Input::PublicationCommitted { effect_id } => self.publication_committed(effect_id),
            _ => {}
        }
        self.pump();
        Ok(())
    }

    fn matches(effect: &EffectKind, outcome: &Outcome) -> bool {
        matches!(
            (effect, outcome),
            (_, Outcome::Failed(_))
                | (EffectKind::StartWorkspace, Outcome::WorkspaceStarted)
                | (EffectKind::CaptureWorkspace, Outcome::WorkspaceCaptured(_))
                | (EffectKind::PublishCapture(_), Outcome::Published)
                | (EffectKind::StartSession { .. }, Outcome::Started(_))
                | (
                    EffectKind::Model(_),
                    Outcome::Model(_) | Outcome::ModelCompleted { .. } | Outcome::ModelError(_)
                )
                | (EffectKind::Tool(_), Outcome::Tool(_))
                | (EffectKind::Snapshot { .. }, Outcome::Snapshot(_))
                | (EffectKind::PublishCheckpoint(_), Outcome::Published)
                | (EffectKind::Cleanup, Outcome::Cleaned(_))
        )
    }

    fn emit(&mut self, kind: &str, data: Value) -> Option<u64> {
        let identity = if kind == "model_request" {
            ObservationIdentity::default()
        } else if kind.starts_with("model_")
            || kind.starts_with("request_")
            || matches!(
                kind,
                "compaction_applied" | "completion_declared" | "tool_batch_rejected"
            )
        {
            self.request_identity()
        } else if kind.starts_with("tool_") || kind == "memento_write" {
            let mut identity = self.request_identity();
            identity.effect_id = self
                .tool
                .as_ref()
                .and_then(|tool| tool.effect_id)
                .and_then(NonZeroU64::new);
            identity.tool_request_sequence = if kind == "tool_request" {
                NonZeroU64::new(self.sequence)
            } else {
                self.tool
                    .as_ref()
                    .and_then(|tool| NonZeroU64::new(tool.request_sequence))
            };
            identity
        } else {
            ObservationIdentity::default()
        };
        self.emit_bound(kind, data, identity)
    }

    fn emit_bound(
        &mut self,
        kind: &str,
        data: Value,
        identity: ObservationIdentity,
    ) -> Option<u64> {
        let record = Record {
            sequence: self.sequence,
            elapsed_ms: self.now_ms - self.started_ms,
            session_id: self.session_id.clone(),
            kind: kind.into(),
            data,
        };
        let size = encoded_size(&json!(record)) + 1;
        let ceiling = if self.reason.is_some()
            || matches!(
                self.stage,
                Stage::Checkpoint | Stage::Publishing | Stage::Cleaning | Stage::Done
            ) {
            self.config.parameters.limits.log_bytes.get()
        } else {
            self.config
                .parameters
                .limits
                .log_bytes
                .get()
                .saturating_sub(FINAL_LOG_RESERVE)
        };
        if size > ceiling.saturating_sub(self.log_used) {
            self.log_exhausted = true;
            self.control(ControlFact::ObservationOmitted {
                kind: kind.into(),
                size_bytes: size,
                request: self.bound_request(&identity),
                identity,
            });
            return None;
        }
        self.log_used += size;
        self.sequence += 1;
        self.outbox.push(BoundRecord {
            record,
            request: self.bound_request(&identity),
            identity,
        });
        Some(self.sequence - 1)
    }

    fn phase_to(&mut self, phase: RunPhase) {
        if self.phase != phase {
            self.phase = phase;
            self.control(ControlFact::PhaseChanged(phase));
            self.emit(
                "phase_changed",
                json!({"phase": phase, "session_id": self.session_id}),
            );
        }
    }

    fn diagnostic(&mut self, text: &str) {
        if self.diagnostics.len() < 8 {
            self.diagnostics.push(text.chars().take(512).collect());
        }
    }

    fn effect(&mut self, kind: EffectKind, duration_ms: u64, work: bool) {
        let mut deadline_ms = self.now_ms.saturating_add(duration_ms);
        if work {
            deadline_ms = deadline_ms.min(self.deadline_ms);
        }
        self.pending = Some(Effect {
            id: self.next_effect,
            deadline_ms,
            kind,
        });
        self.pending_started = false;
        self.next_effect += 1;
        self.control(ControlFact::EffectPending(self.pending_info().unwrap()));
        if let Some(Effect {
            id,
            kind: EffectKind::PublishCheckpoint(checkpoint),
            ..
        }) = &self.pending
        {
            self.publication = Some((
                *id,
                PublicationArtifact::Checkpoint(checkpoint.clone()),
                false,
            ));
        }
        if let Some(Effect {
            id,
            kind: EffectKind::PublishCapture(capture),
            ..
        }) = &self.pending
        {
            self.publication = Some((*id, PublicationArtifact::Capture(capture.clone()), false));
        }
    }

    fn stop(&mut self, reason: StopReason) {
        if self.reason.is_some() {
            return;
        }
        let undispatched = (!self.pending_started)
            .then(|| self.pending_info())
            .flatten();
        self.reason = Some(reason);
        self.settle_unknown();
        self.undispatched_tool();
        self.emit(
            "controller_stop",
            json!({"reason": reason, "usage": UsageSummary::from(self.totals)}),
        );
        self.control(ControlFact::StopDecided(reason));
        self.policy.stopped();
        self.tool = None;
        if matches!(
            self.pending.as_ref().map(|effect| &effect.kind),
            Some(
                EffectKind::Snapshot { .. }
                    | EffectKind::CaptureWorkspace
                    | EffectKind::PublishCapture(_)
                    | EffectKind::PublishCheckpoint(_)
                    | EffectKind::Cleanup
            )
        ) {
            return;
        }
        if let Some(effect) = undispatched {
            self.control(ControlFact::EffectNotDispatched(effect));
        }
        self.pending = None;
        self.stage = if self.active && !self.checkpoint_attempted {
            Stage::Checkpoint
        } else {
            Stage::Cleaning
        };
    }

    fn settle_unknown(&mut self) {
        if let Some(reserved_tokens) = self.reservation.take() {
            let request = self
                .request_context
                .clone()
                .expect("reservation has request identity");
            if !self.pending_started {
                self.control(ControlFact::RequestNotDispatched {
                    request,
                    released_tokens: reserved_tokens.get(),
                });
                self.emit(
                    "request_not_dispatched",
                    json!({"call": self.model_calls, "released_tokens": reserved_tokens}),
                );
                return;
            }
            // Admission guarantees this sum fits the original allocation.
            self.totals
                .record(GenerationUsage::Unknown { reserved_tokens })
                .expect("admitted reservation fits");
            self.control(ControlFact::RequestSettled {
                request,
                usage: GenerationUsage::Unknown { reserved_tokens },
                totals: self.totals.into(),
            });
            self.emit("request_settled", json!({"call": self.model_calls,
                "usage": GenerationUsage::Unknown { reserved_tokens }, "totals": UsageSummary::from(self.totals)}));
        }
    }

    fn undispatched_tool(&mut self) {
        if !self.pending_started
            && let Some(tool) = self.tool.as_mut()
        {
            if let Some(index) = tool.command.take() {
                // This command was admitted but never crossed the dispatch
                // boundary. It cannot appear as an observation in evidence.
                self.commands.remove(index);
            }
            let sequence = tool.request_sequence;
            self.emit("tool_not_dispatched", json!({"request_sequence": sequence}));
        }
    }

    fn pump(&mut self) {
        loop {
            for (kind, data) in self.policy.take_observations() {
                self.emit(kind, data);
            }
            if self.log_exhausted && self.reason.is_none() {
                self.stop(StopReason::LogLimit);
            }
            if self.pending.is_some() || self.stage == Stage::Done {
                return;
            }
            match self.stage {
                Stage::Policy => {
                    let proposal = self.policy.propose(
                        View {
                            config: &self.config,
                            facts: self.facts(),
                        },
                        &self.context,
                    );
                    if let Err(error) = self.apply_proposal(proposal) {
                        self.diagnostic(error.0);
                        self.stop(StopReason::InfrastructureFailure);
                    }
                }
                Stage::Checkpoint => {
                    self.checkpoint_attempted = true;
                    if let Some(session_id) = self.session_id.clone() {
                        self.phase_to(RunPhase::Checkpointing);
                        self.effect(EffectKind::Snapshot { session_id }, CHECKPOINT_MS, false);
                    } else {
                        self.phase_to(RunPhase::CapturingWorkspace);
                        self.effect(EffectKind::CaptureWorkspace, CHECKPOINT_MS, false);
                    }
                }
                Stage::Publishing => unreachable!("publication always has a pending effect"),
                Stage::Cleaning => {
                    self.phase_to(RunPhase::CleaningUp);
                    self.effect(EffectKind::Cleanup, LIFECYCLE_MS, false);
                }
                Stage::Done => return,
            }
        }
    }

    fn start_session(&mut self) {
        self.session += 1;
        let id = self.ids.session(self.session);
        if !self.seen_sessions.insert(id.clone()) {
            self.diagnostic("ID source repeated a session ID");
            self.stop(StopReason::InfrastructureFailure);
            return;
        }
        self.session_id = Some(id.clone());
        self.request_context = None;
        self.turns = 0;
        self.handoff_attempts = 0;
        self.tool_calls_remaining = 0;
        self.session_parse_errors = 0;
        self.ended = false;
        self.finished = false;
        self.context = ContextWindow::default();
        self.policy.session_starting();
        self.commands.clear();
        self.checkpoint_attempted = false;
        // A dispatched but partly successful start also needs finalization.
        // Dispatch acknowledgement marks it active; preflight cancellation does not.
        self.active = false;
        self.phase_to(RunPhase::StartingSession);
        self.effect(
            EffectKind::StartSession {
                session_id: id,
                ordinal: self.session,
            },
            LIFECYCLE_MS,
            true,
        );
    }

    fn remaining_tokens(&self) -> u64 {
        self.config
            .parameters
            .limits
            .generated_tokens
            .get()
            .saturating_sub(self.totals.budget_charged_tokens())
    }

    fn session_started(&mut self, memory: Memory) {
        let memory = memory.bounded(
            self.config
                .memento
                .as_ref()
                .unwrap()
                .parameters
                .limits
                .memento_bytes
                .get(),
        );
        let evidence = self
            .last_checkpoint
            .as_ref()
            .and_then(|checkpoint| checkpoint.evidence.clone());
        self.control(ControlFact::SessionStarted {
            ordinal: self.session,
            workspace: self
                .last_checkpoint
                .as_ref()
                .map_or(&self.config.initial_workspace, |checkpoint| {
                    &checkpoint.snapshot.workspace
                })
                .clone(),
            memory: memory.clone(),
            evidence: evidence.clone(),
        });
        self.emit(
            "memory_received",
            json!({"ordinal": self.session, "context": "fresh", "memory": memory,
            "evidence": evidence, "memory_condition": self.config.memento.as_ref().unwrap().parameters.memory_condition}),
        );
        self.policy.session_started(
            View {
                config: &self.config,
                facts: self.facts(),
            },
            &memory,
            evidence.as_ref(),
        );
        self.phase_to(RunPhase::Working);
    }

    fn begin_handoff(&mut self) {
        self.phase_to(RunPhase::HandingOff);
        self.policy.begin_handoff(View {
            config: &self.config,
            facts: self.facts(),
        });
    }

    fn request(&mut self, proposal: Inference) {
        let Inference {
            purpose,
            messages,
            tools,
        } = proposal;
        let handoff = purpose == RequestPurpose::Handoff;
        let limits = &self.config.parameters.limits;
        let contract = self
            .config
            .request_contract(purpose)
            .expect("validated request purpose");
        let maximum = contract.maximum_tokens.min(
            self.remaining_tokens()
                .saturating_sub(contract.generation_reserve),
        );
        let Some(reservation) = NonZeroU64::new(maximum) else {
            self.stop(StopReason::TokenLimit);
            return;
        };
        let (context_input, basis) = self.context.estimate(&messages, &tools);
        let context_reserve = contract.context_reserve;
        if context_input
            .saturating_add(maximum)
            .saturating_add(context_reserve)
            >= limits.context_tokens.get()
        {
            self.emit(
                if self.config.memento.is_none() {
                    "context_guard_rejected"
                } else if handoff {
                    "handoff_skipped"
                } else {
                    "early_handoff"
                },
                json!({"reason": "context_guard", "context_input_bound": context_input}),
            );
            if self.config.memento.is_none() {
                self.diagnostic("run-scoped input exceeds the shared context allowance");
                self.stop(if self.config.compaction.is_some() {
                    StopReason::ContextLimit
                } else {
                    StopReason::InfrastructureFailure
                });
            } else if handoff {
                self.policy.context_rejected();
            } else {
                self.begin_handoff();
            }
            return;
        }
        let timeout_ms = limits.model_seconds.get() * 1000;
        let call = self.model_calls + 1;
        let id = self.ids.request(call);
        if !self.seen_requests.insert(id.clone()) {
            self.diagnostic("ID source repeated a request ID");
            self.stop(StopReason::InfrastructureFailure);
            return;
        }
        let request = ModelRequest {
            request_id: id,
            call,
            purpose,
            sampling_seed: ((u64::from(self.config.parameters.sampling_seed) + call - 1)
                % (1 << 31)) as u32,
            maximum_tokens: maximum,
            messages,
            tools,
            context_input_bound: context_input,
            context_basis: basis.into(),
            context_reserve,
        };
        let mut recorded = json!(request);
        if self.config.memento.is_none() {
            recorded.as_object_mut().unwrap().remove("handoff");
            recorded["purpose"] = json!(request.purpose);
        }
        let Some(sequence) = self.emit("model_request", recorded) else {
            return;
        };
        self.request_context = Some(RequestContext {
            effect_id: self.next_effect,
            scope: self
                .session_id
                .clone()
                .map_or(RequestScope::Run, |session_id| RequestScope::Session {
                    session_id,
                }),
            purpose: request.purpose,
            call,
            request_id: request.request_id.clone(),
        });
        self.bind_record(sequence, self.request_identity());
        self.model_calls = call;
        self.reservation = Some(reservation);
        self.response_bytes = 0;
        self.current_handoff = handoff;
        if handoff {
            self.handoff_attempts += 1;
        } else if self.config.memento.is_some() {
            self.turns += 1;
        }
        self.effect(EffectKind::Model(request), timeout_ms, true);
    }

    fn model_response(
        &mut self,
        request: ModelRequest,
        response: ModelResponse,
        finish_reason: Option<&str>,
    ) {
        if encoded_size(&json!(response)) > MODEL_BYTES {
            self.model_error(ModelFailure::ByteLimit(
                "model response exceeded its byte limit".into(),
            ));
            return;
        }
        if !response.done {
            self.model_error(ModelFailure::Incomplete(
                "model response did not finish".into(),
            ));
            return;
        }
        let Some(tokens) = response.generated_tokens else {
            self.model_error(ModelFailure::MissingUsage(
                "model response omitted usage".into(),
            ));
            return;
        };
        if !crate::validation::valid_message(&response.message) {
            self.model_error(ModelFailure::Parse("invalid assistant message".into()));
            return;
        }
        if self
            .totals
            .record(GenerationUsage::Known {
                generated_tokens: tokens,
            })
            .is_err()
        {
            self.emit(
                "model_usage_overflow",
                json!({"reported_generated_tokens": tokens, "call": self.model_calls}),
            );
            self.diagnostic("server generation cannot be represented in cumulative u64 usage; allowance retained as unknown");
            self.stop(StopReason::TokenLimitExceededByServer);
            return;
        }
        self.reservation = None;
        self.control(ControlFact::RequestSettled {
            request: self.request_context.clone().unwrap(),
            usage: GenerationUsage::Known {
                generated_tokens: tokens,
            },
            totals: self.totals.into(),
        });
        self.parse_streak = 0;
        self.emit("request_settled", json!({"call": self.model_calls,
            "usage": GenerationUsage::Known { generated_tokens: tokens }, "totals": UsageSummary::from(self.totals)}));
        self.emit(
            "model_response",
            json!({"call": self.model_calls, "response": response}),
        );
        if self.totals.budget_charged_tokens()
            > self.config.parameters.limits.generated_tokens.get()
        {
            self.stop(StopReason::TokenLimitExceededByServer);
            return;
        }
        // Measured context belongs to the actual admitted input plus response,
        // never to history rewritten by a policy. Admission estimates the next
        // proposal against this verified prefix.
        let mut observed_messages = request.messages;
        observed_messages.push(response.message.clone());
        self.context.observe(
            &observed_messages,
            &request.tools,
            response.prompt_tokens,
            tokens,
        );
        if self.config.compaction.is_some() {
            self.emit(
                "model_completion",
                json!({"call":request.call,"finish_reason":finish_reason}),
            );
            if tokens > request.maximum_tokens {
                self.stop(StopReason::TokenLimitExceededByServer);
                return;
            }
            if request.purpose == RequestPurpose::Compaction && finish_reason != Some("stop") {
                self.diagnostic("summary requires explicit normal completion");
                self.stop(StopReason::CompactionFailed);
                return;
            }
            if request.purpose == RequestPurpose::Compaction
                && response.message["tool_calls"]
                    .as_array()
                    .is_some_and(|calls| !calls.is_empty())
            {
                self.diagnostic("summary returned tools despite its tool-free contract");
                self.stop(StopReason::CompactionFailed);
                return;
            }
            match crate::compaction::validate_tools(
                &response.message,
                finish_reason,
                self.config.parameters.limits.output_bytes.get(),
            ) {
                Err((reason, error)) => {
                    self.emit(
                        "tool_batch_rejected",
                        json!({"reason":reason,"error":error}),
                    );
                    self.stop(reason);
                    return;
                }
                Ok(Some(summary)) if request.purpose == RequestPurpose::Work => {
                    self.emit(
                        "completion_declared",
                        json!({"request_id":request.request_id,"summary":summary}),
                    );
                    self.finished = true;
                    self.ended = true;
                    self.stage = Stage::Checkpoint;
                    return;
                }
                _ => {}
            }
        }
        if response.message["tool_calls"]
            .as_array()
            .is_some_and(|calls| calls.len() > 8)
        {
            self.stop(StopReason::TooManyToolsInOneResponse);
        } else if request.tools.is_empty()
            && response.message["tool_calls"]
                .as_array()
                .is_some_and(|calls| !calls.is_empty())
        {
            self.diagnostic("tool-free request returned tool calls");
            self.stop(StopReason::InfrastructureFailure);
        } else {
            self.tool_result_bytes = 0;
            self.tool_calls_remaining = response.message["tool_calls"]
                .as_array()
                .map_or(0, Vec::len);
            self.policy.model_response(
                View {
                    config: &self.config,
                    facts: self.facts(),
                },
                request.request_id,
                response,
            );
        }
    }

    fn model_error(&mut self, error: ModelFailure) {
        self.pending = None;
        let charge = self.reserved_tokens();
        self.settle_unknown();
        if matches!(error, ModelFailure::Parse(_)) {
            self.parse_errors += 1;
            self.session_parse_errors += 1;
            self.parse_streak += 1;
        }
        let decision = self.policy.recovery(
            View {
                config: &self.config,
                facts: self.facts(),
            },
            &error,
        );
        let recovery = matches!(decision, Recovery::Retry { .. });
        self.emit(
            "model_error",
            json!({"error": error, "recovery_allowed": recovery,
            "token_budget_charge": charge, "consecutive_errors": self.parse_streak,
            "tool_parse_errors": self.parse_errors, "usage": UsageSummary::from(self.totals)}),
        );
        match decision {
            Recovery::Retry { end_session } => {
                self.policy.retry(View {
                    config: &self.config,
                    facts: self.facts(),
                });
                if end_session {
                    self.emit(
                        "early_handoff",
                        json!({"reason": "tool_parse_error_limit",
                        "session_tool_parse_errors": self.session_parse_errors}),
                    );
                    self.begin_handoff();
                } else {
                    self.after_response(false);
                }
            }
            Recovery::Stop(reason) => {
                self.diagnostic(&format!("{error:?}"));
                self.stop(reason);
            }
        }
    }

    fn next_tool(&mut self, proposed: ToolRequest) {
        let name = proposed.name.clone();
        let arguments = proposed.arguments.clone();
        let Some(sequence) = self.emit(
            "tool_request",
            json!({"name": name, "arguments": arguments}),
        ) else {
            return;
        };
        let error = if let Some(config) = &self.config.memento {
            contract::tool_error(
                &proposed,
                &config.parameters.limits,
                self.ended,
                self.current_handoff,
            )
        } else {
            let purpose = self.request_context.as_ref().unwrap().purpose;
            let allowed = self.config.request_contract(purpose).unwrap().tools;
            if allowed
                .iter()
                .any(|tool| tool["function"]["name"] == proposed.name)
            {
                crate::validation::validate_call(&allowed, &proposed.name, &proposed.arguments)
            } else {
                Some("Tool is unavailable for this request purpose".into())
            }
        };
        let save_memory = self.config.memento.is_some() && name == "end_session";
        let mut active = ActiveTool {
            request: ToolRequest {
                name: name.clone(),
                arguments: arguments.clone(),
            },
            request_sequence: sequence,
            command: None,
            save_memory,
            finish_run: arguments["finish_run"].as_bool().unwrap_or(false),
            effect_id: None,
        };
        if let Some(error) = error {
            self.tool = Some(active);
            self.tool_result(json!({"error": error}));
            return;
        }
        let mut identity = self.request_identity();
        identity.effect_id = NonZeroU64::new(self.next_effect);
        identity.tool_request_sequence = NonZeroU64::new(sequence);
        let Some(dispatch) = self.emit_bound(
            if save_memory {
                "memento_write"
            } else {
                "tool_dispatch"
            },
            json!({"name": name, "request_sequence": sequence}),
            identity.clone(),
        ) else {
            return;
        };
        active.effect_id = Some(self.next_effect);
        self.bind_record(sequence, identity);
        if name == "exec" {
            active.command = Some(self.commands.len());
            self.commands.push(CommandObservation {
                command: arguments["command"].as_str().unwrap().into(),
                cwd: arguments["cwd"].as_str().unwrap_or("/work").into(),
                result: None,
                sources: CommandSources {
                    request: sequence,
                    dispatch,
                    result: None,
                },
            });
        }
        let request = if save_memory {
            contract::workshop_request(&active.request)
        } else {
            active.request.clone()
        };
        let mut timeout_ms = self.config.parameters.limits.command_seconds.get() * 1000;
        if let Some(seconds) = arguments["timeout_seconds"]
            .as_f64()
            .filter(|value| *value > 0.0)
        {
            timeout_ms = timeout_ms.min((seconds * 1000.0).ceil() as u64).max(1);
        }
        if matches!(self.tool_policy, ToolPolicy::ConfirmedExternal { .. }) {
            // timeout_seconds limits the command INSIDE the bridge. Transport
            // and return use the frozen command cap, still clipped by the work
            // deadline. A 100ms request must not consume its return allowance
            // while the committed intent is being published.
            timeout_ms = self.config.parameters.limits.command_seconds.get() * 1000;
        }
        self.tool = Some(active);
        self.effect(EffectKind::Tool(request), timeout_ms, true);
    }

    fn tool_result(&mut self, original: Value) {
        let tool = self.tool.take().expect("one active tool");
        if !original.is_object() {
            self.diagnostic("workshop returned a non-object tool result");
            self.stop(StopReason::InfrastructureFailure);
            return;
        }
        if tool.request.name == "exec"
            && self.pending_started
            && let Some(effect_id) = tool.effect_id
        {
            let code = original
                .get("exit_code")
                .and_then(Value::as_i64)
                .and_then(|code| i32::try_from(code).ok());
            let outcome = if original["timed_out"] == true {
                CommandOutcome::TimedOut { exit_code: code }
            } else if let Some(exit_code) = code {
                CommandOutcome::Exited { exit_code }
            } else {
                CommandOutcome::Unknown {
                    reason: "workshop result did not record an exit status".into(),
                }
            };
            self.control(ControlFact::ExecResult {
                request: self.request_context.clone().unwrap(),
                effect_id,
                tool_request_sequence: tool.request_sequence,
                outcome,
            });
        }
        if tool.save_memory && original.get("error").is_none() {
            self.ended = true;
            self.finished = tool.finish_run;
        }
        let result = self.policy.fit_result(
            View {
                config: &self.config,
                facts: self.facts(),
            },
            &self.context,
            &tool.request.name,
            &original,
        );
        if self.config.compaction.is_some() {
            self.tool_result_bytes =
                self.tool_result_bytes
                    .saturating_add(encoded_size(&crate::context::tool_message(
                        &tool.request.name,
                        &result,
                    )));
            if self.tool_result_bytes > self.config.parameters.limits.output_bytes.get() {
                self.diagnostic("fitted tool results exceed the frozen aggregate allowance");
                self.stop(StopReason::ContextLimit);
                return;
            }
        }
        let mut identity = self.request_identity();
        identity.effect_id = tool.effect_id.and_then(NonZeroU64::new);
        identity.tool_request_sequence = NonZeroU64::new(tool.request_sequence);
        let sequence = self.emit_bound(
            "tool_result",
            json!({"name": tool.request.name,
            "request_sequence": tool.request_sequence, "result": result,
            "unabridged_result": if result != original { Some(&original) } else { None }}),
            identity,
        );
        if let Some(index) = tool.command {
            self.commands[index].result = Some(original);
            self.commands[index].sources.result = sequence;
        }
        self.policy.tool_result(&tool.request.name, &result);
    }

    fn after_response(&mut self, complete: bool) {
        if let Some(feedback) = self.policy.after_response(
            View {
                config: &self.config,
                facts: self.facts(),
            },
            complete,
        ) {
            self.emit(
                "handoff_feedback",
                json!({"reason": "missing_tool_call", "content": feedback}),
            );
        }
    }

    fn complete(&mut self, effect: EffectKind, outcome: Outcome) {
        match (effect, outcome) {
            (EffectKind::StartWorkspace, Outcome::WorkspaceStarted) => {
                self.control(ControlFact::WorkspaceStarted {
                    workspace: self.config.initial_workspace.clone(),
                });
                self.emit(
                    "workspace_started",
                    json!({"workspace": self.config.initial_workspace}),
                );
                self.policy.workspace_started();
                self.phase_to(RunPhase::Working);
            }
            (EffectKind::CaptureWorkspace, Outcome::WorkspaceCaptured(snapshot)) => {
                self.active = false;
                self.stage = Stage::Publishing;
                self.effect(
                    EffectKind::PublishCapture(Box::new(Capture {
                        snapshot,
                        interrupted: self.reason.is_some(),
                    })),
                    LIFECYCLE_MS,
                    false,
                );
            }
            (EffectKind::PublishCapture(capture), Outcome::Published) => {
                if let Some((effect_id, _, _)) = self.publication.take() {
                    self.control(ControlFact::CaptureCommitted { effect_id });
                }
                self.emit("workspace_captured", json!({"workspace": capture.snapshot.workspace, "interrupted": capture.interrupted}));
                self.last_capture = Some(*capture);
                self.policy.published();
                self.stage = if self.reason.is_some() {
                    Stage::Cleaning
                } else {
                    Stage::Policy
                };
            }
            (EffectKind::StartSession { .. }, Outcome::Started(memory)) => {
                self.session_started(memory)
            }
            (EffectKind::Model(request), Outcome::Model(response)) => {
                self.model_response(request, response, None)
            }
            (
                EffectKind::Model(request),
                Outcome::ModelCompleted {
                    response,
                    finish_reason,
                },
            ) => self.model_response(request, response, Some(&finish_reason)),
            (EffectKind::Model(_), Outcome::ModelError(error)) => self.model_error(error),
            (EffectKind::Tool(_), Outcome::Tool(result)) => self.tool_result(result),
            (EffectKind::Snapshot { .. }, Outcome::Snapshot(mut snapshot)) => {
                self.active = false;
                self.control(ControlFact::SessionClosed {
                    finish_run: self.finished,
                    handoff_saved: self.ended,
                });
                snapshot.memory = snapshot.memory.bounded(
                    self.config
                        .memento
                        .as_ref()
                        .unwrap()
                        .parameters
                        .limits
                        .memento_bytes
                        .get(),
                );
                let evidence = Memento::checkpoint_evidence(
                    self.config.memento.as_ref().unwrap(),
                    self.session,
                    self.last_checkpoint.as_ref(),
                    &snapshot,
                    &self.commands,
                );
                let checkpoint = Checkpoint {
                    session_id: self.session_id.clone().unwrap(),
                    ordinal: self.session,
                    snapshot,
                    evidence,
                    handoff_saved: self.ended,
                    interrupted: self.reason.is_some(),
                };
                self.stage = Stage::Publishing;
                self.effect(
                    EffectKind::PublishCheckpoint(Box::new(checkpoint)),
                    LIFECYCLE_MS,
                    false,
                );
            }
            (EffectKind::PublishCheckpoint(checkpoint), Outcome::Published) => {
                if let Some((effect_id, _, _)) = self.publication.take() {
                    self.control(ControlFact::CheckpointCommitted {
                        effect_id,
                        session_id: checkpoint.session_id.clone(),
                    });
                }
                self.emit("controller_checkpoint", json!({"ordinal": checkpoint.ordinal, "workspace": checkpoint.snapshot.workspace,
                    "memory": checkpoint.snapshot.memory, "evidence": checkpoint.evidence,
                    "handoff_saved": checkpoint.handoff_saved, "interrupted": checkpoint.interrupted}));
                self.last_checkpoint = Some(*checkpoint);
                self.policy.published();
                self.stage = Stage::Policy;
                if self.reason.is_some() {
                    self.stage = Stage::Cleaning;
                } else if self.log_exhausted {
                    self.stop(StopReason::LogLimit);
                }
            }

            (EffectKind::Cleanup, Outcome::Cleaned(errors)) => self.finish(errors),
            (effect, Outcome::Failed(error)) => self.failed(effect, error),
            _ => unreachable!("advance validates completion type"),
        }
    }

    fn failed(&mut self, effect: EffectKind, error: String) {
        match effect {
            EffectKind::Model(_) => self.model_error(ModelFailure::Infrastructure(error)),
            EffectKind::Cleanup => self.finish(vec![error]),
            EffectKind::Snapshot { .. }
            | EffectKind::PublishCheckpoint(_)
            | EffectKind::CaptureWorkspace
            | EffectKind::PublishCapture(_) => {
                let operation = if self.config.memento.is_some() {
                    "checkpoint"
                } else {
                    "workspace capture"
                };
                self.diagnostic(&format!("{operation}: {error}"));
                self.stop(StopReason::InfrastructureFailure);
                self.stage = Stage::Cleaning;
            }
            _ => {
                self.diagnostic(&error);
                self.stop(StopReason::InfrastructureFailure);
            }
        }
    }

    fn timeout(&mut self) {
        let effect = self.pending.take().unwrap();
        if !self.pending_started {
            self.control(ControlFact::EffectNotDispatched(self.effect_info(&effect)));
        }
        match effect.kind {
            EffectKind::Model(_) => {
                self.model_error(ModelFailure::Timeout("model deadline expired".into()))
            }
            EffectKind::Tool(request) => {
                self.undispatched_tool();
                if matches!(self.tool_policy, ToolPolicy::ConfirmedExternal { .. }) {
                    self.failed(
                        EffectKind::Tool(request),
                        "external tool deadline expired without confirmed termination".into(),
                    );
                    return;
                }
                self.tool_result(
                    json!({"error": "workshop command deadline expired", "timed_out": true}),
                );
            }
            kind => self.failed(kind, "effect deadline expired".into()),
        }
    }

    fn finish(&mut self, errors: Vec<String>) {
        let errors: Vec<String> = errors
            .into_iter()
            .take(8)
            .map(|error| error.chars().take(512).collect())
            .collect();
        let cleanup = if errors.is_empty() {
            CleanupOutcome::Complete
        } else {
            CleanupOutcome::Failed { errors }
        };
        self.stage = Stage::Done;
        self.phase_to(RunPhase::Terminal);
        let result = RunResult {
            run_id: self.run_id.clone(),
            mode: self.mode,
            reason: self.reason.expect("cleanup follows a decision"),
            usage: self.totals.into(),
            model_calls: self.model_calls,
            tool_parse_errors: self.parse_errors,
            sessions_started: self.session,
            elapsed_ms: self.now_ms - self.started_ms,
            cleanup,
            last_checkpoint: self.last_checkpoint.clone(),
            last_capture: self.last_capture.clone(),
            diagnostics: self.diagnostics.clone(),
        };
        self.emit(
            "controller_finished",
            json!({"run_id": result.run_id, "mode": result.mode, "reason": result.reason,
            "usage": result.usage, "cleanup": result.cleanup, "diagnostics": result.diagnostics}),
        );
        self.result = Some(result);
        self.control(ControlFact::Finished);
    }

    fn control(&mut self, fact: ControlFact) {
        assert!(
            self.controls.len() < CONTROL_QUEUE_CAPACITY,
            "one transition exceeded its reserved control capacity"
        );
        self.controls.push(ControlUpdate {
            elapsed_ms: self.now_ms - self.started_ms,
            session_id: self.session_id.clone(),
            fact,
        });
    }

    fn bound_request(&self, identity: &ObservationIdentity) -> Option<RequestContext> {
        self.request_context
            .as_ref()
            .filter(|request| identity.request_id.as_ref() == Some(&request.request_id))
            .cloned()
    }

    fn request_identity(&self) -> ObservationIdentity {
        self.request_context
            .as_ref()
            .map_or_else(ObservationIdentity::default, |request| {
                ObservationIdentity {
                    effect_id: NonZeroU64::new(request.effect_id),
                    request_id: Some(request.request_id.clone()),
                    tool_request_sequence: None,
                }
            })
    }

    fn bind_record(&mut self, sequence: u64, identity: ObservationIdentity) {
        if let Some(entry) = self
            .outbox
            .iter_mut()
            .find(|entry| entry.record.sequence == sequence)
        {
            entry.request = self.request_context.clone();
            entry.identity = identity;
        }
    }

    fn effect_info(&self, effect: &Effect) -> EffectInfo {
        let (kind, maximum_tokens) = match &effect.kind {
            EffectKind::StartWorkspace => ("start_workspace", None),
            EffectKind::CaptureWorkspace => ("capture_workspace", None),
            EffectKind::PublishCapture(_) => ("publish_capture", None),
            EffectKind::StartSession { .. } => ("start_session", None),
            EffectKind::Model(request) => ("model", Some(request.maximum_tokens)),
            EffectKind::Tool(_) => ("tool", None),
            EffectKind::Snapshot { .. } => ("snapshot", None),
            EffectKind::PublishCheckpoint(_) => ("publish_checkpoint", None),
            EffectKind::Cleanup => ("cleanup", None),
        };
        let mut identity = if matches!(effect.kind, EffectKind::Model(_) | EffectKind::Tool(_)) {
            self.request_identity()
        } else {
            ObservationIdentity::default()
        };
        identity.effect_id = NonZeroU64::new(effect.id);
        if matches!(effect.kind, EffectKind::Tool(_)) {
            identity.tool_request_sequence = self
                .tool
                .as_ref()
                .and_then(|tool| NonZeroU64::new(tool.request_sequence));
        }
        EffectInfo {
            id: effect.id,
            kind,
            session_id: self.session_id.clone(),
            request: self.bound_request(&identity),
            identity,
            maximum_tokens,
            deadline_ms: effect.deadline_ms,
        }
    }

    fn publication_committed(&mut self, effect_id: u64) {
        let artifact = self.publication.as_ref().unwrap().1.clone();
        if self
            .pending
            .as_ref()
            .is_some_and(|effect| effect.id == effect_id)
        {
            self.pending = None;
            self.complete(artifact.effect(), Outcome::Published);
        } else {
            self.publication = None;
            let PublicationArtifact::Checkpoint(checkpoint) = artifact else {
                let PublicationArtifact::Capture(capture) = artifact else {
                    unreachable!()
                };
                self.control(ControlFact::CaptureCommitted { effect_id });
                self.emit("workspace_captured", json!({"workspace": capture.snapshot.workspace, "interrupted": capture.interrupted}));
                self.last_capture = Some(*capture);
                if let Some(result) = &mut self.result {
                    result.last_capture = self.last_capture.clone();
                }
                return;
            };
            self.control(ControlFact::CheckpointCommitted {
                effect_id,
                session_id: checkpoint.session_id.clone(),
            });
            self.emit(
                "controller_checkpoint",
                json!({"ordinal": checkpoint.ordinal, "workspace": checkpoint.snapshot.workspace,
                "memory": checkpoint.snapshot.memory, "evidence": checkpoint.evidence,
                "handoff_saved": checkpoint.handoff_saved, "interrupted": checkpoint.interrupted}),
            );
            self.last_checkpoint = Some(*checkpoint);
            if let Some(result) = &mut self.result {
                result.last_checkpoint = self.last_checkpoint.clone();
            }
        }
    }
}
