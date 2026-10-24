//! Bounded transcript index and payload working set. Used only by background work.
#[path = "trace_presentation.rs"]
mod presentation;
use crate::progress::{self, Progress, Tracker, Update};
use playtime_protocol::{
    api::EventCursor,
    events::{CleanupOutcome, EventEnvelope, EventPayload, KnownEvent, MemoryRef, decode_event},
    observations::{KnownObservation, ObservationPayload},
};
use presentation::Display;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::Arc,
};

pub const PAYLOAD_BYTES: usize = 16 << 20;
pub const INDEX_BYTES: usize = 32 << 20;
pub const TEXT_BYTES: usize = 64 << 10;
pub const WINDOW_ROWS: usize = 64;

#[derive(Clone, Debug, Default)]
pub struct Metrics {
    pub inbox_bytes_high_water: usize,
    pub inbox_records_high_water: usize,
    pub records: usize,
    pub rows: usize,
    pub payload_bytes: usize,
    pub payload_high_water: usize,
    pub index_bytes: usize,
    pub index_high_water: usize,
}
#[derive(Clone, Debug)]
pub struct Section {
    pub label: String,
    pub text: String,
}
/// Which records the timeline lists. Compact and Readable share `Readable`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum View {
    /// Activity once, with one-line markers instead of archive bookkeeping.
    #[default]
    Readable,
    /// Every record as a card.
    Full,
}
/// A boundary whose exact published content is prepared separately from events.
#[derive(Clone, Debug)]
pub enum TraceContent {
    Incoming(playtime_protocol::archive::ContentRef),
    Checkpoint(playtime_core::SessionId),
}
#[derive(Clone, Debug)]
pub struct SessionLink {
    pub session: String,
    pub ordinal: u32,
    pub row: usize,
    pub sequence: u64,
    observed: bool,
}
#[derive(Clone, Debug, Default)]
pub struct Sessions {
    pub start: usize,
    pub total: usize,
    pub items: Vec<SessionLink>,
    pub complete: bool,
}
#[derive(Clone, Debug, Default)]
pub struct Row {
    pub id: usize,
    pub first_sequence: u64,
    pub title: String,
    pub excerpt: String,
    pub status: String,
    pub sources: String,
    pub sections: Vec<Section>,
    pub needs_loading: bool,
    /// A one-line marker in place of an archive record's card.
    pub marker: bool,
    /// The session whose checkpoint the marker announces.
    pub checkpoint: Option<String>,
    /// A tool block belonging to the preceding model call; virtualized independently.
    pub nested: bool,
    pub parent: Option<String>,
    pub session: Option<SessionLink>,
    pub trace: Option<TraceContent>,
}
#[derive(Clone, Debug, Default)]
pub struct Window {
    pub start: usize,
    pub total: usize,
    pub rows: Vec<Row>,
    pub metrics: Metrics,
    /// A requested row and its index in this view, to keep it in place.
    pub anchor: Option<(usize, usize)>,
    /// The run's progress, for the header.
    pub progress: Progress,
    pub sessions: Sessions,
    pub location: Option<(u64, usize, usize)>,
}
#[derive(Clone, Debug)]
pub struct SourcePage {
    pub text: String,
    pub start: usize,
    pub total: usize,
    pub retained: Option<playtime_protocol::archive::ContentRef>,
}
#[derive(Clone)]
struct Cached {
    raw: Arc<str>,
    parts: Vec<Section>,
    readable: Vec<Section>,
    bytes: usize,
}
struct Record {
    digest: [u8; 32],
    group: usize,
    page: Option<usize>,
}
struct Group {
    key: String,
    first: u64,
    title: String,
    excerpt: String,
    status: String,
    sources: Vec<u64>,
    final_sequence: Option<u64>,
    role: Role,
    display: Display,
    trace: Option<TraceContent>,
    session_id: Option<String>,
}
/// How the readable view shows a group. Full shows every group as a card.
#[derive(Clone, Debug, Default, PartialEq)]
enum Role {
    #[default]
    Activity,
    /// An archive record that repeats what the observations already show.
    Bookkeeping,
    /// A session start, a checkpoint or the run's end, as one line.
    Marker {
        text: String,
        checkpoint: Option<String>,
    },
}
struct Family {
    first: u64,
    members: Vec<usize>,
}
#[derive(Default)]
pub struct Transcript {
    run_id: Option<playtime_core::RunId>,
    records: BTreeMap<u64, Record>,
    groups: Vec<Group>,
    keys: BTreeMap<String, usize>,
    order: Vec<usize>,
    readable: Vec<usize>,
    families: BTreeMap<String, Family>,
    ended: bool,
    sessions: BTreeMap<u64, SessionLink>,
    session_keys: BTreeMap<String, u64>,
    pages: Vec<String>,
    cache: BTreeMap<u64, Cached>,
    lru: VecDeque<u64>,
    progress: Tracker,
    pub metrics: Metrics,
    pub boundary: bool,
}

pub fn excerpt(text: &str, bytes: usize) -> String {
    let end = floor(text, bytes.min(text.len()));
    let mut result = text[..end].to_owned();
    if end < text.len() {
        result.push_str(" …");
    }
    result
}
fn floor(text: &str, mut index: usize) -> usize {
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}
pub fn source_page(text: &str, offset: usize) -> SourcePage {
    let start = floor(text, offset.min(text.len()));
    let end = floor(text, start.saturating_add(TEXT_BYTES).min(text.len()));
    SourcePage {
        text: text[start..end].into(),
        start,
        total: text.len(),
        retained: None,
    }
}
fn json(value: &impl serde::Serialize) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| "Unavailable source".into())
}
/// Formats a count with thousands separators, such as 300,000.
pub fn thousands(value: u64) -> String {
    let digits = value.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}
/// Formats a byte count, such as 512 bytes or 1.5 KiB.
pub fn size(bytes: u64) -> String {
    match bytes {
        0..1024 => format!("{bytes} bytes"),
        1024..1_048_576 => format!("{:.1} KiB", bytes as f64 / 1024.0),
        _ => format!("{:.1} MiB", bytes as f64 / 1_048_576.0),
    }
}
/// A wire value such as `session_limit` as words.
pub(crate) fn words(value: &impl serde::Serialize) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(|text| text.replace('_', " ")))
        .unwrap_or_else(|| "unavailable".into())
}
/// Session starts, checkpoints and the run's end become markers; the other
/// archive records repeat the observations.
fn role(event: &KnownEvent) -> Role {
    let (text, checkpoint) = match event {
        KnownEvent::SessionStarted {
            session_id,
            ordinal,
            memento,
            ..
        } => (
            format!(
                "{} · Session {ordinal} started {}",
                session_id.as_str(),
                match memento {
                    MemoryRef::Available { content } =>
                        format!("with a memento ({})", size(content.size_bytes)),
                    MemoryRef::Missing => "without a memento".into(),
                    MemoryRef::Unavailable { reason } =>
                        format!("without its memento: {}", excerpt(reason, 120)),
                }
            ),
            None,
        ),
        KnownEvent::CheckpointPublished { checkpoint } => (
            format!(
                "{} · Checkpoint saved {}",
                checkpoint.session_id.as_str(),
                match &checkpoint.memento {
                    MemoryRef::Available { .. } => "with a memento",
                    MemoryRef::Missing => "without a memento",
                    MemoryRef::Unavailable { .. } => "with its memento unavailable",
                }
            ),
            Some(checkpoint.session_id.as_str().to_owned()),
        ),
        KnownEvent::RunFinished {
            reason,
            usage,
            cleanup,
            ..
        } => (
            format!(
                "Run finished: {} · cleanup {} · {} tokens charged",
                words(reason),
                match cleanup {
                    CleanupOutcome::Complete => "complete".into(),
                    CleanupOutcome::Failed { errors } => format!(
                        "failed ({})",
                        progress::count(errors.len() as u64, "error", "errors")
                    ),
                    CleanupOutcome::Unknown => "unknown".into(),
                },
                thousands(usage.budget_charged_tokens)
            ),
            None,
        ),
        _ => return Role::Bookkeeping,
    };
    Role::Marker { text, checkpoint }
}
struct Projection {
    key: String,
    title: String,
    excerpt: String,
    status: String,
    parts: Vec<Section>,
    final_response: bool,
    role: Role,
    progress: Update,
    display: Display,
    readable: Vec<Section>,
    finished: bool,
    session: Option<(String, u32)>,
    observed_session: bool,
    trace: Option<TraceContent>,
    session_id: Option<String>,
}
fn project(event: &EventEnvelope) -> Result<Projection, String> {
    let decoded = decode_event(&serde_json::to_string(event).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    let mut p = Projection {
        key: format!("event:{}", event.sequence),
        title: event.kind.clone(),
        excerpt: String::new(),
        status: "Recorded".into(),
        parts: vec![],
        final_response: false,
        role: Role::Activity,
        progress: Update::default(),
        display: Display::default(),
        readable: vec![],
        finished: false,
        session: None,
        observed_session: false,
        trace: None,
        session_id: None,
    };
    if let EventPayload::Observation(observation) = decoded.payload() {
        let o = observation.observation();
        p.session_id = o.session_id.as_ref().map(|id| id.as_str().to_owned());
        p.progress = Update::observation(observation);
        (p.display, p.readable) = presentation::observation(observation);
        let session = o
            .session_id
            .as_ref()
            .map_or("session unavailable", |s| s.as_str());
        p.title = format!("{session} · {}", o.observation_kind);
        if let Some(tool) = o.identity.tool_request_sequence {
            p.key = format!("tool:{session}:{tool}");
            p.title = format!("{session} · Tool {tool}");
        } else if let Some(request) = &o.identity.request_id {
            p.key = format!("model:{session}:{request}");
            p.title = format!("{session} · Model {request}");
        }
        match observation.payload() {
            ObservationPayload::Known(known) => match known.as_ref() {
                KnownObservation::ControllerFinished { .. } => {
                    p.finished = true;
                    p.parts.push(Section {
                        label: "Controller finished".into(),
                        text: json(&observation.record().map(|r| &r.data)),
                    });
                }
                KnownObservation::ControllerStarted { mode, .. } => {
                    p.excerpt = format!(
                        "Controller started in {} mode. Frozen parameters are retained in source.",
                        progress::mode_words(*mode)
                    );
                }
                KnownObservation::PhaseChanged { phase, .. } => {
                    p.excerpt = format!("Controller phase: {}", progress::phase_words(*phase));
                }
                KnownObservation::MemoryReceived {
                    memory, ordinal, ..
                } => {
                    p.title = format!("Session {ordinal}");
                    p.observed_session = true;
                    p.session = o
                        .session_id
                        .as_ref()
                        .map(|id| (id.as_str().to_owned(), ordinal.get()));
                    p.status = format!("Memory received · {}", memory.status);
                    p.parts.push(Section {
                        label: "Exact delivered memory".into(),
                        text: memory.content.clone(),
                    });
                }
                KnownObservation::ModelRequest(request) => {
                    p.status = if request.handoff {
                        "Handoff request recorded"
                    } else {
                        "Model request recorded"
                    }
                    .into();
                    p.excerpt = format!(
                        "Call {} · maximum {} tokens · {} messages",
                        request.call,
                        request.maximum_tokens,
                        request.messages.len()
                    );
                    p.parts.push(Section {
                        label: "Recorded model request".into(),
                        text: json(request),
                    });
                }
                KnownObservation::ModelChunk { chunk, .. } => {
                    p.status =
                        "Streamed content · incomplete until a final response is recorded".into();
                    if let Some(message) = chunk.get("message") {
                        for (field, label) in [
                            ("thinking", "Recorded thinking"),
                            ("content", "Model response"),
                        ] {
                            if let Some(text) = message.get(field).and_then(|v| v.as_str()) {
                                p.parts.push(Section {
                                    label: label.into(),
                                    text: text.into(),
                                });
                            }
                        }
                    }
                }
                KnownObservation::ModelResponse { response, .. } => {
                    p.final_response = true;
                    p.status = if response.done {
                        "Final response recorded"
                    } else {
                        "Final recorded response · incomplete"
                    }
                    .into();
                    for (field, label) in [
                        ("thinking", "Recorded thinking"),
                        ("content", "Model response"),
                    ] {
                        if let Some(text) = response.message.get(field).and_then(|v| v.as_str()) {
                            p.parts.push(Section {
                                label: label.into(),
                                text: text.into(),
                            });
                        }
                    }
                    if let Some(calls) = response.message.get("tool_calls") {
                        p.parts.push(Section {
                            label: "Tool calls".into(),
                            text: json(calls),
                        });
                    }
                }
                KnownObservation::ToolRequest { name, arguments } => {
                    p.parts.push(Section {
                        label: name.clone(),
                        text: json(arguments),
                    });
                    p.status = format!("{name} · tool requested");
                }
                KnownObservation::ToolIntent { .. }
                | KnownObservation::MementoWriteIntent { .. } => {
                    p.status = "Dispatch intent recorded".into();
                }
                KnownObservation::ToolNotDispatched { .. } => {
                    p.status = "Not dispatched".into();
                }
                KnownObservation::ToolResult {
                    name,
                    result,
                    unabridged_result,
                    ..
                } => {
                    p.parts.push(Section {
                        label: format!("{name} result"),
                        text: json(result),
                    });
                    if let Some(raw) = unabridged_result {
                        p.parts.push(Section {
                            label: "Unabridged recorded result".into(),
                            text: json(raw),
                        });
                    }
                    let result_status = if result
                        .get("error")
                        .is_some_and(|v| !v.is_null() && v != false)
                    {
                        "Tool error recorded"
                    } else {
                        "Tool result recorded"
                    };
                    p.status = format!("{name} · {result_status}");
                }
                KnownObservation::ModelError { error, .. } => {
                    p.status = format!("Model error · {}", error.kind);
                    p.parts.push(Section {
                        label: "Error".into(),
                        text: error.message.clone(),
                    });
                }
                _ => p.parts.push(Section {
                    label: o.observation_kind.clone(),
                    text: json(&observation.record().map(|r| &r.data)),
                }),
            },
            ObservationPayload::Referenced { content } => {
                p.status = "Retained payload · load verified source".into();
                p.parts.push(Section {
                    label: "Retained content reference".into(),
                    text: json(content),
                });
            }
            ObservationPayload::Unknown { kind, data } => {
                p.status = "Unknown observation · source retained".into();
                p.parts.push(Section {
                    label: kind.clone(),
                    text: json(data),
                });
            }
        }
    } else {
        p.parts.push(Section {
            label: "Source data".into(),
            text: json(&event.data),
        });
        match decoded.payload() {
            EventPayload::Known(known) => {
                p.finished = matches!(known.as_ref(), KnownEvent::RunFinished { .. });
                p.role = role(known);
                p.progress = Update::event(known);
                match known.as_ref() {
                    KnownEvent::SessionStarted {
                        session_id,
                        ordinal,
                        memento,
                        delivery,
                        ..
                    } => {
                        p.session = Some((session_id.as_str().to_owned(), ordinal.get()));
                        // Only the explicit delivery describes what this session
                        // actually received. An older workspace memento is not a substitute.
                        p.trace = delivery
                            .as_ref()
                            .map(|d| TraceContent::Incoming(d.memento.content.clone()));
                        p.readable.push(Section {
                            label: "Memory received".into(),
                            text: match (memento, delivery) {
                                (MemoryRef::Missing, _) => "No memento supplied.".into(),
                                (MemoryRef::Unavailable { reason }, _) => format!("Unavailable: {}", excerpt(reason, 512)),
                                (_, Some(d)) if d.memento.status != "loaded" => format!("Delivery status: {}", excerpt(&d.memento.status, 120)),
                                (_, Some(_)) => "Loading exact delivered memory…".into(),
                                _ => "Exact delivery unavailable in this record; see recorded memory observations if present.".into(),
                            },
                        });
                        if !matches!(memento, MemoryRef::Available { .. }) {
                            p.trace = None;
                        }
                    }
                    KnownEvent::CheckpointPublished { checkpoint } => {
                        p.trace = Some(TraceContent::Checkpoint(checkpoint.session_id.clone()));
                    }
                    _ => (),
                }
            }
            EventPayload::Control(_) if event.kind == "effect_intent" => {
                p.role = Role::Bookkeeping;
            }
            EventPayload::Unknown { .. } => p.status = "Unknown event · source retained".into(),
            _ => (),
        }
    }
    if let Some(section) = p
        .parts
        .iter()
        .find(|part| part.label == "Model response")
        .or(p.parts.first())
        && p.excerpt.is_empty()
    {
        p.excerpt = excerpt(
            &section.text.lines().take(3).collect::<Vec<_>>().join("\n"),
            240,
        );
    }
    if p.role == Role::Activity
        && p.display.family.is_none()
        && p.readable.is_empty()
        && !p.parts.is_empty()
    {
        p.readable.push(Section {
            label: "Recorded data".into(),
            text: "Inspect source for recorded details.".into(),
        });
    }
    Ok(p)
}
impl Transcript {
    pub fn page(&mut self, cursor: &str) -> Result<usize, String> {
        EventCursor::decode(cursor).map_err(|e| e.to_string())?;
        if let Some(id) = self.pages.iter().position(|p| p == cursor) {
            return Ok(id);
        }
        self.reserve(cursor.len() + 64)?;
        self.pages.push(cursor.into());
        Ok(self.pages.len() - 1)
    }
    fn reserve(&mut self, bytes: usize) -> Result<(), String> {
        if self.metrics.index_bytes.saturating_add(bytes) > INDEX_BYTES {
            self.boundary = true;
            return Err("History index limit reached; continue in a new history segment".into());
        }
        self.metrics.index_bytes += bytes;
        self.metrics.index_high_water = self.metrics.index_high_water.max(self.metrics.index_bytes);
        Ok(())
    }
    /// Returns false for an identical redelivery; contradictory content fails.
    /// A record without a history page arrived live.
    pub fn apply(&mut self, event: EventEnvelope, page: Option<usize>) -> Result<bool, String> {
        if self.run_id.as_ref().is_some_and(|run| *run != event.run_id) {
            return Err("Transcript run identity mismatch".into());
        }
        self.run_id = Some(event.run_id.clone());
        let raw: Arc<str> = serde_json::to_string(&event)
            .map_err(|e| e.to_string())?
            .into();
        let digest: [u8; 32] = Sha256::digest(raw.as_bytes()).into();
        let seq = event.sequence.get();
        if let Some(old) = self.records.get(&seq) {
            if old.digest != digest {
                return Err("Conflicting duplicate event".into());
            }
            let p = project(&event)?;
            self.put(seq, raw, p.parts, p.readable);
            if let Some(page) = page {
                self.records.get_mut(&seq).unwrap().page = Some(page);
            }
            return Ok(false);
        }
        let mut p = project(&event)?;
        let update = std::mem::take(&mut p.progress);
        let existing = self.keys.get(&p.key).copied();
        if let Some((session, ordinal)) = &p.session
            && let Some(sequence) = self.session_keys.get(session)
        {
            let old = &self.sessions[sequence];
            if old.ordinal != *ordinal {
                return Err("Conflicting recorded session ordinal".into());
            }
            if old.observed && !p.observed_session {
                p.role = Role::Bookkeeping;
            }
        }
        if existing.is_some_and(|id| self.groups[id].display.family != p.display.family) {
            return Err("Conflicting originating model request for activity".into());
        }
        let marker = match &p.role {
            Role::Marker { text, .. } => text.len() + 64,
            Role::Activity | Role::Bookkeeping => 0,
        };
        // Conservative per-node overhead includes B-tree slots, source and order
        // indexes. Strings are counted separately; full source lives in the LRU.
        let cost = 192
            + update.tracked_bytes()
            + p.session.as_ref().map_or(0, |(id, _)| 512 + id.len() * 4)
            + match &p.trace {
                Some(TraceContent::Incoming(content)) => {
                    256 + content.path.as_str().len() + content.sha256.as_str().len()
                }
                Some(TraceContent::Checkpoint(id)) => 128 + id.as_str().len(),
                None => 0,
            }
            + if existing.is_none() {
                384 + p.key.len() * 2
                    + p.title.len()
                    + 256
                    + 176
                    + marker
                    + p.session_id.as_ref().map_or(0, |id| 32 + id.len() * 2)
            } else {
                0
            };
        self.reserve(
            cost + if existing.is_none() {
                p.display.tracked_bytes()
            } else {
                0
            },
        )?;
        let mut family_moved = false;
        if let Some(key) = &p.display.family
            && let Some(family) = self.families.get_mut(key)
            && seq < family.first
        {
            family.first = seq;
            family_moved = true;
        }
        let id = existing.unwrap_or_else(|| {
            let id = self.groups.len();
            self.keys.insert(p.key.clone(), id);
            self.groups.push(Group {
                key: p.key.clone(),
                first: seq,
                title: p.title.clone(),
                excerpt: String::new(),
                status: String::new(),
                sources: vec![],
                final_sequence: None,
                role: p.role.clone(),
                display: p.display.clone(),
                trace: p.trace.clone(),
                session_id: p.session_id.clone(),
            });
            if let Some(family) = &p.display.family {
                self.families
                    .entry(family.clone())
                    .or_insert_with(|| Family {
                        first: seq,
                        members: vec![],
                    })
                    .members
                    .push(id);
            }
            self.place(id);
            id
        });
        if let Some((session, ordinal)) = p.session {
            let old = self.session_keys.get(&session).copied();
            if old.is_none_or(|old| {
                (!self.sessions[&old].observed && p.observed_session)
                    || (self.sessions[&old].observed == p.observed_session && seq < old)
            }) {
                if let Some(old) = old {
                    let old = self.sessions.remove(&old).unwrap();
                    if !old.observed && p.observed_session {
                        self.groups[old.row].role = Role::Bookkeeping;
                        self.readable.retain(|id| *id != old.row);
                    }
                }
                self.session_keys.insert(session.clone(), seq);
                self.sessions.insert(
                    seq,
                    SessionLink {
                        session,
                        ordinal,
                        row: id,
                        sequence: seq,
                        observed: p.observed_session,
                    },
                );
            }
        }
        if p.finished {
            self.ended = true;
        }
        let group = &mut self.groups[id];
        group.display.merge(&p.display, seq);
        let slot = group.sources.partition_point(|old| *old < seq);
        group.sources.insert(slot, seq);
        if p.final_response {
            group.final_sequence = Some(group.final_sequence.map_or(seq, |old| old.max(seq)));
        }
        if (seq == *group.sources.last().unwrap() && group.final_sequence.is_none())
            || group.final_sequence == Some(seq)
        {
            if !p.excerpt.is_empty() {
                group.excerpt = p.excerpt;
            }
            group.status = excerpt(&p.status, 150);
        }
        if seq < group.first {
            group.first = seq;
            self.order.retain(|old| *old != id);
            self.readable.retain(|old| *old != id);
            self.place(id);
        }
        if family_moved {
            self.place_family(id);
        }
        self.records.insert(
            seq,
            Record {
                digest,
                group: id,
                page,
            },
        );
        self.put(seq, raw, p.parts, p.readable);
        self.progress.apply(seq, update, page.is_none());
        self.metrics.records = self.records.len();
        self.metrics.rows = self.groups.len();
        Ok(true)
    }
    /// Inserts a group by its first record into every order that lists it.
    fn place(&mut self, id: usize) {
        let first = self.groups[id].first;
        let slot = self
            .order
            .partition_point(|old| self.groups[*old].first < first);
        self.order.insert(slot, id);
        if self.groups[id].role != Role::Bookkeeping {
            let slot = self
                .readable
                .partition_point(|old| self.readable_key(*old) < self.readable_key(id));
            self.readable.insert(slot, id);
        }
    }
    fn readable_key(&self, id: usize) -> (u64, bool, u64) {
        let group = &self.groups[id];
        let first = group
            .display
            .family
            .as_ref()
            .and_then(|key| self.families.get(key))
            .map(|family| family.first)
            .unwrap_or(group.first);
        (first, group.display.tool, group.first)
    }
    fn place_family(&mut self, id: usize) {
        let Some(ids) = self.groups[id]
            .display
            .family
            .as_ref()
            .and_then(|key| self.families.get(key))
            .map(|family| family.members.clone())
        else {
            return;
        };
        // Reposition only this call's independently virtualized blocks. This also
        // handles tools arriving before their model request/history.
        self.readable.retain(|old| ids.binary_search(old).is_err());
        for id in ids {
            let key = self.readable_key(id);
            let slot = self
                .readable
                .partition_point(|old| self.readable_key(*old) < key);
            self.readable.insert(slot, id);
        }
    }
    /// `complete` means earlier history has loaded.
    pub fn progress(&self, complete: bool) -> Progress {
        self.progress.progress(complete)
    }
    fn order(&self, view: View) -> &[usize] {
        match view {
            View::Readable => &self.readable,
            View::Full => &self.order,
        }
    }
    /// The index of a row in `view`, or of the next row listed there when the
    /// view leaves it out.
    pub fn position(&self, view: View, id: usize) -> Option<usize> {
        let first = self.groups.get(id)?.first;
        let order = self.order(view);
        let index = order.iter().position(|old| *old == id).unwrap_or_else(|| {
            order
                .iter()
                .position(|old| self.groups[*old].first >= first)
                .unwrap_or(order.len())
        });
        (!order.is_empty()).then(|| index.min(order.len() - 1))
    }
    fn put(&mut self, seq: u64, raw: Arc<str>, parts: Vec<Section>, readable: Vec<Section>) {
        if let Some(old) = self.cache.remove(&seq) {
            self.metrics.payload_bytes -= old.bytes;
            self.lru.retain(|old| *old != seq);
        }
        let bytes = raw.len()
            + parts
                .iter()
                .map(|p| p.label.capacity() + p.text.capacity() + 64)
                .sum::<usize>()
            + readable
                .iter()
                .map(|p| p.label.capacity() + p.text.capacity() + 64)
                .sum::<usize>()
            + 128;
        if bytes > PAYLOAD_BYTES {
            return;
        }
        while self.metrics.payload_bytes + bytes > PAYLOAD_BYTES {
            if let Some(old) = self.lru.pop_front().and_then(|seq| self.cache.remove(&seq)) {
                self.metrics.payload_bytes -= old.bytes;
            }
        }
        self.metrics.payload_bytes += bytes;
        self.metrics.payload_high_water = self
            .metrics
            .payload_high_water
            .max(self.metrics.payload_bytes);
        self.cache.insert(
            seq,
            Cached {
                raw,
                parts,
                readable,
                bytes,
            },
        );
        self.lru.push_back(seq);
    }
    pub fn window(&self, view: View, start: usize, count: usize) -> Window {
        let order = self.order(view);
        let start = start.min(order.len());
        let mut budget = TEXT_BYTES;
        let rows = order
            .iter()
            .skip(start)
            .take(count.min(WINDOW_ROWS))
            .map(|id| {
                let g = &self.groups[*id];
                let marker = match (&g.role, view) {
                    (Role::Marker { text, checkpoint }, View::Readable) => Some((text, checkpoint)),
                    _ => None,
                };
                let mut sections: Vec<Section> = vec![];
                let mut missing = false;
                for seq in &g.sources {
                    let Some(cached) = self.cache.get(seq) else {
                        missing = true;
                        continue;
                    };
                    let parts = if view == View::Readable {
                        &cached.readable
                    } else {
                        &cached.parts
                    };
                    for section in parts {
                        let model_text = matches!(
                            section.label.as_str(),
                            "Model response" | "Recorded thinking"
                        );
                        if model_text && g.final_sequence.is_some_and(|final_seq| final_seq != *seq)
                        {
                            continue;
                        }
                        if budget < 32 {
                            break;
                        }
                        let text = excerpt(&section.text, budget.min(4096).saturating_sub(8));
                        budget = budget.saturating_sub(text.len() + section.label.len());
                        if model_text
                            && g.final_sequence.is_none()
                            && let Some(old) =
                                sections.iter_mut().find(|s| s.label == section.label)
                        {
                            let remaining = 4096usize.saturating_sub(old.text.len());
                            if remaining > 8 {
                                old.text.push_str(&excerpt(&text, remaining - 8));
                            } else if remaining >= 4 && !old.text.ends_with(" …") {
                                old.text.push_str(" …");
                            }
                        } else {
                            sections.push(Section {
                                label: section.label.clone(),
                                text,
                            });
                        }
                    }
                }
                let (title, excerpt, status) = match marker {
                    Some((text, _)) => (text.clone(), String::new(), String::new()),
                    None => (g.title.clone(), g.excerpt.clone(), g.status.clone()),
                };
                let parent = g
                    .display
                    .family
                    .as_ref()
                    .and_then(|key| self.families.get(key))
                    .and_then(|ids| {
                        ids.members.iter().find_map(|id| {
                            let display = &self.groups[*id].display;
                            (!display.tool).then(|| display.heading()).flatten()
                        })
                    });
                let (title, status) = if view == View::Readable {
                    (
                        g.display.heading().unwrap_or(title),
                        g.display.status(self.ended).unwrap_or(status),
                    )
                } else {
                    (title, status)
                };
                Row {
                    id: *id,
                    first_sequence: g.first,
                    title,
                    excerpt: if view == View::Readable && g.display.family.is_some() {
                        String::new()
                    } else {
                        excerpt
                    },
                    status,
                    sources: format!(
                        "{} source records · first {} · last {}",
                        g.sources.len(),
                        g.first,
                        g.sources.last().unwrap()
                    ),
                    sections,
                    needs_loading: missing,
                    marker: marker.is_some(),
                    checkpoint: marker.and_then(|(_, checkpoint)| checkpoint.clone()),
                    nested: view == View::Readable && g.display.tool,
                    session: if let Some(id) = &g.session_id {
                        self.session_keys
                            .get(id)
                            .and_then(|seq| self.sessions.get(seq))
                            .cloned()
                    } else {
                        self.sessions
                            .range(..=g.first)
                            .next_back()
                            .map(|(_, session)| session.clone())
                    },
                    trace: g.trace.clone(),
                    parent: (view == View::Readable && g.display.tool)
                        .then(|| parent.unwrap_or_else(|| "Call details unavailable".into())),
                }
            })
            .collect();
        Window {
            start,
            total: order.len(),
            rows,
            metrics: self.metrics.clone(),
            anchor: None,
            progress: Progress::default(),
            sessions: self.sessions(0, false),
            location: None,
        }
    }
    pub fn sessions(&self, start: usize, complete: bool) -> Sessions {
        let start = start.min(self.sessions.len().saturating_sub(1)) / WINDOW_ROWS * WINDOW_ROWS;
        Sessions {
            start,
            total: self.sessions.len(),
            items: self
                .sessions
                .values()
                .skip(start)
                .take(WINDOW_ROWS)
                .cloned()
                .collect(),
            complete,
        }
    }
    pub fn sources(&self, id: usize) -> Vec<u64> {
        self.groups
            .get(id)
            .map(|g| g.sources.clone())
            .unwrap_or_default()
    }
    pub fn raw(&self, seq: u64, offset: usize) -> Option<SourcePage> {
        self.cache.get(&seq).map(|c| {
            let mut page = source_page(&c.raw, offset);
            if let Ok(decoded) = decode_event(&c.raw)
                && let EventPayload::Observation(observation) = decoded.payload()
                && let ObservationPayload::Referenced { content } = observation.payload()
            {
                page.retained = Some(content.clone());
            }
            page
        })
    }
    pub fn cursor_for(&self, seq: u64) -> Option<&str> {
        self.records
            .get(&seq)?
            .page
            .and_then(|id| self.pages.get(id))
            .map(String::as_str)
    }
    pub fn contains(&self, seq: u64) -> bool {
        self.cache.contains_key(&seq)
    }
    pub fn row_for(&self, seq: u64) -> Option<usize> {
        self.records.get(&seq).map(|r| r.group)
    }
    pub fn row_key(&self, id: usize) -> Option<&str> {
        self.groups.get(id).map(|g| g.key.as_str())
    }
}
