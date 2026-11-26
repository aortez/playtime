use std::collections::{BTreeSet, VecDeque};

use playtime_core::RequestPurpose;
use playtime_engine::ModelRequest;
use serde::ser::SerializeMap;
use serde::{Deserialize, Serialize, Serializer};
use serde_json::{Value, json};

use crate::{Failure, FailureKind, Limits, ModelLock, Result, Service};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Thinking {
    None,
    /// Thinking without an effort level, for llama-server templates, which
    /// only switch it on or off.
    On,
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
    /// Strata and Basalt take an effort level or none.
    pub(crate) fn responses_thinking(&self) -> bool {
        self.thinking != Thinking::On && self.summary_thinking != Thinking::On
    }
    /// llama-server's templates only switch thinking on or off.
    pub(crate) fn llama_thinking(&self) -> bool {
        [self.thinking, self.summary_thinking]
            .iter()
            .all(|t| matches!(t, Thinking::None | Thinking::On))
    }
}

/// One validated message of a normalized conversation.
enum Turn<'a> {
    Text {
        role: &'a str,
        content: &'a str,
    },
    Assistant {
        thinking: &'a str,
        content: &'a str,
        calls: Vec<Call<'a>>,
    },
    Result {
        call_id: String,
        output: &'a str,
    },
}
struct Call<'a> {
    id: String,
    name: &'a str,
    arguments: String,
}

fn invalid() -> Failure {
    Failure::new(
        FailureKind::Request,
        "invalid or incomplete normalized conversation",
    )
}

/// Each tool result must answer the earliest unanswered call, by name, before
/// any other message.
fn conversation(request: &ModelRequest) -> Result<Vec<Turn<'_>>> {
    let mut turns = Vec::new();
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
                turns.push(Turn::Text { role, content });
            }
            "assistant" => {
                let thinking = match message.get("thinking") {
                    Some(text) => text.as_str().ok_or_else(invalid)?,
                    None => "",
                };
                let content = message
                    .get("content")
                    .map(Value::as_str)
                    .unwrap_or(Some(""))
                    .ok_or_else(invalid)?;
                let mut calls = Vec::new();
                if let Some(listed) = message.get("tool_calls") {
                    for (ordinal, call) in listed.as_array().ok_or_else(invalid)?.iter().enumerate()
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
                        pending.push_back((id.clone(), name));
                        calls.push(Call {
                            id,
                            name,
                            arguments: serde_json::to_string(args).map_err(|_| invalid())?,
                        });
                    }
                }
                turns.push(Turn::Assistant {
                    thinking,
                    content,
                    calls,
                });
            }
            "tool" => {
                let (call_id, name) = pending.pop_front().ok_or_else(invalid)?;
                if message["tool_name"] != name {
                    return Err(invalid());
                }
                let output = message["content"].as_str().ok_or_else(invalid)?;
                turns.push(Turn::Result { call_id, output });
            }
            _ => return Err(invalid()),
        }
    }
    if !pending.is_empty() || turns.is_empty() {
        return Err(invalid());
    }
    Ok(turns)
}

/// Uniquely named function tools with object parameters.
fn functions(request: &ModelRequest) -> Result<Vec<&serde_json::Map<String, Value>>> {
    let mut functions = Vec::new();
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
        functions.push(function);
    }
    Ok(functions)
}

pub fn encode_request(
    request: &ModelRequest,
    lock: &ModelLock,
    limits: &Limits,
) -> Result<Vec<u8>> {
    lock.validate()?;
    limits.validate()?;
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
    let turns = conversation(request)?;
    let functions = functions(request)?;
    let settings = &lock.settings;
    let thinking = if request.purpose == RequestPurpose::Compaction {
        settings.summary_thinking
    } else {
        settings.thinking
    };
    let bytes = if lock.identity.service == Service::LlamaServer {
        serde_json::to_vec(&ChatBody(chat_body(
            request, lock, &turns, &functions, thinking,
        )?))
    } else {
        serde_json::to_vec(&responses_body(request, lock, &turns, &functions, thinking))
    }
    .map_err(|e| Failure::new(FailureKind::Request, e))?;
    if bytes.len() > limits.request_bytes {
        return Err(Failure::new(
            FailureKind::ByteLimit,
            "encoded request exceeds byte limit",
        ));
    }
    Ok(bytes)
}

fn responses_body(
    request: &ModelRequest,
    lock: &ModelLock,
    turns: &[Turn<'_>],
    functions: &[&serde_json::Map<String, Value>],
    thinking: Thinking,
) -> Value {
    let mut input = Vec::new();
    for turn in turns {
        match turn {
            Turn::Text { role, content } => {
                input.push(json!({"type":"message", "role":role, "content":content}));
            }
            Turn::Assistant {
                thinking,
                content,
                calls,
            } => {
                if !thinking.is_empty() {
                    input.push(json!({"type":"reasoning", "content":[{"type":"reasoning_text", "text":thinking}]}));
                }
                input.push(json!({"type":"message", "role":"assistant", "content":content}));
                for call in calls {
                    input.push(
                        json!({"type":"function_call", "call_id":call.id, "name":call.name,
                        "arguments":call.arguments}),
                    );
                }
            }
            Turn::Result { call_id, output } => {
                input.push(
                    json!({"type":"function_call_output", "call_id":call_id, "output":output}),
                );
            }
        }
    }
    let tools: Vec<Value> = functions
        .iter()
        .map(|function| {
            let mut flat = (*function).clone();
            flat.insert("type".into(), json!("function"));
            Value::Object(flat)
        })
        .collect();
    let settings = &lock.settings;
    let mut body = json!({"model":lock.identity.model, "input":input, "tools":tools,
        "stream":true, "store":false, "truncation":"disabled", "tool_choice":"auto",
        "max_output_tokens":request.maximum_tokens, "reasoning":{"effort":thinking},
        "temperature":settings.temperature, "top_p":settings.top_p, "top_k":settings.top_k,
        "min_p":settings.min_p, "repetition_penalty":settings.repetition_penalty,
        "presence_penalty":settings.presence_penalty, "frequency_penalty":settings.frequency_penalty,
        "penalty_last_n":settings.penalty_last_n, "seed":request.sampling_seed});
    if lock.identity.service == Service::Basalt {
        body["relaxed_drafting"] = json!(false);
    }
    body
}

/// llama-server's Chat Completions, with its own sampler names. Thinking
/// arrives as `reasoning_content`, and usage in the stream's last chunk.
fn chat_body(
    request: &ModelRequest,
    lock: &ModelLock,
    turns: &[Turn<'_>],
    functions: &[&serde_json::Map<String, Value>],
    thinking: Thinking,
) -> Result<Value> {
    // A trailing assistant message would be continued as a prefill, and the
    // maximum seed asks llama-server for a random one.
    if matches!(turns.last(), Some(Turn::Assistant { .. })) || request.sampling_seed == u32::MAX {
        return Err(invalid());
    }
    // llama-server silently drops tools that its chat template can't render.
    if !functions.is_empty() && lock.identity.template_capabilities["supports_tool_calls"] != true {
        return Err(Failure::new(
            FailureKind::Request,
            "the locked chat template does not support tool calls",
        ));
    }
    let mut messages = Vec::new();
    for turn in turns {
        messages.push(match turn {
            Turn::Text { role, content } => {
                if *role == "developer" {
                    return Err(invalid());
                }
                json!({"role":role, "content":content})
            }
            Turn::Assistant {
                thinking,
                content,
                calls,
            } => {
                let mut message = json!({"role":"assistant", "content":content});
                if !thinking.is_empty() {
                    message["reasoning_content"] = json!(thinking);
                }
                if !calls.is_empty() {
                    message["tool_calls"] = calls
                        .iter()
                        .map(|call| {
                            json!({"id":call.id, "type":"function",
                            "function":{"name":call.name, "arguments":call.arguments}})
                        })
                        .collect();
                }
                message
            }
            Turn::Result { call_id, output } => {
                json!({"role":"tool", "tool_call_id":call_id, "content":output})
            }
        });
    }
    let tools: Vec<Value> = functions
        .iter()
        .map(|function| json!({"type":"function", "function":function}))
        .collect();
    let settings = &lock.settings;
    Ok(
        json!({"model":lock.identity.model, "messages":messages, "tools":tools,
        "tool_choice":"auto", "parallel_tool_calls":true, "stream":true,
        "stream_options":{"include_usage":true}, "max_tokens":request.maximum_tokens,
        "reasoning_format":"deepseek",
        "chat_template_kwargs":{"enable_thinking":thinking == Thinking::On},
        "temperature":settings.temperature, "top_p":settings.top_p, "top_k":settings.top_k,
        "min_p":settings.min_p, "repeat_penalty":settings.repetition_penalty,
        "repeat_last_n":settings.penalty_last_n, "presence_penalty":settings.presence_penalty,
        "frequency_penalty":settings.frequency_penalty, "seed":request.sampling_seed}),
    )
}

/// A Chat Completions body. llama-server renders each tool schema's keys in
/// the order they arrive, and sorted keys put `additionalProperties` first and
/// `type` last, which small models such as LFM2.5-350M misread. So the tools
/// are written as they usually are, and the body's other fields keep their
/// sorted order.
struct ChatBody(Value);

impl Serialize for ChatBody {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let Value::Object(fields) = &self.0 else {
            return self.0.serialize(serializer);
        };
        let mut map = serializer.serialize_map(Some(fields.len()))?;
        for (key, value) in fields {
            if key == "tools" {
                map.serialize_entry(key, &Usual(value))?;
            } else {
                map.serialize_entry(key, value)?;
            }
        }
        map.end()
    }
}

/// The keys that tool definitions usually list first, in this order: a tool's
/// `type` and `function`; a function's `name`, `description` and `parameters`;
/// and a schema's `type`, `description`, `properties` and `required`.
const LEADING: [&str; 7] = [
    "type",
    "function",
    "name",
    "description",
    "parameters",
    "properties",
    "required",
];

/// Tool definitions with each object's [`LEADING`] keys first and its other
/// keys sorted. Property names keep their sorted order.
struct Usual<'a>(&'a Value);

impl Serialize for Usual<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        match self.0 {
            Value::Array(items) => serializer.collect_seq(items.iter().map(Usual)),
            Value::Object(fields) => {
                let mut fields: Vec<_> = fields.iter().collect();
                // The sort is stable, so the other keys stay sorted.
                fields.sort_by_key(|(key, _)| {
                    LEADING
                        .iter()
                        .position(|leading| leading == key)
                        .unwrap_or(LEADING.len())
                });
                let mut map = serializer.serialize_map(Some(fields.len()))?;
                for (key, value) in fields {
                    match value {
                        Value::Object(properties) if key == "properties" => {
                            map.serialize_entry(key, &Properties(properties))?;
                        }
                        _ => map.serialize_entry(key, &Usual(value))?,
                    }
                }
                map.end()
            }
            value => value.serialize(serializer),
        }
    }
}

/// A schema's properties: names in their sorted order, each mapped to a schema
/// written as usual.
struct Properties<'a>(&'a serde_json::Map<String, Value>);

impl Serialize for Properties<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.collect_map(self.0.iter().map(|(name, schema)| (name, Usual(schema))))
    }
}
