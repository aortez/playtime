//! Frozen memento capabilities, used independently by runtime admission.
//! These functions derive bounds from the accepted configuration, never from
//! a policy-provided estimate or allowance.

use std::sync::LazyLock;

use playtime_core::ResourceLimits;
use serde_json::{Value, json};

use crate::ToolRequest;

pub(crate) static TOOLS: LazyLock<Vec<Value>> = LazyLock::new(|| {
    serde_json::from_str(include_str!("../tools.json")).expect("versioned tool definitions")
});

pub(crate) fn validate_call(name: &str, arguments: &Value) -> Option<String> {
    let schema = TOOLS.iter().find(|tool| tool["function"]["name"] == name);
    let (Some(tool), Some(arguments)) = (schema, arguments.as_object()) else {
        return Some("Unknown tool or invalid arguments object".into());
    };
    let schema = &tool["function"]["parameters"];
    let properties = schema["properties"].as_object().unwrap();
    if arguments.keys().any(|key| !properties.contains_key(key))
        || schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .any(|key| !arguments.contains_key(key.as_str().unwrap()))
    {
        return Some("Unexpected or missing tool arguments".into());
    }
    for (key, value) in arguments {
        let valid = match properties[key]["type"].as_str().unwrap() {
            "string" => value.is_string(),
            "boolean" => value.is_boolean(),
            "integer" => value.is_i64() || value.is_u64(),
            "number" => value.is_number(),
            _ => false,
        };
        if !valid {
            return Some(format!("Invalid type for {key}"));
        }
    }
    None
}

pub(crate) fn request_context_reserve(
    limits: &playtime_core::ResourceLimits,
    handoff: bool,
) -> u64 {
    if handoff {
        4096
    } else {
        8192_u64
            .saturating_add(limits.handoff_tokens.get())
            .saturating_add(
                limits
                    .output_bytes
                    .get()
                    .min(limits.context_tokens.get() / 4),
            )
    }
}

pub(crate) fn tools(handoff: bool) -> Vec<Value> {
    if handoff {
        vec![TOOLS.last().unwrap().clone()]
    } else {
        TOOLS.clone()
    }
}

pub(crate) fn allowance(limits: &ResourceLimits, remaining: u64, handoff: bool) -> u64 {
    let (cap, reserve) = if handoff {
        (limits.handoff_tokens.get(), 0)
    } else {
        (limits.tokens_per_call.get(), limits.handoff_tokens.get())
    };
    cap.min(remaining.saturating_sub(reserve))
}

pub(crate) fn parse_ceiling(limits: &ResourceLimits) -> u32 {
    limits
        .tool_parse_error_limit
        .map_or(limits.tool_parse_retries + 1, |n| n.get())
}

pub(crate) fn tool_error(
    request: &ToolRequest,
    limits: &ResourceLimits,
    ended: bool,
    handoff: bool,
) -> Option<String> {
    let mut error = validate_call(&request.name, &request.arguments);
    if ended {
        error = Some("The session has already ended".into());
    }
    if handoff && request.name != "end_session" {
        error = Some("Only end_session is available during handoff".into());
    }
    if error.is_none()
        && request.name == "end_session"
        && request.arguments["memento"].as_str().unwrap().len() as u64 > limits.memento_bytes.get()
    {
        error = Some("Memento exceeds its byte limit; shorten it and retry".into());
    }
    error
}

/// Called only after tool_error accepts the model-facing call. The runtime
/// retains the original call and verifies the write outcome before recording
/// handoff/finish candidates; the policy cannot supply a different operation.
pub(crate) fn workshop_request(request: &ToolRequest) -> ToolRequest {
    if request.name == "end_session" {
        ToolRequest {
            name: "write_file".into(),
            arguments: json!({"path": "/work/MEMENTO.md", "content": request.arguments["memento"]}),
        }
    } else {
        request.clone()
    }
}
