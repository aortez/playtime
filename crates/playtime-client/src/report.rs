//! A run's results, totalled from its records in either archive format: time,
//! model calls by purpose, model errors, tools and their failures,
//! compactions, sessions and the latest workspace. The workbench feeds the
//! totals from its transcript index as records are indexed, and the CLI from
//! a run's event pages; both word the same report, so a run reads the same
//! everywhere. The totals stay bounded whatever the run's length and never
//! depend on evicted payloads. A total counts records, never model claims.
use crate::words::{capitalized, clock, count, duration_words, excerpt, size, thousands};
use playtime_core::{GenerationUsage, RequestPurpose};
use playtime_protocol::{
    api::{Artifact, Checkpoint, ContentAvailability},
    events::{DecodedEvent, EventEnvelope, EventPayload, KnownEvent, decode_event},
    execution::{Fact, Observation},
    observations::{DecodedObservation, KnownObservation, ObservationData, ObservationPayload},
};
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BinaryHeap, VecDeque};

/// Settled calls keep this many durations per purpose for the median: the
/// run's first, whatever order their records arrive in.
const DURATIONS: usize = 8_192;
/// Calls whose request or end hasn't arrived yet. A call's records arrive
/// together, so this is far more than a page holds.
pub const OPEN_CALLS: usize = 4_096;
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
/// Settled calls remembered by request identity. The continuous format
/// records a settlement fact a few records after the observation that
/// settles the call, so in sequence order the fact finds its call settled:
/// it adds the usage the call still lacks, or nothing.
const RECENT: usize = 1_024;

/// What one record says about the run's results.
#[derive(Debug, Default)]
pub struct Update {
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
    /// Controller time at an observation.
    elapsed_ms: Option<u64>,
    /// A session's ordinal, from its start, its memento or its checkpoint.
    ordinal: Option<u64>,
    /// A checkpoint the run's stop didn't interrupt.
    completed: Option<u64>,
    /// A compaction harness is configured.
    compacting: bool,
    compaction: Option<CompactionRecord>,
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
#[derive(Clone, Copy, Debug)]
enum CompactionRecord {
    Planned(u64),
    Applied(u64),
    Deferred,
    Rejected,
}
impl Update {
    /// What a raw event says, decoded here. A page the client read has
    /// already decoded each event, so a failure is an invalid response.
    pub fn decode(event: &EventEnvelope) -> crate::Result<Self> {
        let json = serde_json::to_string(event).map_err(|_| crate::Error::InvalidResponse)?;
        let decoded = decode_event(&json).map_err(|_| crate::Error::InvalidResponse)?;
        Ok(Self::decoded(&decoded))
    }
    /// What a decoded event says. Control events and unknown kinds say
    /// nothing; the memento format's `request_not_dispatched` is an
    /// observation, and the continuous format's a fact.
    pub fn decoded(decoded: &DecodedEvent) -> Self {
        match decoded.payload() {
            EventPayload::Known(known) => Self::event(known),
            EventPayload::Execution(record) => match &record.fact {
                Fact::Observation(o) => Self::execution(o),
                fact => Self::fact(fact),
            },
            EventPayload::Observation(observation) => Self::observation(observation),
            EventPayload::Control(_) | EventPayload::Unknown { .. } => Self::default(),
        }
    }
    pub fn execution(o: &Observation) -> Self {
        let value = match &o.data {
            ObservationData::Inline { value } => Some(value),
            _ => None,
        };
        let mut update = Self {
            elapsed_ms: Some(o.elapsed_ms),
            ..Self::default()
        };
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
        let ordinal = || value.and_then(|v| v["ordinal"].as_u64());
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
            // The header counts a harness as configured by its trigger.
            "compaction_configured" => {
                update.compacting =
                    value.is_some_and(|v| v["settings"]["trigger_percent"].as_u64().is_some());
            }
            "compaction_planned" => update.compaction = ordinal().map(CompactionRecord::Planned),
            "compaction_applied" => {
                update.applied_ms = Some(o.elapsed_ms);
                update.compaction = ordinal().map(CompactionRecord::Applied);
            }
            "compaction_deferred" => update.compaction = Some(CompactionRecord::Deferred),
            "compaction_rejected" => update.compaction = Some(CompactionRecord::Rejected),
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
    pub fn fact(fact: &Fact) -> Self {
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
    pub fn observation(observation: &DecodedObservation) -> Self {
        let o = observation.observation();
        let mut update = Self {
            elapsed_ms: Some(o.elapsed_ms),
            ..Self::default()
        };
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
                KnownObservation::MemoryReceived { ordinal, .. } => {
                    update.ordinal = Some(ordinal.get().into());
                }
                KnownObservation::ControllerCheckpoint {
                    ordinal,
                    interrupted,
                    workspace,
                    ..
                } => {
                    update.ordinal = Some(ordinal.get().into());
                    update.completed = (!interrupted).then_some(ordinal.get().into());
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
    pub fn event(event: &KnownEvent) -> Self {
        let mut update = Self::default();
        match event {
            KnownEvent::SessionStarted { ordinal, .. } => {
                update.ordinal = Some(ordinal.get().into());
            }
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
    pub fn tracked_bytes(&self) -> usize {
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
impl Purpose {
    fn usage(&mut self, usage: Usage, ms: u64) {
        match usage {
            Usage::Known(tokens) => {
                self.known_calls += 1;
                self.known_tokens = self.known_tokens.saturating_add(tokens);
                self.known_ms = self.known_ms.saturating_add(ms);
            }
            Usage::Unknown(reserved) => {
                self.unknown_calls += 1;
                self.reserved_tokens = self.reserved_tokens.saturating_add(reserved);
                self.unknown_ms = self.unknown_ms.saturating_add(ms);
            }
        }
    }
}
/// A settled call, for a usage record that arrives after it.
struct Settled {
    purpose: usize,
    ms: u64,
    has_usage: bool,
}
/// A count and the first record's sequence.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub struct Counted {
    pub count: u64,
    #[serde(rename = "first_sequence")]
    pub first: u64,
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
    fn some(self) -> Option<Self> {
        (self.count > 0).then_some(self)
    }
}

/// Records arrive in any order: earlier history loads while live records
/// arrive. A call's records pair by request identity, and each total keeps
/// the first record it counts, for the Results tab to jump to.
#[derive(Default)]
pub struct Totals {
    open: BTreeMap<String, Open>,
    /// The open calls in arrival order.
    arrivals: BTreeMap<u64, String>,
    arrived: u64,
    /// Calls dropped from the open calls at their bound.
    dropped: u64,
    /// The last `RECENT` settled calls, and their order.
    recent: BTreeMap<String, Settled>,
    recent_order: VecDeque<String>,
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
    /// The latest observation's sequence and controller time.
    latest: Option<(u64, u64)>,
    /// The highest session ordinal recorded.
    max_ordinal: u64,
    /// The highest uninterrupted checkpoint. Sessions run in order and an
    /// interrupted checkpoint is the run's last, so this counts the sessions
    /// completed.
    completed: u64,
    /// A compaction harness or record has been indexed.
    compacting: bool,
    /// The highest ordinal planned and the highest applied; the deferrals
    /// and rejections counted.
    compactions: [u64; 4],
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
    pub fn apply(&mut self, sequence: u64, update: Update) {
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
        if let Some(elapsed) = update.elapsed_ms
            && self.latest.is_none_or(|(latest, _)| sequence > latest)
        {
            self.latest = Some((sequence, elapsed));
        }
        if let Some(ordinal) = update.ordinal {
            self.max_ordinal = self.max_ordinal.max(ordinal);
        }
        if let Some(ordinal) = update.completed {
            self.completed = self.completed.max(ordinal);
        }
        self.compacting |= update.compacting;
        if let Some(record) = update.compaction {
            self.compacting = true;
            let [planned, applied, deferred, rejected] = &mut self.compactions;
            match record {
                CompactionRecord::Planned(ordinal) => *planned = (*planned).max(ordinal),
                CompactionRecord::Applied(ordinal) => *applied = (*applied).max(ordinal),
                CompactionRecord::Deferred => *deferred += 1,
                CompactionRecord::Rejected => *rejected += 1,
            }
        }
    }
    fn call(&mut self, sequence: u64, call: Call) {
        // Usage alone, for a call already settled, adds what the call lacks.
        let usage_only =
            call.requested_ms.is_none() && call.ended_ms.is_none() && !call.not_dispatched;
        if usage_only
            && !self.open.contains_key(&call.request_id)
            && let Some(settled) = self.recent.get_mut(&call.request_id)
        {
            if !settled.has_usage
                && let Some(usage) = call.usage
            {
                settled.has_usage = true;
                self.purposes[settled.purpose].usage(usage, settled.ms);
            }
            return;
        }
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
        let index = purpose_index(call.purpose);
        let purpose = &mut self.purposes[index];
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
        if let Some(usage) = call.usage {
            purpose.usage(usage, ms);
        }
        let settled = Settled {
            purpose: index,
            ms,
            has_usage: call.usage.is_some(),
        };
        if self
            .recent
            .insert(call.request_id.clone(), settled)
            .is_none()
        {
            self.recent_order.push_back(call.request_id);
            if self.recent_order.len() > RECENT
                && let Some(oldest) = self.recent_order.pop_front()
            {
                self.recent.remove(&oldest);
            }
        }
    }
    fn sum(&self, field: impl Fn(&Purpose) -> u64) -> u64 {
        self.purposes
            .iter()
            .fold(0, |sum, p| sum.saturating_add(field(p)))
    }
    /// The run's results so far, once every earlier record is counted.
    /// `ended` says whether an open call is still in progress or never
    /// settled, and whether the latest workspace is the final one.
    pub fn report(&self, complete: bool, ended: bool) -> Report {
        if !complete {
            return Report {
                ended,
                ..Report::default()
            };
        }
        let model_ms = self.sum(|p| p.total_ms);
        let by_purpose = |purpose| self.purposes[purpose_index(purpose)].total_ms;
        // Usage alone isn't a call: its settlement observation or omission
        // carries the call's end.
        let (mut waiting, mut unrequested) = (Counted::default(), Counted::default());
        for open in self.open.values() {
            if open.call.requested_ms.is_some() {
                waiting.add(open.first);
            } else if open.call.ended_ms.is_some() {
                unrequested.add(open.first);
            }
        }
        let unknown_calls = self.sum(|p| p.unknown_calls);
        let mut kinds: Vec<ErrorKind> = self
            .errors
            .iter()
            .map(|(kind, counted)| ErrorKind {
                kind: kind.clone(),
                count: counted.count,
                first_sequence: counted.first,
            })
            .collect();
        kinds.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.kind.cmp(&b.kind)));
        let errors = kinds.iter().fold(self.other_errors, |sum, kind| {
            sum.saturating_add(kind.count)
        });
        let mut names: Vec<ToolCalls> = self
            .tools
            .iter()
            .map(|(name, calls)| ToolCalls {
                name: name.clone(),
                calls: *calls,
            })
            .collect();
        names.sort_by(|a, b| b.calls.cmp(&a.calls).then_with(|| a.name.cmp(&b.name)));
        let mut failures: Vec<Failure> = self
            .faults
            .iter()
            .map(|((tool, message), counted)| Failure {
                tool: tool.clone(),
                message: message.clone(),
                count: counted.count,
                first_sequence: counted.first,
            })
            .collect();
        failures.sort_by(|a, b| {
            b.count
                .cmp(&a.count)
                .then_with(|| (&a.tool, &a.message).cmp(&(&b.tool, &b.message)))
        });
        let mut applied_at_ms = self.applied.clone();
        applied_at_ms.sort_unstable();
        let [planned, applied, deferred, rejected] = self.compactions;
        Report {
            complete: true,
            ended,
            time: self
                .latest
                .filter(|_| model_ms > 0)
                .map(|(_, elapsed_ms)| Time {
                    elapsed_ms,
                    model_ms,
                    work_ms: by_purpose(Some(RequestPurpose::Work)),
                    handoff_ms: by_purpose(Some(RequestPurpose::Handoff)),
                    compaction_ms: by_purpose(Some(RequestPurpose::Compaction)),
                    retained_ms: by_purpose(None),
                    other_ms: elapsed_ms.saturating_sub(model_ms),
                }),
            calls: Calls {
                work: self.purpose(Some(RequestPurpose::Work)),
                handoff: self.purpose(Some(RequestPurpose::Handoff)),
                compaction: self.purpose(Some(RequestPurpose::Compaction)),
                retained: self.purpose(None),
                unknown_usage: (unknown_calls > 0).then(|| UnknownUsage {
                    calls: unknown_calls,
                    reserved_tokens: self.sum(|p| p.reserved_tokens),
                    ms: self.sum(|p| p.unknown_ms),
                }),
                never_dispatched: self.not_dispatched.some(),
                open: waiting.some(),
                unrequested: unrequested.some(),
                dropped: self.dropped,
            },
            errors: (errors > 0).then_some(Errors {
                total: errors,
                kinds,
                other_kinds: self.other_errors,
                longest_parse_streak: self.error_streak,
            }),
            tools: Tools {
                calls: self
                    .tools
                    .values()
                    .fold(self.other_tools, |sum, n| sum.saturating_add(*n)),
                names,
                other: self.other_tools,
                failed: self.failed,
                exited_non_zero: self.exited,
                timed_out: self.timed_out,
                failures,
                other_failures: self.other_faults,
            },
            compactions: self.compacting.then(|| Compactions {
                planned,
                applied,
                deferred,
                rejected,
                more_applied: self
                    .applied_count
                    .count
                    .saturating_sub(applied_at_ms.len() as u64),
                applied_at_ms,
                first_applied_sequence: (self.applied_count.count > 0)
                    .then_some(self.applied_count.first),
            }),
            sessions: (self.max_ordinal > 0
                || self.completed > 0
                || self.handoffs_missing.count > 0)
                .then(|| Sessions {
                    started: self.max_ordinal,
                    completed: self.completed,
                    handoffs_missing: self.handoffs_missing.some(),
                }),
            workspace: self
                .workspace
                .map(|(sequence, bytes)| Captured { bytes, sequence }),
        }
    }
    /// The Results tab's sections, once earlier history has loaded.
    pub fn results(&self, complete: bool, ended: bool) -> Results {
        self.report(complete, ended).results()
    }
    fn purpose(&self, purpose: Option<RequestPurpose>) -> Option<PurposeCalls> {
        let p = &self.purposes[purpose_index(purpose)];
        if p.calls == 0 {
            return None;
        }
        let mut durations: Vec<u32> = p.durations.iter().map(|(_, ms)| *ms).collect();
        let median_ms = (!durations.is_empty()).then(|| {
            let middle = durations.len() / 2;
            u64::from(*durations.select_nth_unstable(middle).1)
        });
        Some(PurposeCalls {
            calls: p.calls,
            median_ms,
            median_of: (p.calls > durations.len() as u64).then_some(durations.len() as u64),
            longest_ms: p.max_ms,
            known_calls: p.known_calls,
            known_tokens: p.known_tokens,
            known_ms: p.known_ms,
            tokens_per_second: (p.known_ms >= 1000 && p.known_tokens > 0)
                .then(|| p.known_tokens as f64 / (p.known_ms as f64 / 1000.0)),
            unknown_calls: p.unknown_calls,
            reserved_tokens: p.reserved_tokens,
            unknown_ms: p.unknown_ms,
        })
    }
}

/// A run's results, typed. Each part is present once something counts
/// toward it; a rate or median is left out rather than guessed.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Report {
    /// Every earlier record is counted, so the totals are the run's so far.
    /// Without it the report is empty.
    pub complete: bool,
    /// The run has ended, so an open call never settled and the latest
    /// workspace is the final one.
    pub ended: bool,
    pub time: Option<Time>,
    pub calls: Calls,
    pub errors: Option<Errors>,
    pub tools: Tools,
    /// Present once a compaction harness is configured.
    pub compactions: Option<Compactions>,
    /// Present for a memento run, which runs in sessions.
    pub sessions: Option<Sessions>,
    /// The latest workspace captured or saved.
    pub workspace: Option<Captured>,
}
/// The run's time, once a model call has settled: the share in model
/// requests by purpose, the rest in tools and the controller.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub struct Time {
    /// Controller time at the latest observation.
    pub elapsed_ms: u64,
    pub model_ms: u64,
    pub work_ms: u64,
    pub handoff_ms: u64,
    pub compaction_ms: u64,
    /// Memento requests whose payload, with its handoff flag, is retained
    /// elsewhere.
    pub retained_ms: u64,
    pub other_ms: u64,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Calls {
    pub work: Option<PurposeCalls>,
    pub handoff: Option<PurposeCalls>,
    pub compaction: Option<PurposeCalls>,
    /// Memento requests whose payload is retained elsewhere, so their
    /// purpose is unknown.
    pub retained: Option<PurposeCalls>,
    /// Calls settled without the server reporting their usage, across
    /// purposes.
    pub unknown_usage: Option<UnknownUsage>,
    /// Requests the controller never dispatched.
    pub never_dispatched: Option<Counted>,
    /// Calls with their request but no end: in progress while the run goes
    /// on, unsettled once it has ended.
    pub open: Option<Counted>,
    /// Settlements whose request was never recorded.
    pub unrequested: Option<Counted>,
    /// Calls dropped at the bound of open calls, each counted once.
    pub dropped: u64,
}
/// The settled calls of one purpose.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct PurposeCalls {
    pub calls: u64,
    /// The upper-middle duration of the calls kept for the median.
    pub median_ms: Option<u64>,
    /// How many of the run's first calls the median is over, when fewer
    /// than all of them.
    pub median_of: Option<u64>,
    pub longest_ms: u64,
    /// Calls whose usage the server reported, their tokens and their time.
    pub known_calls: u64,
    pub known_tokens: u64,
    pub known_ms: u64,
    /// Over the calls with known usage, once they add up to a second.
    pub tokens_per_second: Option<f64>,
    /// Calls settled with unknown usage, the tokens charged for them and
    /// their time.
    pub unknown_calls: u64,
    pub reserved_tokens: u64,
    pub unknown_ms: u64,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub struct UnknownUsage {
    pub calls: u64,
    pub reserved_tokens: u64,
    pub ms: u64,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Errors {
    pub total: u64,
    /// By count, then by kind.
    pub kinds: Vec<ErrorKind>,
    /// Errors of kinds past the bound of kinds counted.
    pub other_kinds: u64,
    /// The controller's longest run of consecutive parse errors.
    pub longest_parse_streak: u32,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct ErrorKind {
    pub kind: String,
    pub count: u64,
    pub first_sequence: u64,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Tools {
    pub calls: u64,
    /// By calls, then by name.
    pub names: Vec<ToolCalls>,
    /// Calls of tools past the bound of names counted.
    pub other: u64,
    pub failed: u64,
    pub exited_non_zero: u64,
    pub timed_out: u64,
    /// Failures by tool and message, by count.
    pub failures: Vec<Failure>,
    /// Failures past the bound of messages counted.
    pub other_failures: u64,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct ToolCalls {
    pub name: String,
    pub calls: u64,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Failure {
    pub tool: String,
    /// The error's first line, when the result gave one.
    pub message: Option<String>,
    pub count: u64,
    pub first_sequence: u64,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Compactions {
    /// The highest ordinal planned and the highest applied.
    pub planned: u64,
    pub applied: u64,
    pub deferred: u64,
    pub rejected: u64,
    /// Controller times of the compactions applied, in order, up to a bound.
    pub applied_at_ms: Vec<u64>,
    pub more_applied: u64,
    pub first_applied_sequence: Option<u64>,
}
/// Sessions as the run header counts them: a session the stop cut off
/// isn't completed.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Sessions {
    pub started: u64,
    pub completed: u64,
    /// Sessions whose handoff the model skipped.
    pub handoffs_missing: Option<Counted>,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub struct Captured {
    pub bytes: u64,
    /// The record that captured or saved it.
    pub sequence: u64,
}

impl Report {
    /// The report worded, section by section, as the Results tab shows it.
    pub fn results(&self) -> Results {
        if !self.complete {
            return Results::default();
        }
        let mut sections = vec![];
        let mut add = |title: &'static str, lines: Vec<Line>| {
            if !lines.is_empty() {
                sections.push(Section { title, lines });
            }
        };
        add("Time", self.time.as_ref().map_or_else(Vec::new, time_lines));
        add("Model calls", call_lines(&self.calls, self.ended));
        add(
            "Model errors",
            self.errors.as_ref().map_or_else(Vec::new, error_lines),
        );
        add("Tools", tool_lines(&self.tools));
        add(
            "Compactions",
            self.compactions
                .as_ref()
                .map_or_else(Vec::new, compaction_lines),
        );
        add(
            "Sessions",
            self.sessions.as_ref().map_or_else(Vec::new, session_lines),
        );
        add(
            if self.ended {
                "Final workspace"
            } else {
                "Latest workspace"
            },
            self.workspace
                .map(|captured| Line {
                    text: format!("{} captured", size(captured.bytes)),
                    jump: Some(captured.sequence),
                })
                .into_iter()
                .collect(),
        );
        Results {
            complete: true,
            sections,
        }
    }
}
fn time_lines(time: &Time) -> Vec<Line> {
    let percent =
        u128::from(time.model_ms) * 100 / u128::from(time.elapsed_ms.max(1).max(time.model_ms));
    let mut parts: Vec<String> = [
        (time.work_ms, "work"),
        (time.handoff_ms, "handoff"),
        (time.compaction_ms, "compaction"),
        (time.retained_ms, "retained requests"),
    ]
    .into_iter()
    .filter(|(total, _)| *total > 0)
    .map(|(total, name)| format!("{name} {}", clock(total / 1000)))
    .collect();
    parts.push(format!(
        "tools and controller {}",
        clock(time.other_ms / 1000)
    ));
    vec![
        Line::plain(format!(
            "{} elapsed · {percent}% in model requests",
            clock(time.elapsed_ms / 1000),
        )),
        Line::plain(capitalized(&parts.join(" · "))),
    ]
}
fn call_lines(calls: &Calls, ended: bool) -> Vec<Line> {
    let mut lines = vec![];
    for (purpose, one, many) in [
        (&calls.work, "work call", "work calls"),
        (&calls.handoff, "handoff call", "handoff calls"),
        (&calls.compaction, "compaction call", "compaction calls"),
        (
            &calls.retained,
            "call with a retained request",
            "calls with retained requests",
        ),
    ] {
        let Some(p) = purpose else {
            continue;
        };
        let mut parts = vec![count(p.calls, one, many)];
        if let Some(median) = p.median_ms {
            parts.push(format!(
                "median {}{}",
                duration_words(median),
                p.median_of
                    .map(|of| format!(" of the first {}", thousands(of)))
                    .unwrap_or_default()
            ));
            parts.push(format!("longest {}", duration_words(p.longest_ms)));
        }
        if p.known_calls > 0 {
            parts.push(format!("{} tokens", thousands(p.known_tokens)));
            if let Some(rate) = p.tokens_per_second {
                parts.push(format!("{rate:.0} tokens/s"));
            }
        }
        lines.push(Line::plain(parts.join(" · ")));
    }
    if let Some(unknown) = &calls.unknown_usage {
        let mut text = format!(
            "{} with unknown usage · {} tokens charged",
            count(unknown.calls, "call", "calls"),
            thousands(unknown.reserved_tokens)
        );
        if unknown.ms >= 1000 {
            text.push_str(&format!(" · {} in those calls", clock(unknown.ms / 1000)));
        }
        lines.push(Line::plain(text));
    }
    if let Some(never) = calls.never_dispatched {
        lines.push(Line {
            text: format!("{} never dispatched", count(never.count, "call", "calls")),
            jump: Some(never.first),
        });
    }
    // An open call with its request waits for its end; one without its
    // request has only its end, which a complete archive never leaves.
    if let Some(waiting) = calls.open {
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
    if let Some(unrequested) = calls.unrequested {
        lines.push(Line {
            text: format!(
                "{} without a recorded request",
                count(unrequested.count, "settled call", "settled calls")
            ),
            jump: Some(unrequested.first),
        });
    }
    if calls.dropped > 0 {
        lines.push(Line::plain(format!(
            "{} dropped at the bound of {} open calls",
            count(calls.dropped, "call", "calls"),
            thousands(OPEN_CALLS as u64)
        )));
    }
    lines
}
fn error_lines(errors: &Errors) -> Vec<Line> {
    let mut parts: Vec<String> = errors
        .kinds
        .iter()
        .map(|kind| format!("{} {}", thousands(kind.count), kind.kind.replace('_', " ")))
        .collect();
    if errors.other_kinds > 0 {
        parts.push(format!("{} of other kinds", thousands(errors.other_kinds)));
    }
    let mut text = format!(
        "{}: {}",
        count(errors.total, "model error", "model errors"),
        parts.join(" · ")
    );
    if errors.longest_parse_streak > 1 {
        text.push_str(&format!(
            " · longest parse streak {}",
            errors.longest_parse_streak
        ));
    }
    vec![Line {
        text,
        jump: errors.kinds.iter().map(|kind| kind.first_sequence).min(),
    }]
}
fn tool_lines(tools: &Tools) -> Vec<Line> {
    let mut lines = vec![];
    if tools.calls > 0 {
        let mut names: Vec<String> = tools
            .names
            .iter()
            .map(|tool| format!("{} {}", thousands(tool.calls), tool.name))
            .collect();
        if tools.other > 0 {
            names.push(format!("{} other", thousands(tools.other)));
        }
        lines.push(Line::plain(format!(
            "{}: {}",
            count(tools.calls, "tool call", "tool calls"),
            names.join(" · ")
        )));
    }
    let outcomes: Vec<String> = [
        (tools.failed, "failed"),
        (tools.exited_non_zero, "exited non-zero"),
        (tools.timed_out, "timed out"),
    ]
    .into_iter()
    .filter(|(n, _)| *n > 0)
    .map(|(n, what)| format!("{} {what}", thousands(n)))
    .collect();
    if !outcomes.is_empty() {
        lines.push(Line::plain(outcomes.join(" · ")));
    }
    for failure in &tools.failures {
        lines.push(Line {
            text: match &failure.message {
                Some(message) => {
                    format!("{} × {}: {message}", thousands(failure.count), failure.tool)
                }
                None => format!(
                    "{} × {} failed without a message",
                    thousands(failure.count),
                    failure.tool
                ),
            },
            jump: Some(failure.first_sequence),
        });
    }
    if tools.other_failures > 0 {
        lines.push(Line::plain(format!(
            "{} with other messages",
            count(tools.other_failures, "more failure", "more failures")
        )));
    }
    lines
}
fn compaction_lines(c: &Compactions) -> Vec<Line> {
    let mut lines = vec![];
    if [c.planned, c.applied, c.deferred, c.rejected]
        .iter()
        .any(|n| *n > 0)
    {
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
    if !c.applied_at_ms.is_empty() {
        let mut text = format!(
            "Applied at {}",
            c.applied_at_ms
                .iter()
                .map(|ms| clock(ms / 1000))
                .collect::<Vec<_>>()
                .join(", ")
        );
        if c.more_applied > 0 {
            text.push_str(&format!(" and {} more", thousands(c.more_applied)));
        }
        lines.push(Line {
            text,
            jump: c.first_applied_sequence,
        });
    }
    lines
}
fn session_lines(sessions: &Sessions) -> Vec<Line> {
    let mut parts = vec![
        format!("{} started", count(sessions.started, "session", "sessions")),
        format!("{} completed", thousands(sessions.completed)),
    ];
    if let Some(missing) = sessions.handoffs_missing {
        parts.push(format!(
            "{} missing",
            count(missing.count, "handoff", "handoffs")
        ));
    }
    vec![Line {
        text: parts.join(" · "),
        jump: sessions.handoffs_missing.map(|missing| missing.first),
    }]
}

/// The report's prepared sections.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Results {
    /// Earlier history has loaded, so the sections total the whole run so far.
    pub complete: bool,
    pub sections: Vec<Section>,
}
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Section {
    pub title: &'static str,
    pub lines: Vec<Line>,
}
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Line {
    pub text: String,
    /// The first record the line counts, to jump to.
    #[serde(rename = "first_sequence")]
    pub jump: Option<u64>,
}
impl Line {
    pub fn plain(text: String) -> Self {
        Self { text, jump: None }
    }
}
impl Results {
    /// Adds a workspace listing: to the captured workspace's section when a
    /// record captured it, else under its own heading, which names the
    /// latest checkpoint while the run goes on. A final capture without
    /// files says so rather than listing an empty workspace.
    pub fn list_workspace(&mut self, ended: bool, listing: &Listing) {
        let mut lines = vec![];
        if listing.files == 0 && listing.source == ListingSource::FinalCapture {
            lines.push(Line::plain(
                if listing.incomplete {
                    "Listing limit reached before any final workspace file."
                } else {
                    "No final workspace publication was found."
                }
                .into(),
            ));
        } else {
            lines.push(Line::plain(listing.words()));
            if listing.incomplete {
                lines.push(Line::plain(
                    "Listing limit reached; this listing is incomplete.".into(),
                ));
            }
        }
        match self
            .sections
            .iter_mut()
            .find(|section| matches!(section.title, "Final workspace" | "Latest workspace"))
        {
            Some(section) => section.lines.extend(lines),
            None => self.sections.push(Section {
                title: if ended {
                    "Final workspace"
                } else {
                    "Latest checkpoint"
                },
                lines,
            }),
        }
    }
}

/// Whether a file of this name is previewed as an image.
pub fn image_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    [".png", ".jpg", ".jpeg", ".webp", ".gif"]
        .iter()
        .any(|suffix| lower.ends_with(suffix))
}
/// Whether a file of this name is counted as a script or source file.
pub fn script_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    [
        ".c", ".cpp", ".go", ".h", ".js", ".py", ".rb", ".rs", ".sh", ".ts",
    ]
    .iter()
    .any(|suffix| lower.ends_with(suffix))
}
/// Where a workspace listing comes from.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ListingSource {
    /// A continuous run's final capture.
    FinalCapture,
    /// A memento run's last published checkpoint.
    Checkpoint,
}
/// A workspace's files counted by kind. Each file's content, when retained,
/// adds its size; a file whose content wasn't retained counts as unavailable.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Listing {
    pub source: ListingSource,
    pub files: u64,
    pub bytes: u64,
    pub images: u64,
    pub scripts: u64,
    pub unavailable: u64,
    /// The listing reached its bound before the end.
    pub incomplete: bool,
}
impl Listing {
    /// An empty listing, complete until said otherwise.
    pub fn new(source: ListingSource) -> Self {
        Self {
            source,
            files: 0,
            bytes: 0,
            images: 0,
            scripts: 0,
            unavailable: 0,
            incomplete: false,
        }
    }
    /// Counts a file by its name and, when its content was retained, its
    /// size.
    pub fn add(&mut self, name: &str, size: Option<u64>) {
        self.files += 1;
        if script_name(name) {
            self.scripts += 1;
        }
        match size {
            Some(size) => {
                self.bytes = self.bytes.saturating_add(size);
                if image_name(name) {
                    self.images += 1;
                }
            }
            None => self.unavailable += 1,
        }
    }
    /// A continuous run's retained final files, from its artifact listing.
    pub fn from_artifacts(files: &[Artifact], complete: bool) -> Self {
        let mut listing = Self::new(ListingSource::FinalCapture);
        for artifact in files
            .iter()
            .filter(|artifact| artifact.kind == "file_version")
        {
            listing.add(&artifact.name, Some(artifact.content.size_bytes));
        }
        listing.incomplete = !complete;
        listing
    }
    /// A memento run's published checkpoint.
    pub fn from_checkpoint(checkpoint: &Checkpoint) -> Self {
        let mut listing = Self::new(ListingSource::Checkpoint);
        for file in checkpoint
            .files
            .iter()
            .filter(|file| !matches!(file.kind.as_str(), "5" | "directory"))
        {
            listing.add(
                &file.name,
                match &file.content {
                    ContentAvailability::Available { content, .. } => Some(content.size_bytes),
                    ContentAvailability::Unavailable { .. } => None,
                },
            );
        }
        listing
    }
    /// The counts worded, such as `40 files · 12.6 MiB · 16 images · 11 scripts`.
    pub fn words(&self) -> String {
        let mut parts = vec![
            count(self.files, "file", "files"),
            crate::words::bytes(self.bytes),
        ];
        for (n, one, many) in [
            (self.images, "image", "images"),
            (self.scripts, "script", "scripts"),
            (self.unavailable, "unavailable", "unavailable"),
        ] {
            if n > 0 {
                parts.push(count(n, one, many));
            }
        }
        parts.join(" · ")
    }
}
