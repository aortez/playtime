//! Bounded transcript index and payload working set. Used only by background work.
use playtime_protocol::{
    api::EventCursor,
    events::{EventEnvelope, EventPayload, decode_event},
    observations::{KnownObservation, ObservationPayload},
};
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
#[derive(Clone, Debug)]
pub struct Row {
    pub id: usize,
    pub first_sequence: u64,
    pub title: String,
    pub excerpt: String,
    pub status: String,
    pub sources: String,
    pub sections: Vec<Section>,
    pub needs_loading: bool,
}
#[derive(Clone, Debug, Default)]
pub struct Window {
    pub start: usize,
    pub total: usize,
    pub rows: Vec<Row>,
    pub metrics: Metrics,
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
}
#[derive(Default)]
pub struct Transcript {
    run_id: Option<playtime_core::RunId>,
    records: BTreeMap<u64, Record>,
    groups: Vec<Group>,
    keys: BTreeMap<String, usize>,
    order: Vec<usize>,
    pages: Vec<String>,
    cache: BTreeMap<u64, Cached>,
    lru: VecDeque<u64>,
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
struct Projection {
    key: String,
    title: String,
    excerpt: String,
    status: String,
    parts: Vec<Section>,
    final_response: bool,
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
    };
    if let EventPayload::Observation(observation) = decoded.payload() {
        let o = observation.observation();
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
                KnownObservation::ControllerStarted { mode, .. } => {
                    p.excerpt = format!(
                        "Controller started in {mode:?} mode. Frozen parameters are retained in source."
                    );
                }
                KnownObservation::PhaseChanged { phase, .. } => {
                    p.excerpt = format!("Controller phase: {phase:?}");
                }
                KnownObservation::MemoryReceived {
                    memory, ordinal, ..
                } => {
                    p.status = format!("Session {ordinal} incoming memory: {}", memory.status);
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
        if matches!(decoded.payload(), EventPayload::Unknown { .. }) {
            p.status = "Unknown event · source retained".into();
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
            self.put(seq, raw, p.parts);
            if let Some(page) = page {
                self.records.get_mut(&seq).unwrap().page = Some(page);
            }
            return Ok(false);
        }
        let p = project(&event)?;
        let existing = self.keys.get(&p.key).copied();
        // Conservative per-node overhead includes B-tree slots, source and order
        // indexes. Strings are counted separately; full source lives in the LRU.
        let cost = 192
            + if existing.is_none() {
                384 + p.key.len() * 2 + p.title.len() + 256 + 160
            } else {
                0
            };
        self.reserve(cost)?;
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
            });
            let slot = self
                .order
                .partition_point(|id| self.groups[*id].first < seq);
            self.order.insert(slot, id);
            id
        });
        let group = &mut self.groups[id];
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
            let slot = self
                .order
                .partition_point(|id| self.groups[*id].first < seq);
            self.order.insert(slot, id);
        }
        self.records.insert(
            seq,
            Record {
                digest,
                group: id,
                page,
            },
        );
        self.put(seq, raw, p.parts);
        self.metrics.records = self.records.len();
        self.metrics.rows = self.groups.len();
        Ok(true)
    }
    fn put(&mut self, seq: u64, raw: Arc<str>, parts: Vec<Section>) {
        if let Some(old) = self.cache.remove(&seq) {
            self.metrics.payload_bytes -= old.bytes;
            self.lru.retain(|old| *old != seq);
        }
        let bytes = raw.len()
            + parts
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
        self.cache.insert(seq, Cached { raw, parts, bytes });
        self.lru.push_back(seq);
    }
    pub fn window(&self, start: usize, count: usize) -> Window {
        let start = start.min(self.order.len());
        let mut budget = TEXT_BYTES;
        let rows = self
            .order
            .iter()
            .skip(start)
            .take(count.min(WINDOW_ROWS))
            .map(|id| {
                let g = &self.groups[*id];
                let mut sections: Vec<Section> = vec![];
                let mut missing = false;
                for seq in &g.sources {
                    let Some(cached) = self.cache.get(seq) else {
                        missing = true;
                        continue;
                    };
                    for section in &cached.parts {
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
                            old.text.push_str(&text);
                        } else {
                            sections.push(Section {
                                label: section.label.clone(),
                                text,
                            });
                        }
                    }
                }
                Row {
                    id: *id,
                    first_sequence: g.first,
                    title: g.title.clone(),
                    excerpt: g.excerpt.clone(),
                    status: g.status.clone(),
                    sources: format!(
                        "{} source records · first {} · last {}",
                        g.sources.len(),
                        g.first,
                        g.sources.last().unwrap()
                    ),
                    sections,
                    needs_loading: missing,
                }
            })
            .collect();
        Window {
            start,
            total: self.order.len(),
            rows,
            metrics: self.metrics.clone(),
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
