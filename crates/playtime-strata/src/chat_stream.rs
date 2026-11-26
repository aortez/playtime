use std::collections::BTreeSet;

use playtime_engine::ModelResponse;
use serde_json::{Value, json};

use crate::stream::{Frames, protocol};
use crate::{Completion, Failure, FailureKind, Limits, Result, StreamEvent};

/// The rejection llama-server streams when the model's output doesn't parse
/// as its chat format. The unparsed text appears only in the server's log.
const UNPARSED_OUTPUT: &str = "The model produced output that does not match the expected ";

/// A tool call's compact observation, by its stream index; later deltas carry
/// no call ID.
pub(crate) fn controller_chunk(chunk: &Value) -> Option<Value> {
    let delta = &chunk["choices"][0]["delta"];
    if let Some(text) = delta["reasoning_content"].as_str() {
        return Some(json!({"item_id":"reasoning", "thinking":text}));
    }
    if let Some(text) = delta["content"].as_str() {
        return Some(json!({"item_id":"message", "content":text}));
    }
    let call = &delta["tool_calls"][0];
    let arguments = call["function"]["arguments"].as_str()?;
    Some(
        json!({"item_id":format!("tool_call_{}", call["index"].as_u64()?),
        "arguments_delta":arguments}),
    )
}

struct Call {
    id: String,
    name: String,
    arguments: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Stage {
    /// Before the finish chunk: deltas and the optional role chunk.
    Deltas,
    /// After the finish chunk, which the usage chunk follows.
    Finished,
    /// After the usage chunk, which `[DONE]` follows.
    Usage,
}

/// Strict, bounded decoder for llama-server v1.0.0's Chat Completions stream.
///
/// Each chunk carries one delta. The finish chunk, the usage chunk and
/// `[DONE]` follow in that order. Calls become executable only after `[DONE]`,
/// when the finish reason is `tool_calls` and every call's arguments parse as
/// a JSON object. That finish reason alone doesn't show this: llama-server's
/// lenient parser reports it for calls whose arguments were cut off.
pub struct ChatDecoder {
    model: String,
    engine: String,
    limits: Limits,
    frames: Frames,
    chunks: u64,
    id: Option<String>,
    stage: Stage,
    content: String,
    reasoning: String,
    calls: Vec<Call>,
    call_ids: BTreeSet<String>,
    assembled: usize,
    finish_reason: Option<String>,
    usage: Option<Value>,
    timings: Option<Value>,
    terminal: bool,
    completion: Option<Result<Completion>>,
    events: Vec<StreamEvent>,
    failure: Option<Failure>,
}

fn identifier(value: &Value) -> Result<String> {
    value
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 256)
        .map(str::to_owned)
        .ok_or_else(|| protocol("invalid identifier"))
}
fn only(value: &Value, keys: &[&str]) -> bool {
    value
        .as_object()
        .is_some_and(|map| map.keys().all(|key| keys.contains(&key.as_str())))
}

impl ChatDecoder {
    /// `model` is the lock's alias and `engine` its `build_info`, which every
    /// chunk repeats.
    pub fn new(model: &str, engine: &str, limits: Limits) -> Result<Self> {
        limits.validate()?;
        Ok(Self {
            model: model.into(),
            engine: engine.into(),
            limits,
            frames: Frames::default(),
            chunks: 0,
            id: None,
            stage: Stage::Deltas,
            content: String::new(),
            reasoning: String::new(),
            calls: Vec::new(),
            call_ids: BTreeSet::new(),
            assembled: 0,
            finish_reason: None,
            usage: None,
            timings: None,
            terminal: false,
            completion: None,
            events: Vec::new(),
            failure: None,
        })
    }
    pub fn received_bytes(&self) -> usize {
        self.frames.received_bytes()
    }
    /// A finish chunk or a streamed error. Neither shows that the server
    /// stopped generating.
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
        let (frames, framed) = self.frames.push(bytes, &self.limits);
        for frame in frames {
            if self.completion.is_some() {
                return Err(protocol("event after the end of the stream"));
            }
            if frame.event.is_some() {
                return Err(protocol("Chat Completions frames carry no event name"));
            }
            self.chunks += 1;
            if self.chunks > self.limits.events {
                return Err(Failure::new(
                    FailureKind::ByteLimit,
                    "SSE event limit exceeded",
                ));
            }
            if frame.data == "[DONE]" {
                self.done()?;
                self.events.push(StreamEvent {
                    data: json!("[DONE]"),
                });
                continue;
            }
            let value: Value = serde_json::from_str(&frame.data).map_err(protocol)?;
            self.accept(&value)?;
            self.events.push(StreamEvent { data: value });
        }
        framed
    }

    fn accept(&mut self, chunk: &Value) -> Result<()> {
        if let Some(error) = chunk.get("error") {
            if !only(chunk, &["error"]) || self.stage != Stage::Deltas {
                return Err(protocol("unexpected streamed error"));
            }
            let message = error["message"].as_str().unwrap_or_default();
            let kind = if message.starts_with(UNPARSED_OUTPUT) {
                FailureKind::ToolParse
            } else {
                FailureKind::Remote
            };
            self.terminal = true;
            self.completion = Some(Err(Failure::new(kind, error)));
            return Ok(());
        }
        if !only(
            chunk,
            &[
                "choices",
                "created",
                "id",
                "model",
                "object",
                "system_fingerprint",
                "usage",
                "timings",
            ],
        ) || chunk["object"] != "chat.completion.chunk"
            || chunk["model"] != self.model
            || chunk["system_fingerprint"] != self.engine
            || !chunk["created"].is_u64()
        {
            return Err(protocol("chunk identity or fields are unexpected"));
        }
        let id = identifier(&chunk["id"])?;
        match &self.id {
            None if id.starts_with("chatcmpl-") => self.id = Some(id),
            Some(first) if *first == id => {}
            _ => return Err(protocol("completion identity changed during streaming")),
        }
        let choices = chunk["choices"]
            .as_array()
            .ok_or_else(|| protocol("missing choices"))?;
        if choices.is_empty() {
            return self.usage_chunk(chunk);
        }
        if choices.len() != 1
            || chunk.get("usage").is_some()
            || chunk.get("timings").is_some()
            || !only(&choices[0], &["delta", "finish_reason", "index"])
            || choices[0]["index"] != 0
            || self.stage != Stage::Deltas
        {
            return Err(protocol("unexpected choice"));
        }
        let delta = &choices[0]["delta"];
        match &choices[0]["finish_reason"] {
            Value::Null => self.delta(delta),
            Value::String(reason)
                if matches!(reason.as_str(), "stop" | "length" | "tool_calls")
                    && delta == &json!({}) =>
            {
                self.finish_reason = Some(reason.clone());
                self.stage = Stage::Finished;
                self.terminal = true;
                Ok(())
            }
            _ => Err(protocol("unsupported finish chunk")),
        }
    }

    fn delta(&mut self, delta: &Value) -> Result<()> {
        // llama-server opens with the role, unless its first result is final.
        if delta == &json!({"role":"assistant", "content":null}) {
            return if self.chunks == 1 {
                Ok(())
            } else {
                Err(protocol("repeated role delta"))
            };
        }
        let fields = delta
            .as_object()
            .filter(|map| map.len() == 1)
            .ok_or_else(|| protocol("a delta must carry one field"))?;
        let (field, value) = fields.iter().next().unwrap();
        let text = match field.as_str() {
            "reasoning_content" | "content" => value
                .as_str()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| protocol("empty text delta"))?,
            "tool_calls" => return self.call_delta(value),
            _ => return Err(protocol("unsupported delta field")),
        };
        self.grow(text.len())?;
        if field == "content" {
            self.content.push_str(text);
        } else {
            self.reasoning.push_str(text);
        }
        Ok(())
    }

    /// A new call opens at the next index with its ID, type and name; later
    /// deltas only extend an open call's arguments.
    fn call_delta(&mut self, value: &Value) -> Result<()> {
        let invalid = || protocol("invalid tool call delta");
        let call = match value.as_array().map(Vec::as_slice) {
            Some([call]) if only(call, &["index", "id", "type", "function"]) => call,
            _ => return Err(invalid()),
        };
        let function = match call.get("function") {
            None => &Value::Null,
            Some(f) if only(f, &["name", "arguments"]) => f,
            Some(_) => return Err(invalid()),
        };
        let arguments = match function.get("arguments") {
            None => "",
            Some(a) => a.as_str().filter(|s| !s.is_empty()).ok_or_else(invalid)?,
        };
        let index = call["index"].as_u64().ok_or_else(invalid)?;
        if index == self.calls.len() as u64 {
            if self.calls.len() == 256 || call["type"] != "function" {
                return Err(invalid());
            }
            let id = identifier(&call["id"])?;
            if !self.call_ids.insert(id.clone()) {
                return Err(protocol("duplicate call identity"));
            }
            self.calls.push(Call {
                id,
                name: identifier(&function["name"])?,
                arguments: String::new(),
            });
        } else if index >= self.calls.len() as u64
            || call.get("id").is_some()
            || call.get("type").is_some()
            || function.get("name").is_some()
            || arguments.is_empty()
        {
            return Err(invalid());
        }
        self.grow(arguments.len())?;
        self.calls[index as usize].arguments.push_str(arguments);
        Ok(())
    }

    fn grow(&mut self, bytes: usize) -> Result<()> {
        self.assembled = self.assembled.saturating_add(bytes);
        if self.assembled > self.limits.assembled_bytes {
            return Err(Failure::new(
                FailureKind::ByteLimit,
                "assembled output exceeds limit",
            ));
        }
        Ok(())
    }

    fn usage_chunk(&mut self, chunk: &Value) -> Result<()> {
        let usage = &chunk["usage"];
        if self.stage != Stage::Finished
            || !usage.is_object()
            || chunk.get("timings").is_some_and(|t| !t.is_object())
        {
            return Err(protocol("unexpected usage chunk"));
        }
        self.usage = Some(usage.clone());
        self.timings = chunk.get("timings").cloned();
        self.stage = Stage::Usage;
        Ok(())
    }

    fn done(&mut self) -> Result<()> {
        if self.stage != Stage::Usage {
            return Err(protocol("stream ended before its finish and usage chunks"));
        }
        self.completion = Some(self.complete());
        Ok(())
    }

    fn complete(&self) -> Result<Completion> {
        let usage = self.usage.as_ref().unwrap();
        let count = |value: &Value| {
            value
                .as_u64()
                .ok_or_else(|| protocol("invalid usage count"))
        };
        let generated = count(&usage["completion_tokens"])?;
        let prompt = count(&usage["prompt_tokens"])?;
        if prompt.checked_add(generated) != Some(count(&usage["total_tokens"])?) {
            return Err(protocol("inconsistent usage totals"));
        }
        let cached = usage["prompt_tokens_details"]
            .get("cached_tokens")
            .map(count)
            .transpose()?;
        if cached.is_some_and(|c| c > prompt) {
            return Err(protocol("usage details exceed totals"));
        }
        let finish_reason = self.finish_reason.clone().unwrap();
        let mut calls = Vec::new();
        match finish_reason.as_str() {
            // Calls cut off by the length limit never execute.
            "length" => {}
            "stop" if self.calls.is_empty() => {}
            "tool_calls" if !self.calls.is_empty() => {
                for call in &self.calls {
                    let mut error = Failure::new(
                        FailureKind::ToolParse,
                        "incomplete function call or non-object JSON arguments",
                    );
                    error.generated_tokens = Some(generated);
                    let args: Value =
                        serde_json::from_str(&call.arguments).map_err(|_| error.clone())?;
                    if !args.is_object() {
                        return Err(error);
                    }
                    calls.push(json!({"id":call.id, "type":"function",
                        "function":{"name":call.name, "arguments":args}}));
                }
            }
            _ => return Err(protocol("finish reason contradicts the streamed calls")),
        }
        Ok(Completion {
            response: ModelResponse {
                message: json!({"role":"assistant", "content":self.content,
                    "thinking":self.reasoning, "tool_calls":calls}),
                done: true,
                generated_tokens: Some(generated),
                prompt_tokens: Some(prompt),
            },
            finish_reason,
            response_id: self.id.clone().unwrap(),
            cached_input_tokens: cached,
            reasoning_tokens: None,
            timings: self.timings.clone(),
        })
    }

    pub fn partial(&self) -> Value {
        json!({"content":self.content, "thinking":self.reasoning,
            "tool_calls":self.calls.iter().map(|c| json!({"id":c.id, "name":c.name,
                "arguments":c.arguments})).collect::<Vec<_>>(),
            "finish_reason":self.finish_reason})
    }
    pub fn finish(mut self) -> Result<Completion> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if self.frames.inside_frame() {
            return Err(protocol("stream ended inside an SSE frame"));
        }
        self.completion.take().unwrap_or_else(|| {
            Err(Failure::new(
                FailureKind::Incomplete,
                "stream ended before [DONE]",
            ))
        })
    }
}
