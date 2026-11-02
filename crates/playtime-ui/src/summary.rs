//! A run's results, totalled as records are indexed: time, model calls by
//! purpose, model errors, tools and their failures, compactions, sessions and
//! the latest workspace. Like the progress tracker, the totals stay bounded
//! and never depend on evicted payloads; the Results tab words them. A total
//! counts records, never model claims.
use crate::{
    progress::{Progress, clock, count},
    transcript::{duration_words, excerpt, size, thousands},
};
use playtime_core::{GenerationUsage, RequestPurpose};
use playtime_protocol::{
    events::KnownEvent,
    execution::{Fact, Observation},
    observations::{DecodedObservation, KnownObservation, ObservationData, ObservationPayload},
};
use serde_json::Value;
use std::collections::{BTreeMap, BinaryHeap};

/// Settled calls keep this many durations per purpose for the median: the
/// run's first, whatever order their records arrive in.
const DURATIONS: usize = 8_192;
/// Calls whose request or end hasn't arrived yet. A call's records arrive
/// together, so this is far more than a page holds.
const OPEN_CALLS: usize = 4_096;
/// Distinct model error kinds counted; later kinds count as other kinds.
const ERROR_KINDS: usize = 16;
/// Distinct tool names counted; later names count as other tools.
const TOOL_NAMES: usize = 16;
/// Distinct tool failure messages counted; later ones count as other failures.
const FAULTS: usize = 24;
/// Compaction times kept for the list of when each was applied.
const APPLIED: usize = 64;
/// The bytes of a failure message kept.
const MESSAGE_BYTES: usize = 120;
/// The bytes of an error kind kept.
const KIND_BYTES: usize = 40;

/// What one record says about the run's results.
#[derive(Debug, Default)]
pub(crate) struct Update {
    call: Option<Call>,
    /// A model error's recorded kind and the controller's count of
    /// consecutive parse errors at it.
    error: Option<(String, u32)>,
    /// A tool request's name.
    tool: Option<String>,
    fault: Option<Fault>,
    /// A compaction applied at this controller time.
    applied_ms: Option<u64>,
    handoff_missing: bool,
    /// The bytes of a checkpoint's or the final capture's workspace.
    workspace_bytes: Option<u64>,
}
/// What one record says about a model call, by request identity.
#[derive(Debug, Default)]
struct Call {
    request_id: String,
    /// The recorded purpose. A memento request whose payload is retained
    /// elsewhere keeps its handoff flag there, so it has none here.
    purpose: Option<RequestPurpose>,
    requested_ms: Option<u64>,
    /// Controller time at the call's end: its settlement, which the
    /// controller records with the response or error, or the record that it
    /// was never dispatched.
    ended_ms: Option<u64>,
    /// The request never reached the backend; its reservation was released.
    not_dispatched: bool,
    usage: Option<Usage>,
}
/// The records that open and end a call.
#[derive(Clone, Copy, Debug)]
enum CallRecord {
    Requested,
    Settled,
    NotDispatched,
}
fn call_record(kind: &str) -> Option<CallRecord> {
    match kind {
        "model_request" => Some(CallRecord::Requested),
        "request_settled" => Some(CallRecord::Settled),
        "request_not_dispatched" => Some(CallRecord::NotDispatched),
        _ => None,
    }
}
impl Call {
    fn record(
        request_id: String,
        purpose: Option<RequestPurpose>,
        record: CallRecord,
        elapsed_ms: u64,
    ) -> Self {
        let requested = matches!(record, CallRecord::Requested);
        Self {
            request_id,
            purpose,
            requested_ms: requested.then_some(elapsed_ms),
            ended_ms: (!requested).then_some(elapsed_ms),
            not_dispatched: matches!(record, CallRecord::NotDispatched),
            usage: None,
        }
    }
}
#[derive(Clone, Copy, Debug)]
enum Usage {
    Known(u64),
    /// The tokens charged for a request whose usage the server never reported.
    Unknown(u64),
}
impl From<&GenerationUsage> for Usage {
    fn from(usage: &GenerationUsage) -> Self {
        match usage {
            GenerationUsage::Known { generated_tokens } => Self::Known(*generated_tokens),
            GenerationUsage::Unknown { reserved_tokens } => Self::Unknown(reserved_tokens.get()),
        }
    }
}
#[derive(Debug)]
struct Fault {
    tool: String,
    kind: FaultKind,
    /// The error's first line, for failures that report one.
    message: Option<String>,
}
#[derive(Clone, Copy, Debug)]
enum FaultKind {
    Failed,
    TimedOut,
    Exited,
}
/// How a recorded tool result failed, timed out or exited non-zero, if it did.
fn fault(name: &str, result: &Value) -> Option<Fault> {
    let kind = if result
        .get("error")
        .is_some_and(|error| !error.is_null() && *error != false)
    {
        FaultKind::Failed
    } else if result.get("timed_out").and_then(Value::as_bool) == Some(true) {
        FaultKind::TimedOut
    } else if result
        .get("exit_code")
        .and_then(Value::as_i64)
        .is_some_and(|code| code != 0)
    {
        FaultKind::Exited
    } else {
        return None;
    };
    let message = match kind {
        FaultKind::Failed => result["error"]
            .as_str()
            .and_then(|error| error.lines().next())
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(|line| excerpt(line, MESSAGE_BYTES)),
        FaultKind::TimedOut | FaultKind::Exited => None,
    };
    Some(Fault {
        tool: excerpt(name, 60),
        kind,
        message,
    })
}
/// A settlement's usage, as both archive formats record it.
fn usage(value: &Value) -> Option<Usage> {
    match value["status"].as_str()? {
        "known" => Some(Usage::Known(value["generated_tokens"].as_u64()?)),
        "unknown" => Some(Usage::Unknown(
            value["reserved_tokens"].as_u64().unwrap_or(0),
        )),
        _ => None,
    }
}
impl Update {
    pub(crate) fn execution(o: &Observation) -> Self {
        let value = match &o.data {
            ObservationData::Inline { value } => Some(value),
            _ => None,
        };
        let mut update = Self::default();
        // A call is its request and its end. The purpose comes with both, so
        // a retained payload still counts under it.
        if let Some(record) = call_record(&o.observation_kind)
            && let Some(request) = &o.request
        {
            let mut call = Call::record(
                request.request_id.to_string(),
                Some(request.purpose),
                record,
                o.elapsed_ms,
            );
            if matches!(record, CallRecord::Settled) {
                call.usage = value.and_then(|v| usage(&v["usage"]));
            }
            update.call = Some(call);
        }
        match o.observation_kind.as_str() {
            "model_error" => update.error = Some(error(value)),
            "tool_request" => {
                update.tool = value
                    .and_then(|v| v["name"].as_str())
                    .map(|name| excerpt(name, 60));
            }
            "tool_result" => {
                update.fault = value.and_then(|v| {
                    fault(v["name"].as_str().unwrap_or("unknown tool"), &v["result"])
                });
            }
            "compaction_applied" => update.applied_ms = Some(o.elapsed_ms),
            "workspace_captured" => {
                update.workspace_bytes = value.and_then(|v| v["workspace"]["size_bytes"].as_u64());
            }
            _ => (),
        }
        update
    }
    /// The v2 facts that aren't observations: the final capture and the run's
    /// end; a settlement's usage, which its observation repeats; and the call
    /// records of observations the controller log limit omitted, which keep
    /// their time and request.
    pub(crate) fn fact(fact: &Fact) -> Self {
        let mut update = Self::default();
        match fact {
            Fact::CapturePublished { capture } => {
                update.workspace_bytes = Some(capture.workspace.size_bytes);
            }
            Fact::RunFinished {
                last_capture: Some(capture),
                ..
            } => {
                update.workspace_bytes = Some(capture.workspace.size_bytes);
            }
            Fact::RequestSettled { request, usage } => {
                update.call = Some(Call {
                    request_id: request.request_id.to_string(),
                    purpose: Some(request.purpose),
                    usage: Some(usage.into()),
                    ..Default::default()
                });
            }
            Fact::ObservationOmitted {
                observation_kind,
                elapsed_ms,
                request: Some(request),
                ..
            } => {
                update.call = call_record(observation_kind).map(|record| {
                    Call::record(
                        request.request_id.to_string(),
                        Some(request.purpose),
                        record,
                        *elapsed_ms,
                    )
                });
            }
            _ => (),
        }
        update
    }
    pub(crate) fn observation(observation: &DecodedObservation) -> Self {
        let o = observation.observation();
        let mut update = Self::default();
        // A call is its request and its end, which name the request. The
        // request says whether it asks for the handoff; one whose payload is
        // retained elsewhere keeps that flag there, so it counts on its own
        // line rather than as work.
        let record = call_record(&o.observation_kind);
        let call = |purpose: Option<RequestPurpose>| {
            let record = record?;
            let request_id = o.identity.request_id.as_ref()?;
            Some(Call::record(
                request_id.to_string(),
                purpose,
                record,
                o.elapsed_ms,
            ))
        };
        match observation.payload() {
            ObservationPayload::Known(known) => match known.as_ref() {
                KnownObservation::ModelRequest(request) => {
                    update.call = call(Some(if request.handoff {
                        RequestPurpose::Handoff
                    } else {
                        RequestPurpose::Work
                    }));
                }
                KnownObservation::ModelError {
                    error,
                    consecutive_errors,
                    ..
                } => {
                    update.error = Some((excerpt(&error.kind, KIND_BYTES), *consecutive_errors));
                }
                KnownObservation::RequestSettled { usage, .. } => {
                    update.call = call(None).map(|mut call| {
                        call.usage = Some(usage.into());
                        call
                    });
                }
                KnownObservation::RequestNotDispatched { .. } => update.call = call(None),
                KnownObservation::ToolRequest { name, .. } => {
                    update.tool = Some(excerpt(name, 60));
                }
                KnownObservation::ToolResult { name, result, .. } => {
                    update.fault = fault(name, result);
                }
                KnownObservation::ControllerCheckpoint { workspace, .. } => {
                    update.workspace_bytes = Some(workspace.size_bytes);
                }
                _ => (),
            },
            // A retained payload still names its kind.
            ObservationPayload::Referenced { .. } => match o.observation_kind.as_str() {
                "model_error" => update.error = Some(error(None)),
                _ => update.call = call(None),
            },
            ObservationPayload::Unknown { kind, .. } => {
                update.handoff_missing = kind == "handoff_missing";
            }
        }
        update
    }
    pub(crate) fn event(event: &KnownEvent) -> Self {
        let mut update = Self::default();
        match event {
            KnownEvent::CheckpointPublished { checkpoint } => {
                update.workspace_bytes = Some(checkpoint.workspace.size_bytes);
            }
            KnownEvent::RunFinished {
                last_checkpoint: Some(checkpoint),
                ..
            } => {
                update.workspace_bytes = Some(checkpoint.workspace.size_bytes);
            }
            _ => (),
        }
        update
    }
    /// A call's record waits in the open calls, in two maps, until the call
    /// ends; an error keeps its kind and a failure its message.
    pub(crate) fn tracked_bytes(&self) -> usize {
        self.call
            .as_ref()
            .map_or(0, |call| 128 + 2 * call.request_id.len())
            + self.error.as_ref().map_or(0, |(kind, _)| 32 + kind.len())
            + self.fault.as_ref().map_or(0, |fault| {
                160 + fault.message.as_ref().map_or(0, String::len)
            })
    }
}
/// A model error's recorded kind and consecutive count, or unknown without
/// its payload.
fn error(value: Option<&Value>) -> (String, u32) {
    (
        value
            .and_then(|v| v["error"]["kind"].as_str())
            .map_or_else(|| "unknown".into(), |kind| excerpt(kind, KIND_BYTES)),
        value
            .and_then(|v| v["consecutive_errors"].as_u64())
            .map_or(1, |n| n.min(u32::MAX.into()) as u32),
    )
}

/// A call waiting for the rest of its records.
struct Open {
    /// The first record's sequence, to jump to.
    first: u64,
    /// Its place in arrival order, so the call waiting longest is dropped at
    /// the bound.
    arrived: u64,
    call: Call,
}
#[derive(Default)]
struct Purpose {
    calls: u64,
    total_ms: u64,
    max_ms: u64,
    /// The durations of the run's first `DURATIONS` calls by sequence, for
    /// the median; past the bound the heap drops the latest.
    durations: BinaryHeap<(u64, u32)>,
    known_calls: u64,
    known_tokens: u64,
    /// The durations of the calls with known usage, for the rate.
    known_ms: u64,
    unknown_calls: u64,
    reserved_tokens: u64,
    /// The durations of the calls with unknown usage: those that failed or
    /// were cut off before a response.
    unknown_ms: u64,
}
/// A count and the first record's sequence.
#[derive(Clone, Copy, Default)]
struct Counted {
    count: u64,
    first: u64,
}
impl Counted {
    fn add(&mut self, sequence: u64) {
        self.count += 1;
        self.first = if self.count == 1 {
            sequence
        } else {
            self.first.min(sequence)
        };
    }
}

/// Records arrive in any order: earlier history loads while live records
/// arrive. A call's records pair by request identity, and each total keeps
/// the first record it counts, for the Results tab to jump to.
#[derive(Default)]
pub(crate) struct Totals {
    open: BTreeMap<String, Open>,
    /// The open calls in arrival order.
    arrivals: BTreeMap<u64, String>,
    arrived: u64,
    /// Calls dropped from the open calls at their bound.
    dropped: u64,
    /// Work, handoff, compaction and retained requests.
    purposes: [Purpose; 4],
    not_dispatched: Counted,
    errors: BTreeMap<String, Counted>,
    other_errors: u64,
    error_streak: u32,
    /// Requests by tool name.
    tools: BTreeMap<String, u64>,
    other_tools: u64,
    failed: u64,
    timed_out: u64,
    exited: u64,
    /// Failures by tool and message.
    faults: BTreeMap<(String, Option<String>), Counted>,
    other_faults: u64,
    /// Controller times of the compactions applied, up to `APPLIED`.
    applied: Vec<u64>,
    applied_count: Counted,
    handoffs_missing: Counted,
    /// The latest workspace captured or saved, by sequence.
    workspace: Option<(u64, u64)>,
}
fn purpose_index(purpose: Option<RequestPurpose>) -> usize {
    match purpose {
        Some(RequestPurpose::Work) => 0,
        Some(RequestPurpose::Handoff) => 1,
        Some(RequestPurpose::Compaction) => 2,
        None => 3,
    }
}
impl Totals {
    pub(crate) fn apply(&mut self, sequence: u64, update: Update) {
        if let Some(call) = update.call {
            self.call(sequence, call);
        }
        if let Some((kind, consecutive)) = update.error {
            if self.errors.contains_key(&kind) || self.errors.len() < ERROR_KINDS {
                self.errors.entry(kind).or_default().add(sequence);
            } else {
                self.other_errors += 1;
            }
            self.error_streak = self.error_streak.max(consecutive);
        }
        if let Some(name) = update.tool {
            if self.tools.contains_key(&name) || self.tools.len() < TOOL_NAMES {
                *self.tools.entry(name).or_default() += 1;
            } else {
                self.other_tools += 1;
            }
        }
        if let Some(fault) = update.fault {
            match fault.kind {
                FaultKind::Failed => self.failed += 1,
                FaultKind::TimedOut => self.timed_out += 1,
                FaultKind::Exited => self.exited += 1,
            }
            if matches!(fault.kind, FaultKind::Failed) {
                let key = (fault.tool, fault.message);
                if self.faults.contains_key(&key) || self.faults.len() < FAULTS {
                    self.faults.entry(key).or_default().add(sequence);
                } else {
                    self.other_faults += 1;
                }
            }
        }
        if let Some(ms) = update.applied_ms {
            self.applied_count.add(sequence);
            if self.applied.len() < APPLIED {
                self.applied.push(ms);
            }
        }
        if update.handoff_missing {
            self.handoffs_missing.add(sequence);
        }
        if let Some(bytes) = update.workspace_bytes
            && self.workspace.is_none_or(|(latest, _)| sequence > latest)
        {
            self.workspace = Some((sequence, bytes));
        }
    }
    fn call(&mut self, sequence: u64, call: Call) {
        if !self.open.contains_key(&call.request_id) {
            if self.open.len() >= OPEN_CALLS
                && let Some((_, oldest)) = self.arrivals.pop_first()
            {
                self.open.remove(&oldest);
                self.dropped += 1;
            }
            self.arrived += 1;
            self.arrivals.insert(self.arrived, call.request_id.clone());
            self.open.insert(
                call.request_id.clone(),
                Open {
                    first: sequence,
                    arrived: self.arrived,
                    call: Call {
                        request_id: call.request_id.clone(),
                        ..Default::default()
                    },
                },
            );
        }
        let open = self.open.get_mut(&call.request_id).unwrap();
        open.first = open.first.min(sequence);
        let known = &mut open.call;
        if call.purpose.is_some() {
            known.purpose = call.purpose;
        }
        if call.requested_ms.is_some() {
            known.requested_ms = call.requested_ms;
        }
        if call.ended_ms.is_some() {
            known.ended_ms = call.ended_ms;
        }
        if call.usage.is_some() {
            known.usage = call.usage;
        }
        known.not_dispatched |= call.not_dispatched;
        if known.requested_ms.is_some() && known.ended_ms.is_some() {
            let Open {
                first,
                arrived,
                call,
            } = self.open.remove(&call.request_id).unwrap();
            self.arrivals.remove(&arrived);
            self.settle(first, call);
        }
    }
    /// A call with its request and end indexed adds to its purpose's totals,
    /// unless it was never dispatched.
    fn settle(&mut self, first: u64, call: Call) {
        if call.not_dispatched {
            self.not_dispatched.add(first);
            return;
        }
        let purpose = &mut self.purposes[purpose_index(call.purpose)];
        let ms = call
            .ended_ms
            .unwrap_or(0)
            .saturating_sub(call.requested_ms.unwrap_or(0));
        purpose.calls += 1;
        purpose.total_ms = purpose.total_ms.saturating_add(ms);
        purpose.max_ms = purpose.max_ms.max(ms);
        purpose
            .durations
            .push((first, ms.min(u32::MAX.into()) as u32));
        if purpose.durations.len() > DURATIONS {
            purpose.durations.pop();
        }
        match call.usage {
            Some(Usage::Known(tokens)) => {
                purpose.known_calls += 1;
                purpose.known_tokens = purpose.known_tokens.saturating_add(tokens);
                purpose.known_ms = purpose.known_ms.saturating_add(ms);
            }
            Some(Usage::Unknown(reserved)) => {
                purpose.unknown_calls += 1;
                purpose.reserved_tokens = purpose.reserved_tokens.saturating_add(reserved);
                purpose.unknown_ms = purpose.unknown_ms.saturating_add(ms);
            }
            None => (),
        }
    }
    /// The sections, once earlier history has loaded. `ended` says whether
    /// an open call is still in progress or never settled, and whether the
    /// latest workspace is the final one.
    pub(crate) fn results(&self, progress: &Progress, complete: bool, ended: bool) -> Results {
        if !complete {
            return Results::default();
        }
        let mut sections = vec![];
        let mut add = |title: &'static str, lines: Vec<Line>| {
            if !lines.is_empty() {
                sections.push(Section { title, lines });
            }
        };
        add("Time", self.time(progress));
        add("Model calls", self.calls(ended));
        add("Model errors", self.model_errors());
        add("Tools", self.tool_lines());
        add("Compactions", self.compactions(progress));
        add("Sessions", self.sessions(progress));
        add(
            if ended {
                "Final workspace"
            } else {
                "Latest workspace"
            },
            self.workspace
                .map(|(sequence, bytes)| Line {
                    text: format!("{} captured", size(bytes)),
                    jump: Some(sequence),
                })
                .into_iter()
                .collect(),
        );
        Results {
            complete: true,
            sections,
        }
    }
    fn sum(&self, field: impl Fn(&Purpose) -> u64) -> u64 {
        self.purposes
            .iter()
            .fold(0, |sum, p| sum.saturating_add(field(p)))
    }
    fn time(&self, progress: &Progress) -> Vec<Line> {
        let model_ms = self.sum(|p| p.total_ms);
        let (Some(elapsed), true) = (progress.elapsed_ms, model_ms > 0) else {
            return vec![];
        };
        let percent = u128::from(model_ms) * 100 / u128::from(elapsed.max(1).max(model_ms));
        let mut lines = vec![Line::plain(format!(
            "{} elapsed · {percent}% in model requests",
            clock(elapsed / 1000),
        ))];
        let mut parts: Vec<String> = [
            (Some(RequestPurpose::Work), "work"),
            (Some(RequestPurpose::Handoff), "handoff"),
            (Some(RequestPurpose::Compaction), "compaction"),
            (None, "retained requests"),
        ]
        .into_iter()
        .filter_map(|(purpose, name)| {
            let total = self.purposes[purpose_index(purpose)].total_ms;
            (total > 0).then(|| format!("{name} {}", clock(total / 1000)))
        })
        .collect();
        parts.push(format!(
            "tools and controller {}",
            clock(elapsed.saturating_sub(model_ms) / 1000)
        ));
        lines.push(Line::plain(crate::progress::capitalized(
            &parts.join(" · "),
        )));
        lines
    }
    fn calls(&self, ended: bool) -> Vec<Line> {
        let mut lines = vec![];
        for (purpose, one, many) in [
            (Some(RequestPurpose::Work), "work call", "work calls"),
            (
                Some(RequestPurpose::Handoff),
                "handoff call",
                "handoff calls",
            ),
            (
                Some(RequestPurpose::Compaction),
                "compaction call",
                "compaction calls",
            ),
            (
                None,
                "call with a retained request",
                "calls with retained requests",
            ),
        ] {
            let p = &self.purposes[purpose_index(purpose)];
            if p.calls == 0 {
                continue;
            }
            let mut parts = vec![count(p.calls, one, many)];
            if !p.durations.is_empty() {
                let mut durations: Vec<u32> = p.durations.iter().map(|(_, ms)| *ms).collect();
                let middle = durations.len() / 2;
                let median = *durations.select_nth_unstable(middle).1;
                parts.push(format!(
                    "median {}{}",
                    duration_words(median.into()),
                    if p.calls > durations.len() as u64 {
                        format!(" of the first {}", thousands(durations.len() as u64))
                    } else {
                        String::new()
                    }
                ));
                parts.push(format!("longest {}", duration_words(p.max_ms)));
            }
            if p.known_calls > 0 {
                parts.push(format!("{} tokens", thousands(p.known_tokens)));
                if p.known_ms >= 1000 && p.known_tokens > 0 {
                    parts.push(format!(
                        "{:.0} tokens/s",
                        p.known_tokens as f64 / (p.known_ms as f64 / 1000.0)
                    ));
                }
            }
            lines.push(Line::plain(parts.join(" · ")));
        }
        let unknown = self.sum(|p| p.unknown_calls);
        if unknown > 0 {
            let mut text = format!(
                "{} with unknown usage · {} tokens charged",
                count(unknown, "call", "calls"),
                thousands(self.sum(|p| p.reserved_tokens))
            );
            let unknown_ms = self.sum(|p| p.unknown_ms);
            if unknown_ms >= 1000 {
                text.push_str(&format!(" · {} in those calls", clock(unknown_ms / 1000)));
            }
            lines.push(Line::plain(text));
        }
        if self.not_dispatched.count > 0 {
            lines.push(Line {
                text: format!(
                    "{} never dispatched",
                    count(self.not_dispatched.count, "call", "calls")
                ),
                jump: Some(self.not_dispatched.first),
            });
        }
        // An open call with its request waits for its end; one without its
        // request has only its end, which a complete archive never leaves.
        let (mut waiting, mut unrequested) = (Counted::default(), Counted::default());
        for open in self.open.values() {
            if open.call.requested_ms.is_some() {
                waiting.add(open.first);
            } else {
                unrequested.add(open.first);
            }
        }
        if waiting.count > 0 {
            let calls = count(waiting.count, "call", "calls");
            lines.push(Line {
                text: if ended {
                    format!("{calls} without a recorded settlement")
                } else {
                    format!("{calls} in progress")
                },
                jump: Some(waiting.first),
            });
        }
        if unrequested.count > 0 {
            lines.push(Line {
                text: format!(
                    "{} without a recorded request",
                    count(unrequested.count, "settled call", "settled calls")
                ),
                jump: Some(unrequested.first),
            });
        }
        if self.dropped > 0 {
            lines.push(Line::plain(format!(
                "{} dropped at the bound of {} open calls",
                count(self.dropped, "call", "calls"),
                thousands(OPEN_CALLS as u64)
            )));
        }
        lines
    }
    fn model_errors(&self) -> Vec<Line> {
        let total = self
            .errors
            .values()
            .fold(self.other_errors, |sum, c| sum.saturating_add(c.count));
        if total == 0 {
            return vec![];
        }
        let mut kinds: Vec<(&String, &Counted)> = self.errors.iter().collect();
        kinds.sort_by(|a, b| b.1.count.cmp(&a.1.count).then_with(|| a.0.cmp(b.0)));
        let mut parts: Vec<String> = kinds
            .iter()
            .map(|(kind, counted)| {
                format!("{} {}", thousands(counted.count), kind.replace('_', " "))
            })
            .collect();
        if self.other_errors > 0 {
            parts.push(format!("{} of other kinds", thousands(self.other_errors)));
        }
        let mut text = format!(
            "{}: {}",
            count(total, "model error", "model errors"),
            parts.join(" · ")
        );
        if self.error_streak > 1 {
            text.push_str(&format!(" · longest parse streak {}", self.error_streak));
        }
        vec![Line {
            text,
            jump: kinds.iter().map(|(_, counted)| counted.first).min(),
        }]
    }
    fn tool_lines(&self) -> Vec<Line> {
        let requests = self
            .tools
            .values()
            .fold(self.other_tools, |sum, n| sum.saturating_add(*n));
        let mut lines = vec![];
        if requests > 0 {
            let mut tools: Vec<(&String, &u64)> = self.tools.iter().collect();
            tools.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
            let mut names: Vec<String> = tools
                .iter()
                .map(|(name, requests)| format!("{} {name}", thousands(**requests)))
                .collect();
            if self.other_tools > 0 {
                names.push(format!("{} other", thousands(self.other_tools)));
            }
            lines.push(Line::plain(format!(
                "{}: {}",
                count(requests, "tool call", "tool calls"),
                names.join(" · ")
            )));
        }
        let outcomes: Vec<String> = [
            (self.failed, "failed"),
            (self.exited, "exited non-zero"),
            (self.timed_out, "timed out"),
        ]
        .into_iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, what)| format!("{} {what}", thousands(n)))
        .collect();
        if !outcomes.is_empty() {
            lines.push(Line::plain(outcomes.join(" · ")));
        }
        let mut faults: Vec<(&(String, Option<String>), &Counted)> = self.faults.iter().collect();
        faults.sort_by(|a, b| b.1.count.cmp(&a.1.count).then_with(|| a.0.cmp(b.0)));
        for ((tool, message), counted) in faults {
            lines.push(Line {
                text: match message {
                    Some(message) => format!("{} × {tool}: {message}", thousands(counted.count)),
                    None => format!(
                        "{} × {tool} failed without a message",
                        thousands(counted.count)
                    ),
                },
                jump: Some(counted.first),
            });
        }
        if self.other_faults > 0 {
            lines.push(Line::plain(format!(
                "{} with other messages",
                count(self.other_faults, "more failure", "more failures")
            )));
        }
        lines
    }
    fn compactions(&self, progress: &Progress) -> Vec<Line> {
        let Some(c) = &progress.compactions else {
            return vec![];
        };
        let mut lines = vec![];
        if c.planned + c.applied + c.deferred + c.rejected > 0 {
            let mut parts = vec![format!(
                "{} applied of {} planned",
                thousands(c.applied),
                thousands(c.planned)
            )];
            if c.deferred > 0 {
                parts.push(format!("{} deferred", thousands(c.deferred)));
            }
            if c.rejected > 0 {
                parts.push(format!("{} rejected", thousands(c.rejected)));
            }
            lines.push(Line::plain(parts.join(" · ")));
        } else {
            lines.push(Line::plain("None".into()));
        }
        if !self.applied.is_empty() {
            let mut applied = self.applied.clone();
            applied.sort_unstable();
            let mut text = format!(
                "Applied at {}",
                applied
                    .iter()
                    .map(|ms| clock(ms / 1000))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            let more = self
                .applied_count
                .count
                .saturating_sub(applied.len() as u64);
            if more > 0 {
                text.push_str(&format!(" and {} more", thousands(more)));
            }
            lines.push(Line {
                text,
                jump: Some(self.applied_count.first),
            });
        }
        lines
    }
    /// Sessions started and completed, as the run header counts them: a
    /// session the stop cut off isn't completed.
    fn sessions(&self, progress: &Progress) -> Vec<Line> {
        let sessions = progress.session.unwrap_or(0);
        let completed = progress.completed.unwrap_or(0);
        if sessions == 0 && completed == 0 && self.handoffs_missing.count == 0 {
            return vec![];
        }
        let mut parts = vec![
            format!("{} started", count(sessions, "session", "sessions")),
            format!("{} completed", thousands(completed)),
        ];
        if self.handoffs_missing.count > 0 {
            parts.push(format!(
                "{} missing",
                count(self.handoffs_missing.count, "handoff", "handoffs")
            ));
        }
        vec![Line {
            text: parts.join(" · "),
            jump: (self.handoffs_missing.count > 0).then_some(self.handoffs_missing.first),
        }]
    }
}

/// The Results tab's prepared sections.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Results {
    /// Earlier history has loaded, so the sections total the whole run so far.
    pub complete: bool,
    pub sections: Vec<Section>,
}
#[derive(Clone, Debug, PartialEq)]
pub struct Section {
    pub title: &'static str,
    pub lines: Vec<Line>,
}
#[derive(Clone, Debug, PartialEq)]
pub struct Line {
    pub text: String,
    /// The first record the line counts, to jump to.
    pub jump: Option<u64>,
}
impl Line {
    fn plain(text: String) -> Self {
        Self { text, jump: None }
    }
}
