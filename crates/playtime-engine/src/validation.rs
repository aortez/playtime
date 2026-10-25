//! Common response/tool schema validation, independent of conversation policy.
use serde_json::Value;

pub(crate) fn validate_call(tools: &[Value], name: &str, arguments: &Value) -> Option<String> {
    let schema = tools.iter().find(|tool| tool["function"]["name"] == name);
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

pub(crate) fn valid_message(message: &Value) -> bool {
    message.is_object()
        && message["role"] == "assistant"
        && ["content", "thinking"]
            .iter()
            .all(|key| message.get(key).is_none_or(Value::is_string))
        && message.get("tool_calls").is_none_or(Value::is_array)
}
