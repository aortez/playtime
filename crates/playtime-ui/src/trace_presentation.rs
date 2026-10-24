//! Readable projections of recorded observations. Never interprets model text as
//! proof of execution, publication, or artifact correctness.
use super::{Section, excerpt, size};
use playtime_protocol::observations::{DecodedObservation, KnownObservation, ObservationPayload};
use serde_json::Value;

#[derive(Clone, Default)]
pub(super) struct Display {
    pub family: Option<String>,
    pub tool: bool,
    call: Option<u64>,
    handoff: bool,
    title: Option<String>,
    state: Option<String>,
    state_sequence: u64,
    pending: bool,
}
impl Display {
    pub fn tracked_bytes(&self) -> usize {
        // Family map/key, member slot and bounded title/status metadata.
        self.family.as_ref().map_or(0, |s| s.len() * 3) + if self.tool { 768 } else { 256 }
    }
    pub fn merge(&mut self, other: &Self, sequence: u64) {
        if let Some(call) = other.call {
            self.call = Some(call);
        }
        self.handoff |= other.handoff;
        if other.title.is_some() {
            self.title = other.title.clone();
        }
        if other.state.is_some() && sequence >= self.state_sequence {
            self.state = other.state.clone();
            self.state_sequence = sequence;
            self.pending = other.pending;
        }
    }
    pub fn heading(&self) -> Option<String> {
        if self.tool {
            return Some(
                self.title
                    .clone()
                    .unwrap_or_else(|| "Tool · request details unavailable".into()),
            );
        }
        self.call
            .map(|call| {
                format!(
                    "Call {call}{}",
                    if self.handoff {
                        " · Handoff request"
                    } else {
                        ""
                    }
                )
            })
            .or_else(|| {
                self.family
                    .as_ref()
                    .map(|_| "Model call · details unavailable".into())
            })
    }
    pub fn status(&self, ended: bool) -> Option<String> {
        if self.tool && self.pending {
            return Some(
                if ended {
                    "No recorded result"
                } else {
                    "Awaiting recorded result"
                }
                .into(),
            );
        }
        self.state.clone()
    }
}
fn push(parts: &mut Vec<Section>, label: &str, text: impl Into<String>) {
    let text = text.into();
    if !text.is_empty() {
        parts.push(Section {
            label: label.into(),
            text,
        });
    }
}
fn field(parts: &mut Vec<Section>, value: &Value, key: &str, label: &str) {
    if let Some(text) = value.get(key).and_then(Value::as_str) {
        push(parts, label, text);
    }
}
fn required(parts: &mut Vec<Section>, value: &Value, key: &str, label: &str) {
    if let Some(text) = value.get(key).and_then(Value::as_str) {
        push(parts, label, text);
    } else {
        push(
            parts,
            label,
            "Unavailable in recorded data · inspect source",
        );
    }
}
fn result_state(name: &str, result: &Value) -> String {
    if let Some(error) = result.get("error").filter(|v| !v.is_null() && **v != false) {
        return format!(
            "Failed: {}",
            excerpt(
                error
                    .as_str()
                    .unwrap_or("recorded tool error; inspect source"),
                180
            )
        );
    }
    if result.get("timed_out").and_then(Value::as_bool) == Some(true) {
        return "Timed out".into();
    }
    if name == "exec" {
        return match result.get("exit_code").and_then(Value::as_i64) {
            Some(0) => "Exit status: 0".into(),
            Some(code) => format!("Failed · exit status: {code}"),
            None => "Exit status unavailable · inspect recorded result".into(),
        };
    }
    if let Some(bytes) = result.get("bytes_written").and_then(Value::as_u64) {
        return format!("Write recorded · {}", size(bytes));
    }
    if name == "read_file" && result.get("content").is_some_and(Value::is_string) {
        return "Read result recorded".into();
    }
    "Result recorded · inspect source for details".into()
}
pub(super) fn observation(o: &DecodedObservation) -> (Display, Vec<Section>) {
    let record = o.observation();
    let mut d = Display::default();
    let mut parts = vec![];
    if let (Some(session), Some(request)) = (&record.session_id, &record.identity.request_id) {
        d.family = Some(format!("{session}:{request}"));
        d.tool = record.identity.tool_request_sequence.is_some();
    }
    let ObservationPayload::Known(known) = o.payload() else {
        d.state = Some(
            match o.payload() {
                ObservationPayload::Referenced { .. } => {
                    "Retained content · inspect verified source"
                }
                _ => "Unknown observation · source retained",
            }
            .into(),
        );
        push(
            &mut parts,
            "Recorded data",
            "Inspect source to read this observation.",
        );
        return (d, parts);
    };
    match known.as_ref() {
        KnownObservation::MemoryReceived { memory, .. } => {
            let text = match memory.status.as_str() {
                "loaded" if memory.content.is_empty() => "Available empty memento",
                "loaded" => &memory.content,
                "missing" => "No memento supplied.",
                _ if memory.content.is_empty() => {
                    "Delivered content unavailable; inspect source for the recorded status."
                }
                _ => &memory.content,
            };
            push(&mut parts, "Memory received", text);
        }
        KnownObservation::ControllerCheckpoint { .. } => {
            push(
                &mut parts,
                "Checkpoint observation",
                "Controller checkpoint recorded; publication is shown separately.",
            );
        }
        KnownObservation::ControllerStop { reason, .. } => {
            push(&mut parts, "Controller stop", super::words(reason));
        }
        KnownObservation::ControllerFinished { reason, .. } => {
            push(&mut parts, "Controller finished", super::words(reason));
        }
        KnownObservation::ModelRequest(r) => {
            d.call = Some(r.call.get());
            d.handoff = r.handoff;
            d.state = Some("Awaiting model response".into());
        }
        KnownObservation::ModelChunk { call, chunk } => {
            d.call = Some(call.get());
            d.state = Some("Streaming · incomplete".into());
            if let Some(message) = chunk.get("message") {
                field(&mut parts, message, "thinking", "Recorded thinking");
                field(&mut parts, message, "content", "Model response");
            }
        }
        KnownObservation::ModelResponse { call, response } => {
            d.call = Some(call.get());
            d.state = Some(
                if response.done {
                    "Response recorded"
                } else {
                    "Partial response recorded"
                }
                .into(),
            );
            field(
                &mut parts,
                &response.message,
                "thinking",
                "Recorded thinking",
            );
            field(&mut parts, &response.message, "content", "Model response");
        }
        KnownObservation::ModelError { error, .. } => {
            d.state = Some("Model error · partial inference may be present".into());
            push(&mut parts, "Error", &error.message);
        }
        KnownObservation::RequestNotDispatched { call, .. } => {
            d.call = Some(call.get());
            d.state = Some("Not dispatched".into());
        }
        KnownObservation::ToolRequest { name, arguments } => {
            let path = arguments
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or("path unavailable");
            d.title = Some(excerpt(
                &match name.as_str() {
                    "exec" => "Run command".into(),
                    "read_file" => format!("Read · {path}"),
                    "write_file" => format!("Write · {path}"),
                    "end_session" => "End session requested".into(),
                    _ => format!("Tool · {name}"),
                },
                240,
            ));
            d.state = Some("Awaiting recorded result".into());
            d.pending = true;
            match name.as_str() {
                "exec" => {
                    required(&mut parts, arguments, "command", "Command");
                    field(&mut parts, arguments, "cwd", "Working directory");
                }
                "read_file" => {
                    if let Some(offset) = arguments.get("offset_bytes").and_then(Value::as_u64) {
                        push(&mut parts, "Requested byte offset", offset.to_string());
                    }
                }
                "write_file" => {
                    required(
                        &mut parts,
                        arguments,
                        "content",
                        "Content requested for writing",
                    );
                    if arguments.get("append").and_then(Value::as_bool) == Some(true) {
                        push(&mut parts, "Write mode", "Append requested");
                    }
                }
                "end_session" => {
                    required(&mut parts, arguments, "memento", "Submitted memento");
                    push(
                        &mut parts,
                        "Finish run requested",
                        match arguments.get("finish_run").and_then(Value::as_bool) {
                            Some(true) => "Yes",
                            Some(false) => "No",
                            None => "Not recorded",
                        },
                    );
                }
                _ => push(
                    &mut parts,
                    "Arguments",
                    "Inspect source for recorded arguments.",
                ),
            }
        }
        KnownObservation::ToolNotDispatched { .. } => {
            d.state = Some("Not dispatched".into());
        }
        KnownObservation::ToolIntent { .. } | KnownObservation::MementoWriteIntent { .. } => {
            d.state = Some("Awaiting recorded result".into());
            d.pending = true;
        }
        KnownObservation::ToolResult { name, result, .. } => {
            d.state = Some(result_state(name, result));
            match name.as_str() {
                "exec" if result.get("error").is_none_or(Value::is_null) => {
                    required(&mut parts, result, "stdout", "Standard output");
                    required(&mut parts, result, "stderr", "Standard error");
                }
                "read_file" => field(&mut parts, result, "content", "Content read"),
                _ => (),
            }
            if ["truncated", "output_truncated"]
                .iter()
                .any(|key| result.get(key).and_then(Value::as_bool) == Some(true))
            {
                push(&mut parts, "Output limit", "Recorded output was truncated");
            }
        }
        // These facts stay in source; they must not overwrite the model result
        // or add JSON bookkeeping to the readable call.
        KnownObservation::RequestSettled { .. }
        | KnownObservation::ModelUsageOverflow { .. }
        | KnownObservation::ModelChunkRejected { .. } => (),
        _ => (),
    }
    (d, parts)
}
