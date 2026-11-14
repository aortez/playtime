use std::collections::BTreeSet;

use playtime_engine::ModelResponse;
use serde_json::{Value, json};

use crate::{Completion, Failure, FailureKind, Limits, Result};

#[derive(Clone, Debug)]
pub struct StreamEvent {
    pub data: Value,
}
impl StreamEvent {
    /// Compact controller observations; raw SSE metadata and repeated terminal
    /// snapshots stay available separately without consuming its chunk budget.
    pub fn controller_chunk(&self) -> Option<Value> {
        let field = match self.data["type"].as_str()? {
            "response.output_text.delta" => "content",
            "response.reasoning_text.delta" => "thinking",
            "response.function_call_arguments.delta" => "arguments_delta",
            _ => return None,
        };
        Some(json!({"item_id":self.data["item_id"], field:self.data["delta"]}))
    }
}

#[derive(Clone)]
struct Item {
    id: String,
    kind: String,
    name: Option<String>,
    call_id: Option<String>,
    text: String,
    status: String,
    done: bool,
}

fn protocol(detail: impl ToString) -> Failure {
    Failure::new(FailureKind::Protocol, detail)
}
fn text(value: &Value) -> Result<&str> {
    value.as_str().ok_or_else(|| protocol("expected a string"))
}
fn identifier(value: &Value) -> Result<String> {
    let s = text(value)?;
    if s.is_empty() || s.len() > 256 {
        return Err(protocol("invalid output identifier"));
    }
    Ok(s.into())
}
fn item_text(item: &Value) -> Result<String> {
    match text(&item["type"])? {
        "function_call" => Ok(text(&item["arguments"])?.into()),
        "message" | "reasoning" => {
            let kind = if item["type"] == "message" {
                "output_text"
            } else {
                "reasoning_text"
            };
            if item["type"] == "message" && item["role"] != "assistant" {
                return Err(protocol("output message is not assistant"));
            }
            let mut result = String::new();
            for part in item["content"]
                .as_array()
                .ok_or_else(|| protocol("missing output content"))?
            {
                if part["type"] != kind {
                    return Err(protocol("unsupported output content part"));
                }
                result.push_str(text(&part["text"])?);
            }
            Ok(result)
        }
        _ => Err(protocol(
            "only reasoning, text and function calls are supported",
        )),
    }
}
impl Item {
    fn new(value: &Value) -> Result<Self> {
        let kind = text(&value["type"])?;
        let content = item_text(value)?;
        if value["status"] != "in_progress" || !content.is_empty() {
            return Err(protocol("output item must start empty and in progress"));
        }
        Ok(Self {
            id: identifier(&value["id"])?,
            kind: kind.into(),
            name: (kind == "function_call")
                .then(|| identifier(&value["name"]))
                .transpose()?,
            call_id: (kind == "function_call")
                .then(|| identifier(&value["call_id"]))
                .transpose()?,
            text: content,
            status: "in_progress".into(),
            done: false,
        })
    }
    fn agrees(&self, value: &Value) -> Result<bool> {
        Ok(value["id"] == self.id
            && value["type"] == self.kind
            && (self.kind != "function_call"
                || (value["name"].as_str() == self.name.as_deref()
                    && value["call_id"].as_str() == self.call_id.as_deref()))
            && item_text(value)? == self.text)
    }
}

/// Strict, bounded decoder for the installed Strata 0.1.57/0.1.58 event profile.
/// A terminal snapshot must agree with the streamed items before any call can
/// become a normalized executable tool request.
pub struct Decoder {
    model: String,
    limits: Limits,
    line: Vec<u8>,
    data: String,
    event: Option<String>,
    frame_bytes: usize,
    received_bytes: usize,
    sequence: u64,
    response_id: Option<String>,
    items: Vec<Item>,
    ids: BTreeSet<String>,
    call_ids: BTreeSet<String>,
    assembled: usize,
    terminal: bool,
    completion: Option<Result<Completion>>,
    events: Vec<StreamEvent>,
    failure: Option<Failure>,
}
impl Decoder {
    pub fn new(model: &str, limits: Limits) -> Result<Self> {
        limits.validate()?;
        Ok(Self {
            model: model.into(),
            limits,
            line: Vec::new(),
            data: String::new(),
            event: None,
            frame_bytes: 0,
            received_bytes: 0,
            sequence: 0,
            response_id: None,
            items: Vec::new(),
            ids: BTreeSet::new(),
            call_ids: BTreeSet::new(),
            assembled: 0,
            terminal: false,
            completion: None,
            events: Vec::new(),
            failure: None,
        })
    }
    pub fn received_bytes(&self) -> usize {
        self.received_bytes
    }
    pub fn terminal_observed(&self) -> bool {
        self.terminal
    }
    pub fn is_finished(&self) -> bool {
        self.completion.is_some()
    }

    /// Drain events even when push fails: valid frames preceding a malformed
    /// frame in the same network chunk remain observable.
    pub fn take_events(&mut self) -> Vec<StreamEvent> {
        std::mem::take(&mut self.events)
    }

    pub fn push(&mut self, bytes: &[u8]) -> Result<()> {
        if let Some(error) = &self.failure {
            return Err(error.clone());
        }
        let result = self.feed(bytes);
        if let Err(error) = &result {
            self.failure = Some(error.clone());
        }
        result
    }

    fn feed(&mut self, bytes: &[u8]) -> Result<()> {
        self.received_bytes = self.received_bytes.saturating_add(bytes.len());
        if self.received_bytes > self.limits.response_bytes {
            return Err(Failure::new(
                FailureKind::ByteLimit,
                "SSE response exceeds byte limit",
            ));
        }
        for byte in bytes {
            self.frame_bytes += 1;
            if self.frame_bytes > self.limits.event_bytes {
                return Err(Failure::new(
                    FailureKind::ByteLimit,
                    "SSE frame exceeds byte limit",
                ));
            }
            if *byte != b'\n' {
                self.line.push(*byte);
                continue;
            }
            let line = std::mem::take(&mut self.line);
            let line = std::str::from_utf8(&line)
                .map_err(protocol)?
                .trim_end_matches('\r');
            if line.is_empty() {
                self.frame_bytes = 0;
                if !self.data.is_empty() {
                    let data = std::mem::take(&mut self.data);
                    let event = self.event.take();
                    let value: Value = serde_json::from_str(&data).map_err(protocol)?;
                    if event.as_deref().is_some_and(|e| value["type"] != e) {
                        return Err(protocol("SSE event and JSON type disagree"));
                    }
                    self.accept(&value)?;
                    self.events.push(StreamEvent { data: value });
                }
                self.event = None;
            } else if !line.starts_with(':') {
                let (field, value) = line.split_once(':').unwrap_or((line, ""));
                let value = value.strip_prefix(' ').unwrap_or(value);
                match field {
                    "data" => {
                        if !self.data.is_empty() {
                            self.data.push('\n');
                        }
                        self.data.push_str(value);
                    }
                    "event" => self.event = Some(value.into()),
                    "id" | "retry" => {}
                    _ => return Err(protocol("unsupported SSE field")),
                }
            }
        }
        Ok(())
    }

    fn response(&self, value: &Value) -> Result<()> {
        if value["model"] != self.model
            || value["id"].as_str() != self.response_id.as_deref()
            || value["object"] != "response"
        {
            return Err(protocol("response identity changed during streaming"));
        }
        Ok(())
    }
    fn index(&self, event: &Value) -> Result<usize> {
        let index = event["output_index"]
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .ok_or_else(|| protocol("invalid output index"))?;
        if index >= self.items.len()
            || event["item_id"] != self.items[index].id
            || self.items[index].done
        {
            return Err(protocol("event references a missing or finished item"));
        }
        Ok(index)
    }

    fn accept(&mut self, event: &Value) -> Result<()> {
        if self.completion.is_some() {
            return Err(protocol("event after terminal response"));
        }
        if self.sequence >= self.limits.events {
            return Err(Failure::new(
                FailureKind::ByteLimit,
                "SSE event limit exceeded",
            ));
        }
        if event["sequence_number"].as_u64() != Some(self.sequence) {
            return Err(protocol("missing, repeated or reordered SSE sequence"));
        }
        self.sequence += 1;
        let kind = text(&event["type"])?;
        if kind == "response.created" {
            if self.sequence != 1 || self.response_id.is_some() {
                return Err(protocol("duplicate response creation"));
            }
            self.response_id = Some(identifier(&event["response"]["id"])?);
            self.response(&event["response"])?;
            if event["response"]["status"] != "in_progress" {
                return Err(protocol("response did not start in progress"));
            }
            return Ok(());
        }
        if self.response_id.is_none() {
            return Err(protocol("stream omitted response.created"));
        }
        match kind {
            "response.in_progress" => {
                self.response(&event["response"])?;
                if event["response"]["status"] != "in_progress" {
                    return Err(protocol("invalid progress status"));
                }
            }
            "response.output_item.added" => {
                if event["output_index"].as_u64() != Some(self.items.len() as u64)
                    || self.items.len() == 256
                {
                    return Err(protocol("output item order or count is invalid"));
                }
                let item = Item::new(&event["item"])?;
                if !self.ids.insert(item.id.clone())
                    || item
                        .call_id
                        .as_ref()
                        .is_some_and(|id| !self.call_ids.insert(id.clone()))
                {
                    return Err(protocol("duplicate item or call identity"));
                }
                self.items.push(item);
            }
            "response.output_text.delta"
            | "response.reasoning_text.delta"
            | "response.function_call_arguments.delta" => {
                let index = self.index(event)?;
                let expected = match kind {
                    "response.output_text.delta" => "message",
                    "response.reasoning_text.delta" => "reasoning",
                    _ => "function_call",
                };
                if self.items[index].kind != expected
                    || (expected != "function_call" && event["content_index"] != 0)
                {
                    return Err(protocol("delta type does not match its output item"));
                }
                let delta = text(&event["delta"])?;
                self.assembled = self.assembled.saturating_add(delta.len());
                if self.assembled > self.limits.assembled_bytes {
                    return Err(Failure::new(
                        FailureKind::ByteLimit,
                        "assembled output exceeds limit",
                    ));
                }
                self.items[index].text.push_str(delta);
            }
            "response.output_item.done" => {
                let index = event["output_index"]
                    .as_u64()
                    .and_then(|n| usize::try_from(n).ok())
                    .ok_or_else(|| protocol("invalid done index"))?;
                let item = self
                    .items
                    .get_mut(index)
                    .ok_or_else(|| protocol("unknown done item"))?;
                let status = text(&event["item"]["status"])?;
                if item.done
                    || !matches!(status, "completed" | "incomplete")
                    || !item.agrees(&event["item"])?
                {
                    return Err(protocol("completed item contradicts its streamed data"));
                }
                item.status = status.into();
                item.done = true;
            }
            "response.output_text.done"
            | "response.reasoning_text.done"
            | "response.function_call_arguments.done" => {
                let item = &self.items[self.index(event)?];
                let (expected, field) = match kind {
                    "response.output_text.done" => ("message", "text"),
                    "response.reasoning_text.done" => ("reasoning", "text"),
                    _ => ("function_call", "arguments"),
                };
                if item.kind != expected
                    || event[field] != item.text
                    || (expected != "function_call" && event["content_index"] != 0)
                {
                    return Err(protocol("done text contradicts deltas"));
                }
            }
            "response.content_part.added" | "response.content_part.done" => {
                let item = &self.items[self.index(event)?];
                let expected = if kind.ends_with("added") {
                    ""
                } else {
                    &item.text
                };
                if item.kind != "message"
                    || event["content_index"] != 0
                    || event["part"]["type"] != "output_text"
                    || event["part"]["text"] != expected
                {
                    return Err(protocol("invalid content part"));
                }
            }
            "response.completed" | "response.incomplete" | "response.failed" => {
                self.response(&event["response"])?;
                self.terminal = true;
                self.completion = Some(self.finish_response(event));
            }
            _ => return Err(protocol(format!("unsupported Responses event {kind}"))),
        }
        Ok(())
    }

    fn finish_response(&self, event: &Value) -> Result<Completion> {
        let response = &event["response"];
        let state = text(&response["status"])?;
        if event["type"] != format!("response.{state}") {
            return Err(protocol("terminal status disagrees with event"));
        }
        if state == "failed" {
            return Err(Failure::new(
                FailureKind::Remote,
                response["error"].to_string(),
            ));
        }
        let usage = &response["usage"];
        let count = |value: Option<&Value>| -> Result<Option<u64>> {
            match value {
                None | Some(Value::Null) => Ok(None),
                Some(v) => v
                    .as_u64()
                    .map(Some)
                    .ok_or_else(|| protocol("invalid usage count")),
            }
        };
        if !usage.is_null() && !usage.is_object() {
            return Err(protocol("invalid usage object"));
        }
        let generated = count(usage.get("output_tokens"))?;
        let prompt = count(usage.get("input_tokens"))?;
        let total = count(usage.get("total_tokens"))?;
        if let (Some(i), Some(o), Some(t)) = (prompt, generated, total)
            && i.checked_add(o) != Some(t)
        {
            return Err(protocol("inconsistent usage totals"));
        }
        let cached = count(usage["input_tokens_details"].get("cached_tokens"))?;
        let reasoning = count(usage["output_tokens_details"].get("reasoning_tokens"))?;
        if cached.zip(prompt).is_some_and(|(c, p)| c > p)
            || reasoning.zip(generated).is_some_and(|(r, g)| r > g)
        {
            return Err(protocol("usage details exceed totals"));
        }
        let output = response["output"]
            .as_array()
            .ok_or_else(|| protocol("missing terminal output"))?;
        if output.len() != self.items.len() {
            return Err(protocol("terminal output count changed"));
        }
        for (item, value) in self.items.iter().zip(output) {
            if !item.done || !item.agrees(value)? || value["status"] != item.status {
                return Err(protocol("terminal output contradicts streamed items"));
            }
            if state == "completed" && item.status != "completed" && item.kind != "function_call" {
                return Err(protocol("completed response contains incomplete output"));
            }
        }
        let mut calls = Vec::new();
        let mut content = String::new();
        let mut thinking = String::new();
        for item in &self.items {
            match item.kind.as_str() {
                "message" => content.push_str(&item.text),
                "reasoning" => thinking.push_str(&item.text),
                "function_call" if state == "completed" => {
                    let mut error = Failure::new(
                        FailureKind::ToolParse,
                        "incomplete function call or non-object JSON arguments",
                    );
                    error.generated_tokens = generated;
                    if item.status != "completed" {
                        return Err(error);
                    }
                    let args: Value =
                        serde_json::from_str(&item.text).map_err(|_| error.clone())?;
                    if !args.is_object() {
                        return Err(error);
                    }
                    calls.push(json!({"id":item.call_id, "type":"function", "function":{"name":item.name, "arguments":args}}));
                }
                _ => {}
            }
        }
        if state == "incomplete" && response["incomplete_details"]["reason"] != "max_output_tokens"
        {
            return Err(Failure::new(
                FailureKind::Incomplete,
                "unsupported incomplete-response reason",
            ));
        }
        let finish_reason = if state == "incomplete" {
            "length"
        } else if calls.is_empty() {
            "stop"
        } else {
            "tool_calls"
        };
        Ok(Completion {
            response: ModelResponse {
                message: json!({"role":"assistant", "content":content, "thinking":thinking, "tool_calls":calls}),
                done: true,
                generated_tokens: generated,
                prompt_tokens: prompt,
            },
            finish_reason: finish_reason.into(),
            response_id: self.response_id.clone().unwrap(),
            cached_input_tokens: cached,
            reasoning_tokens: reasoning,
            timings: event.get("timings").cloned(),
        })
    }

    pub fn partial(&self) -> Value {
        json!({"items":self.items.iter().map(|i| json!({"id":i.id, "type":i.kind,
            "name":i.name, "call_id":i.call_id, "text":i.text, "status":i.status})).collect::<Vec<_>>()})
    }
    pub fn finish(mut self) -> Result<Completion> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if !self.line.is_empty() || !self.data.is_empty() {
            return Err(protocol("stream ended inside an SSE frame"));
        }
        self.completion.take().unwrap_or_else(|| {
            Err(Failure::new(
                FailureKind::Incomplete,
                "stream ended without a terminal response",
            ))
        })
    }
}
