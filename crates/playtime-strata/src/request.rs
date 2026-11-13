use std::collections::{BTreeSet, VecDeque};

use playtime_core::RequestPurpose;
use playtime_engine::ModelRequest;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{Failure, FailureKind, Limits, ModelLock, Result};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Thinking {
    None,
    Low,
    Medium,
    High,
    Xhigh,
}

/// Every sampler field is sent explicitly; mutable server defaults cannot
/// silently change a conversation's effective inference settings.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub thinking: Thinking,
    pub summary_thinking: Thinking,
    pub temperature: f64,
    pub top_p: f64,
    pub top_k: u32,
    pub min_p: f64,
    pub repetition_penalty: f64,
    pub presence_penalty: f64,
    pub frequency_penalty: f64,
    pub penalty_last_n: u32,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            thinking: Thinking::Medium,
            summary_thinking: Thinking::None,
            temperature: 0.6,
            top_p: 0.95,
            top_k: 20,
            min_p: 0.0,
            repetition_penalty: 1.0,
            presence_penalty: 0.0,
            frequency_penalty: 0.0,
            penalty_last_n: 64,
        }
    }
}
impl Settings {
    pub fn validate(&self) -> Result<()> {
        if !self.temperature.is_finite()
            || !(0.0..=2.0).contains(&self.temperature)
            || !self.top_p.is_finite()
            || !(0.0..=1.0).contains(&self.top_p)
            || self.top_p == 0.0
            || !self.min_p.is_finite()
            || !(0.0..=1.0).contains(&self.min_p)
            || !self.repetition_penalty.is_finite()
            || !(0.1..=2.0).contains(&self.repetition_penalty)
            || !self.presence_penalty.is_finite()
            || !(-2.0..=2.0).contains(&self.presence_penalty)
            || !self.frequency_penalty.is_finite()
            || !(-2.0..=2.0).contains(&self.frequency_penalty)
            || self.top_k > 1_000_000
            || self.penalty_last_n > 1_000_000
        {
            return Err(Failure::new(
                FailureKind::Request,
                "unsupported sampler settings",
            ));
        }
        Ok(())
    }
}

pub fn encode_request(
    request: &ModelRequest,
    lock: &ModelLock,
    limits: &Limits,
) -> Result<Vec<u8>> {
    lock.validate()?;
    limits.validate()?;
    let invalid = || {
        Failure::new(
            FailureKind::Request,
            "invalid or incomplete normalized conversation",
        )
    };
    if request.maximum_tokens == 0
        || request.maximum_tokens > i32::MAX as u64
        || request
            .context_input_bound
            .saturating_add(request.maximum_tokens)
            .saturating_add(request.context_reserve)
            > lock.identity.context_tokens
    {
        return Err(Failure::new(
            FailureKind::Request,
            "request exceeds the locked context/output allowance",
        ));
    }
    let mut input = Vec::new();
    let mut pending = VecDeque::new();
    let mut used_ids = BTreeSet::new();
    for (index, message) in request.messages.iter().enumerate() {
        let role = message["role"].as_str().ok_or_else(invalid)?;
        if role != "tool" && !pending.is_empty() {
            return Err(invalid());
        }
        match role {
            "system" | "user" | "developer" => {
                let content = message["content"].as_str().ok_or_else(invalid)?;
                input.push(json!({"type":"message", "role":role, "content":content}));
            }
            "assistant" => {
                if let Some(text) = message.get("thinking") {
                    let text = text.as_str().ok_or_else(invalid)?;
                    if !text.is_empty() {
                        input.push(json!({"type":"reasoning", "content":[{"type":"reasoning_text", "text":text}]}));
                    }
                }
                let content = message
                    .get("content")
                    .map(Value::as_str)
                    .unwrap_or(Some(""))
                    .ok_or_else(invalid)?;
                input.push(json!({"type":"message", "role":"assistant", "content":content}));
                if let Some(calls) = message.get("tool_calls") {
                    for (ordinal, call) in calls.as_array().ok_or_else(invalid)?.iter().enumerate()
                    {
                        let name = call["function"]["name"]
                            .as_str()
                            .filter(|s| !s.is_empty())
                            .ok_or_else(invalid)?;
                        let args = call["function"]["arguments"]
                            .as_object()
                            .ok_or_else(invalid)?;
                        let id = match call.get("id") {
                            Some(id) => id
                                .as_str()
                                .filter(|s| !s.is_empty() && s.len() <= 256)
                                .ok_or_else(invalid)?
                                .to_owned(),
                            None => format!("playtime_{index}_{ordinal}"),
                        };
                        if !used_ids.insert(id.clone()) {
                            return Err(invalid());
                        }
                        input.push(json!({"type":"function_call", "call_id":id, "name":name,
                            "arguments":serde_json::to_string(args).map_err(|_| invalid())?}));
                        pending.push_back((id, name.to_owned()));
                    }
                }
            }
            "tool" => {
                let (id, name) = pending.pop_front().ok_or_else(invalid)?;
                if message["tool_name"] != name {
                    return Err(invalid());
                }
                let output = message["content"].as_str().ok_or_else(invalid)?;
                input.push(json!({"type":"function_call_output", "call_id":id, "output":output}));
            }
            _ => return Err(invalid()),
        }
    }
    if !pending.is_empty() || input.is_empty() {
        return Err(invalid());
    }
    let mut tools = Vec::new();
    let mut names = BTreeSet::new();
    for tool in &request.tools {
        if tool["type"] != "function" {
            return Err(Failure::new(
                FailureKind::Request,
                "only function tools are supported",
            ));
        }
        let function = tool["function"].as_object().ok_or_else(invalid)?;
        let name = function
            .get("name")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(invalid)?;
        if !names.insert(name) || !function.get("parameters").is_some_and(Value::is_object) {
            return Err(invalid());
        }
        let mut flat = function.clone();
        flat.insert("type".into(), json!("function"));
        tools.push(Value::Object(flat));
    }
    let settings = &lock.settings;
    let thinking = if request.purpose == RequestPurpose::Compaction {
        settings.summary_thinking
    } else {
        settings.thinking
    };
    let body = json!({"model":lock.identity.model, "input":input, "tools":tools,
        "stream":true, "store":false, "truncation":"disabled", "tool_choice":"auto",
        "max_output_tokens":request.maximum_tokens, "reasoning":{"effort":thinking},
        "temperature":settings.temperature, "top_p":settings.top_p, "top_k":settings.top_k,
        "min_p":settings.min_p, "repetition_penalty":settings.repetition_penalty,
        "presence_penalty":settings.presence_penalty, "frequency_penalty":settings.frequency_penalty,
        "penalty_last_n":settings.penalty_last_n, "seed":request.sampling_seed});
    let bytes = serde_json::to_vec(&body).map_err(|e| Failure::new(FailureKind::Request, e))?;
    if bytes.len() > limits.request_bytes {
        return Err(Failure::new(
            FailureKind::ByteLimit,
            "encoded request exceeds byte limit",
        ));
    }
    Ok(bytes)
}
