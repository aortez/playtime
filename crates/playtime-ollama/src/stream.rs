use crate::{
    Failure, FailureKind, Limits, Observation, ObservationKind, ObservationSender, Profile, Result,
    json,
};
use playtime_engine::ModelResponse;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json as value};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AssistantMessage {
    pub content: Option<String>,
    pub thinking: Option<String>,
    pub tool_calls: Option<Vec<Value>>,
}
impl AssistantMessage {
    pub fn value(&self) -> Value {
        let mut message = value!({"role":"assistant"});
        if let Some(s) = &self.content {
            message["content"] = value!(s);
        }
        if let Some(s) = &self.thinking {
            message["thinking"] = value!(s);
        }
        if let Some(calls) = &self.tool_calls {
            message["tool_calls"] = value!(calls);
        }
        message
    }
    fn append(&mut self, part: &Value, size: &mut usize, limit: usize) -> Result<()> {
        let fail = || {
            Failure::new(
                FailureKind::Protocol,
                "malformed native assistant message/tool object",
            )
        };
        let part = part.as_object().ok_or_else(fail)?;
        if let Some(role) = part.get("role")
            && role.as_str() != Some("assistant")
        {
            return Err(fail());
        }
        for key in ["content", "thinking"] {
            if let Some(v) = part.get(key)
                && !v.is_string()
            {
                return Err(fail());
            }
        }
        if let Some(v) = part.get("tool_calls") {
            let calls = v.as_array().ok_or_else(fail)?;
            for call in calls {
                let call = call.as_object().ok_or_else(fail)?;
                if call.get("id").is_some_and(|v| !v.is_string()) {
                    return Err(fail());
                }
                let function = call
                    .get("function")
                    .and_then(Value::as_object)
                    .ok_or_else(fail)?;
                if !function.get("name").is_some_and(Value::is_string)
                    || !function.get("arguments").is_some_and(Value::is_object)
                    || function.get("index").is_some_and(|v| v.as_u64().is_none())
                {
                    return Err(fail());
                }
            }
        }
        let content = part.get("content").and_then(Value::as_str);
        let thinking = part.get("thinking").and_then(Value::as_str);
        let calls = part.get("tool_calls").and_then(Value::as_array);
        let tools_size = calls
            .map(|calls| {
                serde_json::to_vec(calls)
                    .expect("JSON value encoding")
                    .len()
            })
            .unwrap_or(0);
        let next = size
            .checked_add(content.map_or(0, str::len))
            .and_then(|n| n.checked_add(thinking.map_or(0, str::len)))
            .and_then(|n| n.checked_add(tools_size));
        *size = next.filter(|n| *n <= limit).ok_or_else(|| {
            Failure::new(FailureKind::ByteLimit, "assembled message exceeds bound")
        })?;
        if let Some(text) = content {
            self.content.get_or_insert_default().push_str(text);
        }
        if let Some(text) = thinking {
            self.thinking.get_or_insert_default().push_str(text);
        }
        if let Some(calls) = calls {
            self.tool_calls
                .get_or_insert_default()
                .extend(calls.iter().cloned());
        }
        Ok(())
    }
}

/// Framing is independent of HTTP and legacy error classification. A result is
/// yielded once, at the first terminal boundary; all later bytes are discarded.
pub struct Decoder {
    limits: Limits,
    buffer: Vec<u8>,
    diagnostic: Vec<u8>,
    partial: AssistantMessage,
    assembled: usize,
    consumed: usize,
    records: u64,
    finished: bool,
    terminal_observed: bool,
}
impl Decoder {
    pub fn new(limits: Limits) -> Result<Self> {
        limits.validate()?;
        Ok(Self {
            limits,
            buffer: vec![],
            diagnostic: vec![],
            partial: AssistantMessage::default(),
            assembled: 0,
            consumed: 0,
            records: 0,
            finished: false,
            terminal_observed: false,
        })
    }
    pub fn partial(&self) -> &AssistantMessage {
        &self.partial
    }
    pub fn consumed_bytes(&self) -> usize {
        self.consumed
    }
    pub fn records(&self) -> u64 {
        self.records
    }
    pub fn terminal_observed(&self) -> bool {
        self.terminal_observed
    }
    pub fn pending_fragment(&self) -> &[u8] {
        if self.diagnostic.is_empty() {
            &self.buffer
        } else {
            &self.diagnostic
        }
    }
    fn finish_with(&mut self, result: Result<ModelResponse>) -> Option<Result<ModelResponse>> {
        self.finished = true;
        Some(result)
    }
    pub fn feed(
        &mut self,
        mut bytes: &[u8],
        observations: &ObservationSender,
    ) -> Option<Result<ModelResponse>> {
        if self.finished {
            return None;
        }
        while !bytes.is_empty() {
            let count = bytes
                .iter()
                .position(|b| *b == b'\n')
                .map_or(bytes.len(), |n| n + 1);
            let allowed = self
                .limits
                .record_bytes
                .saturating_sub(self.buffer.len())
                .min(self.limits.response_bytes.saturating_sub(self.consumed));
            let take = count.min(allowed);
            self.buffer.extend_from_slice(&bytes[..take]);
            let Some(consumed) = self.consumed.checked_add(take) else {
                return self.finish_with(Err(Failure::new(
                    FailureKind::ByteLimit,
                    "NDJSON byte count overflow",
                )));
            };
            self.consumed = consumed;
            if take < count {
                return self.finish_with(Err(Failure::new(
                    FailureKind::ByteLimit,
                    "NDJSON record/body bound exceeded",
                )));
            }
            bytes = &bytes[take..];
            if self.buffer.last() == Some(&b'\n') {
                let raw = std::mem::take(&mut self.buffer);
                match self.record(raw, observations) {
                    Ok(None) => (),
                    Ok(Some(response)) => return self.finish_with(Ok(response)),
                    Err(error) => return self.finish_with(Err(error)),
                }
            }
        }
        None
    }
    pub fn eof(&mut self, observations: &ObservationSender) -> Option<Result<ModelResponse>> {
        if self.finished {
            return None;
        }
        if !self.buffer.is_empty() {
            let raw = std::mem::take(&mut self.buffer);
            match self.record(raw, observations) {
                Ok(Some(response)) => return self.finish_with(Ok(response)),
                Err(error) => return self.finish_with(Err(error)),
                Ok(None) => (),
            }
        }
        self.finish_with(Err(Failure::new(
            FailureKind::Incomplete,
            "EOF before a terminal native response",
        )))
    }
    fn record(
        &mut self,
        raw: Vec<u8>,
        observations: &ObservationSender,
    ) -> Result<Option<ModelResponse>> {
        self.records = self
            .records
            .checked_add(1)
            .ok_or_else(|| Failure::new(FailureKind::ByteLimit, "NDJSON record count overflow"))?;
        if self.records > self.limits.records {
            self.diagnostic = raw;
            return Err(Failure::new(
                FailureKind::ByteLimit,
                "NDJSON record count exceeded",
            ));
        }
        let parsed = json::parse(&raw, self.limits.record_bytes, false);
        self.diagnostic = raw.clone();
        observations.record(Observation {
            kind: if parsed.is_ok() {
                ObservationKind::NativeChunk
            } else {
                ObservationKind::ProtocolFragment
            },
            bytes: raw,
        })?;
        self.diagnostic.clear();
        let chunk = parsed?;
        let object = chunk.as_object().ok_or_else(|| {
            Failure::new(FailureKind::Protocol, "native record must be an object")
        })?;
        if let Some(error) = object.get("error") {
            let error = error.as_str().ok_or_else(|| {
                Failure::new(FailureKind::Protocol, "native error must be a string")
            })?;
            return Err(Failure::new(
                if Profile::tool_parse_error(error) {
                    FailureKind::BackendToolParse
                } else {
                    FailureKind::Http
                },
                error,
            ));
        }
        crate::identity::local(&chunk)?;
        if object
            .get("model")
            .is_some_and(|v| v.as_str() != Some(Profile::MODEL))
        {
            return Err(Failure::new(
                FailureKind::Identity,
                "response model changed",
            ));
        }
        let done = match object.get("done") {
            None => false,
            Some(Value::Bool(done)) => *done,
            _ => return Err(Failure::new(FailureKind::Protocol, "done must be boolean")),
        };
        if let Some(part) = object.get("message") {
            self.partial
                .append(part, &mut self.assembled, self.limits.assembled_bytes)?;
        }
        if !done {
            return Ok(None);
        }
        self.terminal_observed = true;
        let generated = object
            .get("eval_count")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                Failure::new(
                    FailureKind::MissingUsage,
                    "terminal response lacks valid nonnegative integer generated usage",
                )
            })?;
        Ok(Some(ModelResponse {
            message: self.partial.value(),
            done: true,
            generated_tokens: Some(generated),
            prompt_tokens: object.get("prompt_eval_count").and_then(Value::as_u64),
        }))
    }
}
