//! Run progress against the frozen limits, kept up to date as records are
//! indexed so it never depends on evicted payloads, and the run header's words.
//! Background indexing feeds the tracker; the header formats prepared values.
use crate::transcript::{excerpt, thousands, words};
use playtime_core::{MemoryCondition, RunMode, RunPhase, StopReason};
use playtime_protocol::{
    api::{Cleanup, Enforcement, EvidenceAvailability, OrchestrationState, RunView},
    events::KnownEvent,
    observations::{DecodedObservation, KnownObservation, ObservationPayload},
};
use std::collections::BTreeMap;

/// The limits progress is measured against, from the run's frozen parameters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Limits {
    pub sessions: u64,
    pub turns: u64,
    pub wall_seconds: u64,
    pub tokens: u64,
}
impl Limits {
    /// Reads the parameters `controller_started` records.
    fn from_parameters(parameters: &serde_json::Value) -> Option<Self> {
        let limit = |name: &str| parameters.get("limits")?.get(name)?.as_u64();
        Some(Self {
            sessions: limit("sessions")?,
            turns: limit("work_turns_per_session")?,
            wall_seconds: limit("wall_seconds")?,
            tokens: limit("generated_tokens")?,
        })
    }
}

/// What the run header can say about progress. None means not known yet: a
/// value appears only once every record it depends on is indexed.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Progress {
    pub limits: Option<Limits>,
    /// The latest session's ordinal.
    pub session: Option<u64>,
    /// Sessions whose checkpoint the run's stop didn't interrupt.
    pub completed: Option<u64>,
    /// The work turns the latest session has started.
    pub turn: Option<u64>,
    /// Controller time at the latest record.
    pub elapsed_ms: Option<u64>,
    /// What the run is doing, as of the latest record.
    pub activity: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Activity {
    Starting,
    Phase(RunPhase),
    /// A model request or its streamed output.
    Model,
    Tool(String),
    HandingOff,
    ModelError {
        retrying: bool,
    },
    Stopping(StopReason),
    Working,
    Compacting,
}

/// What one record says about progress.
#[derive(Debug, Default)]
pub(crate) struct Update {
    session: Option<String>,
    elapsed_ms: Option<u64>,
    /// The controller's start, so its checkpoints are recorded too.
    started: bool,
    limits: Option<Limits>,
    ordinal: Option<u64>,
    /// A checkpoint the run's stop didn't interrupt.
    completed: Option<u64>,
    phase: Option<RunPhase>,
    /// A model request, with its handoff flag unless its payload is retained
    /// elsewhere.
    request: Option<Option<bool>>,
    activity: Option<Activity>,
}
impl Update {
    pub(crate) fn execution(o: &playtime_protocol::execution::Observation) -> Self {
        use playtime_protocol::observations::ObservationData;
        let value = match &o.data {
            ObservationData::Inline { value } => Some(value),
            _ => None,
        };
        let mut update = Self {
            elapsed_ms: Some(o.elapsed_ms),
            ..Default::default()
        };
        update.activity = match o.observation_kind.as_str() {
            "controller_started" => {
                update.limits = value.and_then(|v| {
                    let l = &v["parameters"]["limits"];
                    Some(Limits {
                        sessions: 0,
                        turns: 0,
                        wall_seconds: l["wall_seconds"].as_u64()?,
                        tokens: l["generated_tokens"].as_u64()?,
                    })
                });
                Some(Activity::Starting)
            }
            "model_request" | "model_chunk" => Some(
                if o.request
                    .as_ref()
                    .is_some_and(|r| r.purpose == playtime_core::RequestPurpose::Compaction)
                {
                    Activity::Compacting
                } else {
                    Activity::Model
                },
            ),
            "compaction_planned" => Some(Activity::Compacting),
            "tool_request" => value
                .and_then(|v| v["name"].as_str())
                .map(|s| Activity::Tool(excerpt(s, 60))),
            "model_response" | "tool_result" | "compaction_applied" | "compaction_deferred" => {
                Some(Activity::Working)
            }
            "controller_finished" => Some(Activity::Phase(RunPhase::Terminal)),
            _ => None,
        };
        update
    }

    pub(crate) fn observation(observation: &DecodedObservation) -> Self {
        let o = observation.observation();
        let mut update = Self {
            session: o.session_id.as_ref().map(|s| s.as_str().to_owned()),
            elapsed_ms: Some(o.elapsed_ms),
            ..Default::default()
        };
        update.activity = match observation.payload() {
            ObservationPayload::Known(known) => match known.as_ref() {
                KnownObservation::ControllerStarted { parameters, .. } => {
                    update.started = true;
                    update.limits = Limits::from_parameters(parameters);
                    Some(Activity::Starting)
                }
                KnownObservation::PhaseChanged { phase, .. } => {
                    update.phase = Some(*phase);
                    Some(Activity::Phase(*phase))
                }
                KnownObservation::MemoryReceived { ordinal, .. } => {
                    update.ordinal = Some(ordinal.get().into());
                    Some(Activity::Phase(RunPhase::StartingSession))
                }
                KnownObservation::ModelRequest(request) => {
                    update.request = Some(Some(request.handoff));
                    Some(Activity::Model)
                }
                KnownObservation::ModelChunk { .. } => Some(Activity::Model),
                KnownObservation::ModelResponse { .. }
                | KnownObservation::RequestNotDispatched { .. }
                | KnownObservation::ToolNotDispatched { .. }
                | KnownObservation::ToolResult { .. } => Some(Activity::Working),
                KnownObservation::ModelError {
                    recovery_allowed, ..
                } => Some(Activity::ModelError {
                    retrying: *recovery_allowed,
                }),
                KnownObservation::ToolRequest { name, .. }
                | KnownObservation::ToolIntent { name, .. } => {
                    Some(Activity::Tool(excerpt(name, 60)))
                }
                KnownObservation::MementoWriteIntent { .. } => Some(Activity::HandingOff),
                KnownObservation::ControllerCheckpoint {
                    ordinal,
                    interrupted,
                    ..
                } => {
                    update.ordinal = Some(ordinal.get().into());
                    update.completed = (!interrupted).then_some(ordinal.get().into());
                    Some(Activity::Phase(RunPhase::Checkpointing))
                }
                KnownObservation::ControllerStop { reason, .. } => {
                    Some(Activity::Stopping(*reason))
                }
                KnownObservation::ControllerFinished { .. } => {
                    Some(Activity::Phase(RunPhase::Terminal))
                }
                KnownObservation::ModelChunkRejected { .. }
                | KnownObservation::ModelUsageOverflow { .. }
                | KnownObservation::RequestSettled { .. } => None,
            },
            // A retained payload still names its kind.
            ObservationPayload::Referenced { .. } => match o.observation_kind.as_str() {
                "controller_started" => {
                    update.started = true;
                    Some(Activity::Starting)
                }
                "model_request" => {
                    update.request = Some(None);
                    Some(Activity::Model)
                }
                "model_chunk" => Some(Activity::Model),
                "model_response" | "tool_result" => Some(Activity::Working),
                _ => None,
            },
            ObservationPayload::Unknown { .. } => None,
        };
        update
    }
    pub(crate) fn event(event: &KnownEvent) -> Self {
        match event {
            KnownEvent::SessionStarted {
                session_id,
                ordinal,
                ..
            } => Self {
                session: Some(session_id.as_str().to_owned()),
                ordinal: Some(ordinal.get().into()),
                ..Default::default()
            },
            _ => Self::default(),
        }
    }
    /// A model request adds an entry to the latest session's turn count.
    pub(crate) fn tracked_bytes(&self) -> usize {
        if self.request.is_some() { 64 } else { 0 }
    }
}

#[derive(Default)]
struct Session {
    id: String,
    ordinal: Option<u64>,
    /// Its start arrived live, so every later record of it arrives live too.
    started_live: bool,
    /// The record that began its handoff.
    handoff: Option<u64>,
    /// Model requests by sequence, with each handoff flag that is known.
    requests: BTreeMap<u64, Option<bool>>,
}
impl Session {
    /// Requests before the handoff are work turns, as the controller counts them.
    fn turns(&self) -> u64 {
        self.requests
            .iter()
            .filter(|(sequence, handoff)| match handoff {
                Some(handoff) => !handoff,
                None => self.handoff.is_none_or(|start| **sequence < start),
            })
            .count() as u64
    }
}

/// Records arrive in any order: earlier history loads while live records
/// arrive. Each value keeps the latest record by sequence, and only the latest
/// session keeps its requests.
#[derive(Default)]
pub(crate) struct Tracker {
    limits: Option<Limits>,
    /// The highest ordinal recorded for any session.
    max_ordinal: u64,
    /// The highest uninterrupted checkpoint. Sessions run in order and an
    /// interrupted checkpoint is the run's last, so this counts the sessions
    /// completed.
    completed: u64,
    /// The controller's start is indexed. An archive without controller
    /// records has no checkpoints to count.
    started: bool,
    current: Option<Session>,
    /// The latest record naming a session, and whether it arrived live. Until
    /// history loads, only a live record names the run's latest session.
    session_sequence: u64,
    session_live: bool,
    /// The latest observation's sequence, controller time and whether it
    /// arrived live.
    latest: Option<(u64, u64, bool)>,
    activity: Option<(u64, Activity, bool)>,
}
impl Tracker {
    /// A record without a history page arrived live.
    pub(crate) fn apply(&mut self, sequence: u64, update: Update, live: bool) {
        self.started |= update.started;
        if update.limits.is_some() {
            self.limits = update.limits;
        }
        if let Some(ordinal) = update.ordinal {
            self.max_ordinal = self.max_ordinal.max(ordinal);
        }
        if let Some(ordinal) = update.completed {
            self.completed = self.completed.max(ordinal);
        }
        if let Some(id) = update.session {
            if sequence > self.session_sequence {
                self.session_sequence = sequence;
                self.session_live = live;
                if self.current.as_ref().is_none_or(|session| session.id != id) {
                    self.current = Some(Session {
                        id: id.clone(),
                        ..Default::default()
                    });
                }
            }
            // A record from an earlier session changes nothing here.
            if let Some(session) = self.current.as_mut().filter(|session| session.id == id) {
                if update.ordinal.is_some() {
                    session.ordinal = update.ordinal;
                }
                match update.phase {
                    Some(RunPhase::StartingSession) => session.started_live = live,
                    Some(RunPhase::HandingOff) => {
                        session.handoff =
                            Some(session.handoff.map_or(sequence, |s| s.min(sequence)));
                    }
                    _ => (),
                }
                if let Some(handoff) = update.request {
                    session.requests.insert(sequence, handoff);
                }
            }
        }
        if let Some(elapsed) = update.elapsed_ms
            && self.latest.is_none_or(|(latest, ..)| sequence > latest)
        {
            self.latest = Some((sequence, elapsed, live));
        }
        if let Some(activity) = update.activity
            && self
                .activity
                .as_ref()
                .is_none_or(|(latest, ..)| sequence > *latest)
        {
            self.activity = Some((sequence, activity, live));
        }
    }
    /// `complete` means earlier history has loaded, so every record up to
    /// the live stream is indexed.
    pub(crate) fn progress(&self, complete: bool) -> Progress {
        let session = self
            .current
            .as_ref()
            .filter(|_| complete || self.session_live);
        Progress {
            limits: self.limits,
            // A session that hasn't received its memento yet is the next one.
            session: session.and_then(|s| {
                s.ordinal
                    .or_else(|| complete.then_some(self.max_ordinal + 1))
            }),
            completed: (complete && self.started).then_some(self.completed),
            turn: session
                .filter(|s| complete || s.started_live)
                .map(Session::turns),
            elapsed_ms: self
                .latest
                .filter(|(.., live)| complete || *live)
                .map(|(_, elapsed, _)| elapsed),
            activity: self
                .activity
                .as_ref()
                .filter(|(.., live)| complete || *live)
                .map(|(sequence, activity, _)| self.describe(*sequence, activity)),
        }
    }
    fn describe(&self, sequence: u64, activity: &Activity) -> String {
        match activity {
            Activity::Starting => "starting the run".into(),
            Activity::Phase(phase) => phase_words(*phase).into(),
            // A handoff request says so itself, even before the session's
            // handing-off phase has loaded.
            Activity::Model => {
                if self.current.as_ref().is_some_and(|session| {
                    session.handoff.is_some_and(|start| sequence > start)
                        || session
                            .requests
                            .range(..=sequence)
                            .next_back()
                            .is_some_and(|(_, handoff)| *handoff == Some(true))
                }) {
                    "handing off".into()
                } else {
                    "thinking".into()
                }
            }
            Activity::Tool(name) => match name.as_str() {
                "exec" => "running a command".into(),
                "read_file" => "reading a file".into(),
                "write_file" => "writing a file".into(),
                "end_session" => "handing off".into(),
                other => format!("using {other}"),
            },
            Activity::HandingOff => "handing off".into(),
            Activity::ModelError { retrying: true } => "retrying after a model error".into(),
            Activity::ModelError { retrying: false } => "stopping after a model error".into(),
            Activity::Stopping(reason) => format!("stopping: {}", words(reason)),
            Activity::Working => "working".into(),
            Activity::Compacting => "compacting the conversation".into(),
        }
    }
}

/// The header's first two lines. A live run leads with its session, turn and
/// activity, then tokens and time against the frozen limits. An ended run leads
/// with its outcome, the sessions it completed and tokens, then cleanup.
pub fn lead(run: &RunView, progress: &Progress) -> Vec<String> {
    let limits = progress.limits;
    let tokens = run.usage.as_ref().map_or_else(
        || "Tokens charged: unavailable".to_owned(),
        |usage| match limits {
            Some(limits) => format!(
                "{} of {} tokens charged",
                thousands(usage.budget_charged_tokens),
                thousands(limits.tokens)
            ),
            None => format!("{} tokens charged", thousands(usage.budget_charged_tokens)),
        },
    );
    let time = progress.elapsed_ms.map(|elapsed| match limits {
        Some(limits) => format!(
            "{} of {} wall time",
            clock(elapsed / 1000),
            clock(limits.wall_seconds)
        ),
        None => format!("{} elapsed", clock(elapsed / 1000)),
    });
    if run.orchestration.state == OrchestrationState::Terminal {
        let mut first = vec![match &run.outcome {
            Some(outcome) => format!("Ended: {}", words(&outcome.reason)),
            None => "Ended without a recorded outcome".into(),
        }];
        // A session the stop cut off isn't completed.
        first.extend(progress.completed.map(|completed| match limits {
            Some(limits) => format!("{completed} of {} sessions completed", limits.sessions),
            None => count(completed, "session completed", "sessions completed"),
        }));
        first.push(tokens);
        let mut second = vec![cleanup_words(&run.cleanup)];
        second.extend(time);
        return vec![first.join(" · "), second.join(" · ")];
    }
    let mut first = vec![];
    first.extend(progress.session.map(|session| match limits {
        Some(limits) => format!("Session {session} of {}", limits.sessions),
        None => format!("Session {session}"),
    }));
    first.extend(
        progress
            .turn
            .filter(|turn| *turn > 0)
            .map(|turn| match limits {
                Some(limits) => format!("turn {turn} of {}", limits.turns),
                None => format!("turn {turn}"),
            }),
    );
    first.extend(match run.orchestration.state {
        OrchestrationState::Running => progress
            .activity
            .clone()
            .or_else(|| run.controller_phase.map(|phase| phase_words(phase).into())),
        state => Some(state_words(state).to_lowercase()),
    });
    if first.is_empty() {
        first.push(state_words(run.orchestration.state).into());
    }
    let mut second = vec![tokens];
    second.extend(time);
    vec![capitalized(&first.join(" · ")), second.join(" · ")]
}

/// The state, accounting and evidence lines under the header's lead.
pub fn details(run: &RunView) -> Vec<String> {
    let state = run.orchestration.state;
    let mut first = vec![state_words(state).to_owned()];
    first.extend(run.orchestration.reason.as_deref().map(reason_words));
    first.push(mode_words(run.mode).into());
    first.push(evidence_words(&run.evidence));
    let mut lines = vec![first.join(" · ")];
    // The archive can record the outcome before the daemon sees the worker
    // stop, and a cancelled run can wait in reconciling. An ended run reports
    // its outcome in its lead.
    if state != OrchestrationState::Terminal
        && let Some(outcome) = &run.outcome
    {
        lines.push(format!("Outcome: {}", words(&outcome.reason)));
    }
    // An ended run reports cleanup in its lead.
    if matches!(
        state,
        OrchestrationState::Cancelling | OrchestrationState::Reconciling
    ) || (state != OrchestrationState::Terminal && run.cleanup != Cleanup::Unknown)
    {
        lines.push(cleanup_words(&run.cleanup));
    }
    lines.push(accounting(run));
    if let Some(accounting) = &run.request_accounting {
        let classified = accounting
            .by_purpose
            .iter()
            .map(|(purpose, item)| (capitalized(&words(purpose)), item));
        let unclassified = &accounting.unclassified;
        // Missing purpose evidence stays visible instead of joining a purpose.
        let unclassified = (unclassified.admitted_calls > 0
            || unclassified.settled_calls > 0
            || unclassified.not_dispatched_calls > 0)
            .then(|| ("Unclassified".to_owned(), unclassified));
        for (purpose, item) in classified.chain(unclassified) {
            lines.push(purpose_accounting(&purpose, item));
        }
    }
    if let Some(c) = &run.contention {
        let timed = count(c.timed_requests, "timed request", "timed requests");
        lines.push(if c.possible {
            format!(
                "Possible GPU contention: {} and {} across {timed}",
                count(c.reloads, "model reload", "model reloads"),
                count(c.queue_waits, "queued request", "queued requests")
            )
        } else {
            format!("No GPU contention flagged across {timed}")
        });
    }
    if let Some(cancel) = &run.cancellation {
        let mut line = format!("Cancellation: {}", enforcement_words(cancel.enforcement));
        if let Some(uncertainty) = &cancel.uncertainty {
            line.push_str(&format!(" · {}", excerpt(uncertainty, 200)));
        }
        lines.push(line);
    }
    lines
}

/// Settled or known usage, conservative charges for unknown usage and the
/// allowance still held for unsettled requests. Unavailable is never zero.
/// One request purpose's calls and tokens. Unknown usage stays explicit.
fn purpose_accounting(
    purpose: &str,
    item: &playtime_protocol::archive_queries::PurposeAccounting,
) -> String {
    let mut parts = vec![format!(
        "{purpose}: {} · {} settled",
        count(item.admitted_calls, "call admitted", "calls admitted"),
        thousands(item.settled_calls)
    )];
    if item.not_dispatched_calls > 0 {
        parts.push(format!(
            "{} not dispatched",
            thousands(item.not_dispatched_calls)
        ));
    }
    parts.push(format!(
        "{} tokens reported",
        thousands(item.usage.known_generated_tokens)
    ));
    parts.push(format!(
        "{} tokens charged",
        thousands(item.usage.budget_charged_tokens)
    ));
    parts.push(if item.usage.unknown_requests == 0 {
        "no unknown usage".into()
    } else {
        count(
            item.usage.unknown_requests,
            "request with unknown usage",
            "requests with unknown usage",
        )
    });
    parts.join(" · ")
}
fn accounting(run: &RunView) -> String {
    let Some(usage) = &run.usage else {
        return "Token accounting unavailable".into();
    };
    let mut parts = vec![format!(
        "{}: {} tokens reported",
        if run.accounting_coverage.is_some() {
            "Settled"
        } else {
            "Known"
        },
        thousands(usage.known_generated_tokens)
    )];
    parts.push(if usage.unknown_requests == 0 {
        "no unknown usage".into()
    } else {
        format!(
            "{} with unknown usage, charged {}",
            count(usage.unknown_requests, "request", "requests"),
            thousands(usage.unknown_reserved_tokens)
        )
    });
    parts.push(match &run.accounting_coverage {
        None => "pending allowance not reported".into(),
        Some(coverage) => match &coverage.availability {
            EvidenceAvailability::Available if coverage.unsettled_requests == 0 => {
                "nothing pending".into()
            }
            EvidenceAvailability::Available => format!(
                "{} tokens held for {}",
                thousands(coverage.unsettled_allowance),
                count(
                    coverage.unsettled_requests,
                    "unsettled request",
                    "unsettled requests"
                )
            ),
            EvidenceAvailability::Unavailable { reason } => {
                format!("pending allowance unavailable: {}", excerpt(reason, 200))
            }
            EvidenceAvailability::OwnershipUnresolved { reason } => format!(
                "pending allowance ownership unresolved: {}",
                excerpt(reason, 200)
            ),
        },
    });
    parts.join(" · ")
}

pub fn state_words(state: OrchestrationState) -> &'static str {
    match state {
        OrchestrationState::Preparing => "Preparing",
        OrchestrationState::Starting => "Starting",
        OrchestrationState::Running => "Running",
        OrchestrationState::Cancelling => "Cancelling",
        OrchestrationState::Reconciling => "Reconciling",
        OrchestrationState::Terminal => "Ended",
    }
}
/// Reasons are snake_case API words. A released run must read differently from
/// a completed one.
pub fn reason_words(reason: &str) -> String {
    if reason == "operator_released" {
        "released by operator".into()
    } else {
        reason.replace('_', " ")
    }
}
pub fn mode_words(mode: RunMode) -> &'static str {
    match mode {
        RunMode::Experiment => "experiment",
        RunMode::ScriptedSmoke => "scripted smoke test",
        RunMode::LiveSmoke => "live smoke test",
        RunMode::Simulation => "simulation",
    }
}
pub fn phase_words(phase: RunPhase) -> &'static str {
    match phase {
        RunPhase::CapturingWorkspace => "capturing workspace",
        RunPhase::Admitted => "admitted",
        RunPhase::StartingSession => "starting a session",
        RunPhase::Working => "working",
        RunPhase::HandingOff => "handing off",
        RunPhase::Checkpointing => "saving a checkpoint",
        RunPhase::CleaningUp => "cleaning up",
        RunPhase::Terminal => "finished",
    }
}
pub fn memory_words(condition: MemoryCondition) -> &'static str {
    match condition {
        MemoryCondition::SelfWritten => "self-written memento",
        MemoryCondition::SelfPlusEvidence => "self-written memento plus evidence",
    }
}
fn enforcement_words(enforcement: Enforcement) -> &'static str {
    match enforcement {
        Enforcement::Requested => "requested",
        Enforcement::StopAccepted => "stop accepted",
        Enforcement::Stopped => "worker stopped",
        Enforcement::Uncertain => "stop uncertain",
    }
}
fn evidence_words(evidence: &EvidenceAvailability) -> String {
    match evidence {
        EvidenceAvailability::Available => "evidence available".into(),
        EvidenceAvailability::Unavailable { reason } => {
            format!("evidence unavailable: {}", excerpt(reason, 200))
        }
        EvidenceAvailability::OwnershipUnresolved { reason } => {
            format!("evidence ownership unresolved: {}", excerpt(reason, 200))
        }
    }
}
fn cleanup_words(cleanup: &Cleanup) -> String {
    match cleanup {
        Cleanup::Complete => "Cleanup complete".into(),
        // The first error says what was left behind.
        Cleanup::Failed { errors } => match errors.split_first() {
            None => "Cleanup failed".into(),
            Some((first, [])) => format!("Cleanup failed: {}", excerpt(first, 200)),
            Some((first, rest)) => format!(
                "Cleanup failed: {} and {}",
                excerpt(first, 200),
                count(rest.len() as u64, "more error", "more errors")
            ),
        },
        Cleanup::Unknown => "Cleanup unknown".into(),
    }
}
pub(crate) fn count(value: u64, one: &str, many: &str) -> String {
    format!(
        "{} {}",
        thousands(value),
        if value == 1 { one } else { many }
    )
}
/// Seconds as m:ss, or h:mm:ss from an hour.
fn clock(seconds: u64) -> String {
    let (hours, minutes, seconds) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}
pub fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}
