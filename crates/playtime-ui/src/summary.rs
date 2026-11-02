//! A run's results, totalled as records are indexed: time, model calls by
//! purpose, model errors, tools and their failures, compactions, sessions and
//! the final workspace. Like the progress tracker, the totals stay bounded and
//! never depend on evicted payloads; the Results tab words them. A total counts
//! records, never model claims.
use crate::{
    progress::{Progress, clock, count},
    transcript::{duration_words, excerpt, size, thousands},
};
use playtime_core::RequestPurpose;
use playtime_protocol::{
    events::KnownEvent,
    execution::{Fact, Observation},
    observations::{DecodedObservation, KnownObservation, ObservationData, ObservationPayload},
};
use serde_json::Value;
use std::collections::BTreeMap;

/// Settled calls keep this many durations per purpose for the median.
const DURATIONS: usize = 8_192;
/// Calls whose request or settlement hasn't arrived yet. A call's records
/// arrive together, so this is far more than a page holds.
const OPEN_CALLS: usize = 4_096;
/// Distinct tool names counted; later names count as other tools.
const TOOL_NAMES: usize = 16;
/// Distinct tool failure messages counted; later ones count as other failures.
const FAULTS: usize = 24;
/// Compaction times kept for the list of when each was applied.
const APPLIED: usize = 64;
/// The bytes of a failure message kept.
const MESSAGE_BYTES: usize = 120;

/// What one record says about the run's results.
#[derive(Debug, Default)]
pub(crate) struct Update {
    call: Option<Call>,
    /// A model error's recorded kind and the controller's count of
    /// consecutive errors at it.
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
    purpose: Option<RequestPurpose>,
    requested_ms: Option<u64>,
    /// Controller time at its settlement, which the controller records with
    /// the response or error.
    settled_ms: Option<u64>,
    usage: Option<Usage>,
}
#[derive(Clone, Copy, Debug)]
enum Usage {
    Known(u64),
    /// The tokens charged for a request whose usage the server never reported.
    Unknown(u64),
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
        // A call is its request and its settlement. The purpose comes with
        // both, so a retained payload still counts under it.
        let call = |settled: bool| {
            o.request.as_ref().map(|request| Call {
                request_id: request.request_id.to_string(),
                purpose: Some(request.purpose),
                requested_ms: (!settled).then_some(o.elapsed_ms),
                settled_ms: settled.then_some(o.elapsed_ms),
                usage: None,
            })
        };
        match o.observation_kind.as_str() {
            "model_request" => update.call = call(false),
            "model_error" => update.error = Some(error(value)),
            "request_settled" => {
                update.call = call(true).map(|mut call| {
                    call.usage = value.and_then(|v| usage(&v["usage"]));
                    call
                });
            }
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
    /// The v2 facts that aren't observations. A settlement fact repeats the
    /// settlement observation, which carries the controller time, so it adds
    /// nothing here.
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
            _ => (),
        }
        update
    }
    pub(crate) fn observation(observation: &DecodedObservation) -> Self {
        let o = observation.observation();
        let mut update = Self::default();
        // A call is its request and its settlement, which name the request.
        // The request says whether it asks for the handoff; a request whose
        // payload is retained elsewhere counts as work.
        let call = |settled: bool| {
            o.identity.request_id.as_ref().map(|request_id| Call {
                request_id: request_id.to_string(),
                purpose: None,
                requested_ms: (!settled).then_some(o.elapsed_ms),
                settled_ms: settled.then_some(o.elapsed_ms),
                usage: None,
            })
        };
        match observation.payload() {
            ObservationPayload::Known(known) => match known.as_ref() {
                KnownObservation::ModelRequest(request) => {
                    update.call = Some(Call {
                        request_id: request.request_id.to_string(),
                        purpose: Some(if request.handoff {
                            RequestPurpose::Handoff
                        } else {
                            RequestPurpose::Work
                        }),
                        requested_ms: Some(o.elapsed_ms),
                        settled_ms: None,
                        usage: None,
                    });
                }
                KnownObservation::ModelError {
                    error,
                    consecutive_errors,
                    ..
                } => {
                    update.error = Some((error.kind.clone(), *consecutive_errors));
                }
                KnownObservation::RequestSettled { usage, .. } => {
                    update.call = call(true).map(|mut call| {
                        call.usage = Some(match usage {
                            playtime_core::GenerationUsage::Known { generated_tokens } => {
                                Usage::Known(*generated_tokens)
                            }
                            playtime_core::GenerationUsage::Unknown { reserved_tokens } => {
                                Usage::Unknown(reserved_tokens.get())
                            }
                        });
                        call
                    });
                }
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
                "model_request" => update.call = call(false),
                "model_error" => update.error = Some(error(None)),
                _ => (),
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
    /// A call's record waits in the open calls until the call settles, and a
    /// failure keeps its message.
    pub(crate) fn tracked_bytes(&self) -> usize {
        self.call
            .as_ref()
            .map_or(0, |call| 96 + call.request_id.len())
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
            .map_or_else(|| "unknown".into(), |kind| excerpt(kind, 40)),
        value
            .and_then(|v| v["consecutive_errors"].as_u64())
            .map_or(1, |n| n.min(u32::MAX.into()) as u32),
    )
}

/// A call waiting for the rest of its records.
struct Open {
    /// The first record's sequence, so the call waiting longest is dropped at
    /// the bound.
    first: u64,
    call: Call,
}
#[derive(Default)]
struct Purpose {
    calls: u64,
    total_ms: u64,
    max_ms: u64,
    /// Up to `DURATIONS` call durations, for the median.
    durations: Vec<u32>,
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
#[derive(Default)]
struct Tool {
    requests: u64,
    failed: u64,
    timed_out: u64,
    exited: u64,
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
    /// Calls dropped from the open calls at their bound.
    unpaired: u64,
    /// Work, handoff and compaction.
    purposes: [Purpose; 3],
    errors: BTreeMap<String, Counted>,
    error_streak: u32,
    tools: BTreeMap<String, Tool>,
    other_tools: u64,
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
fn purpose_index(purpose: RequestPurpose) -> usize {
    match purpose {
        RequestPurpose::Work => 0,
        RequestPurpose::Handoff => 1,
        RequestPurpose::Compaction => 2,
    }
}
impl Totals {
    pub(crate) fn apply(&mut self, sequence: u64, update: Update) {
        if let Some(call) = update.call {
            self.call(sequence, call);
        }
        if let Some((kind, consecutive)) = update.error {
            self.errors.entry(kind).or_default().add(sequence);
            self.error_streak = self.error_streak.max(consecutive);
        }
        if let Some(name) = update.tool {
            match self.tool(name) {
                Some(tool) => tool.requests += 1,
                None => self.other_tools += 1,
            }
        }
        if let Some(fault) = update.fault {
            if let Some(tool) = self.tool(fault.tool.clone()) {
                match fault.kind {
                    FaultKind::Failed => tool.failed += 1,
                    FaultKind::TimedOut => tool.timed_out += 1,
                    FaultKind::Exited => tool.exited += 1,
                }
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
    /// A tool's totals, counting a new name while there is room for it.
    fn tool(&mut self, name: String) -> Option<&mut Tool> {
        if !self.tools.contains_key(&name) && self.tools.len() >= TOOL_NAMES {
            return None;
        }
        Some(self.tools.entry(name).or_default())
    }
    fn call(&mut self, sequence: u64, call: Call) {
        if !self.open.contains_key(&call.request_id) {
            if self.open.len() >= OPEN_CALLS
                && let Some(oldest) = self
                    .open
                    .iter()
                    .min_by_key(|(_, open)| open.first)
                    .map(|(id, _)| id.clone())
            {
                self.open.remove(&oldest);
                self.unpaired += 1;
            }
            self.open.insert(
                call.request_id.clone(),
                Open {
                    first: sequence,
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
        if call.settled_ms.is_some() {
            known.settled_ms = call.settled_ms;
            known.usage = call.usage;
        }
        if known.requested_ms.is_some() && known.settled_ms.is_some() {
            let settled = self.open.remove(&call.request_id).unwrap().call;
            self.settle(settled);
        }
    }
    /// A call with its request and settlement indexed adds to its purpose's
    /// totals.
    fn settle(&mut self, call: Call) {
        let purpose =
            &mut self.purposes[purpose_index(call.purpose.unwrap_or(RequestPurpose::Work))];
        let ms = call
            .settled_ms
            .unwrap_or(0)
            .saturating_sub(call.requested_ms.unwrap_or(0));
        purpose.calls += 1;
        purpose.total_ms += ms;
        purpose.max_ms = purpose.max_ms.max(ms);
        if purpose.durations.len() < DURATIONS {
            purpose.durations.push(ms.min(u32::MAX.into()) as u32);
        }
        match call.usage {
            Some(Usage::Known(tokens)) => {
                purpose.known_calls += 1;
                purpose.known_tokens += tokens;
                purpose.known_ms += ms;
            }
            Some(Usage::Unknown(reserved)) => {
                purpose.unknown_calls += 1;
                purpose.reserved_tokens += reserved;
                purpose.unknown_ms += ms;
            }
            None => (),
        }
    }
    /// The sections, once earlier history has loaded. `ended` says whether
    /// an open call is still in progress or never settled.
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
            "Final workspace",
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
    fn time(&self, progress: &Progress) -> Vec<Line> {
        let model_ms: u64 = self.purposes.iter().map(|p| p.total_ms).sum();
        let (Some(elapsed), true) = (progress.elapsed_ms, model_ms > 0) else {
            return vec![];
        };
        let mut lines = vec![Line::plain(format!(
            "{} elapsed · {}% in model requests",
            clock(elapsed / 1000),
            model_ms * 100 / elapsed.max(1).max(model_ms)
        ))];
        let mut parts: Vec<String> = [
            (RequestPurpose::Work, "work"),
            (RequestPurpose::Handoff, "handoff"),
            (RequestPurpose::Compaction, "compaction"),
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
            (RequestPurpose::Work, "work call", "work calls"),
            (RequestPurpose::Handoff, "handoff call", "handoff calls"),
            (
                RequestPurpose::Compaction,
                "compaction call",
                "compaction calls",
            ),
        ] {
            let p = &self.purposes[purpose_index(purpose)];
            if p.calls == 0 {
                continue;
            }
            let mut parts = vec![count(p.calls, one, many)];
            if !p.durations.is_empty() {
                let mut durations = p.durations.clone();
                let middle = durations.len() / 2;
                let median = *durations.select_nth_unstable(middle).1;
                parts.push(format!(
                    "median {}{}",
                    duration_words(median.into()),
                    if p.calls as usize > p.durations.len() {
                        format!(" of the first {}", thousands(p.durations.len() as u64))
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
        let unknown: u64 = self.purposes.iter().map(|p| p.unknown_calls).sum();
        if unknown > 0 {
            let mut text = format!(
                "{} with unknown usage · {} tokens charged",
                count(unknown, "call", "calls"),
                thousands(self.purposes.iter().map(|p| p.reserved_tokens).sum())
            );
            let unknown_ms: u64 = self.purposes.iter().map(|p| p.unknown_ms).sum();
            if unknown_ms >= 1000 {
                text.push_str(&format!(" · {} in those calls", clock(unknown_ms / 1000)));
            }
            lines.push(Line::plain(text));
        }
        let open = self.open.len() as u64 + self.unpaired;
        if open > 0 {
            let first = self.open.values().map(|open| open.first).min();
            lines.push(Line {
                text: if ended {
                    format!(
                        "{} without a recorded settlement",
                        count(open, "call", "calls")
                    )
                } else {
                    format!("{} in progress", count(open, "call", "calls"))
                },
                jump: first,
            });
        }
        lines
    }
    fn model_errors(&self) -> Vec<Line> {
        let total: u64 = self.errors.values().map(|c| c.count).sum();
        if total == 0 {
            return vec![];
        }
        let mut kinds: Vec<(&String, &Counted)> = self.errors.iter().collect();
        kinds.sort_by(|a, b| b.1.count.cmp(&a.1.count).then_with(|| a.0.cmp(b.0)));
        let mut text = format!(
            "{}: {}",
            count(total, "model error", "model errors"),
            kinds
                .iter()
                .map(|(kind, counted)| format!(
                    "{} {}",
                    thousands(counted.count),
                    kind.replace('_', " ")
                ))
                .collect::<Vec<_>>()
                .join(" · ")
        );
        if self.error_streak > 1 {
            text.push_str(&format!(" · longest streak {}", self.error_streak));
        }
        vec![Line {
            text,
            jump: kinds.iter().map(|(_, counted)| counted.first).min(),
        }]
    }
    fn tool_lines(&self) -> Vec<Line> {
        let requests: u64 = self.tools.values().map(|t| t.requests).sum::<u64>() + self.other_tools;
        if requests == 0 && self.faults.is_empty() {
            return vec![];
        }
        let mut tools: Vec<(&String, &Tool)> = self.tools.iter().collect();
        tools.sort_by(|a, b| b.1.requests.cmp(&a.1.requests).then_with(|| a.0.cmp(b.0)));
        let mut names: Vec<String> = tools
            .iter()
            .filter(|(_, tool)| tool.requests > 0)
            .map(|(name, tool)| format!("{} {name}", thousands(tool.requests)))
            .collect();
        if self.other_tools > 0 {
            names.push(format!("{} other", thousands(self.other_tools)));
        }
        let mut lines = vec![Line::plain(format!(
            "{}: {}",
            count(requests, "tool call", "tool calls"),
            names.join(" · ")
        ))];
        let (failed, exited, timed_out) = self.tools.values().fold((0, 0, 0), |(f, e, t), tool| {
            (f + tool.failed, e + tool.exited, t + tool.timed_out)
        });
        let outcomes: Vec<String> = [
            (failed, "failed"),
            (exited, "exited non-zero"),
            (timed_out, "timed out"),
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
    fn sessions(&self, progress: &Progress) -> Vec<Line> {
        let sessions = progress.session.unwrap_or(0);
        let completed = progress.completed.unwrap_or(0);
        if sessions == 0 && completed == 0 && self.handoffs_missing.count == 0 {
            return vec![];
        }
        let mut parts = vec![
            format!("{} started", count(sessions, "session", "sessions")),
            format!("{} saved", count(completed, "checkpoint", "checkpoints")),
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
