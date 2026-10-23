use std::sync::LazyLock;

use serde::Serialize;
use serde_json::{Value, json};

pub(crate) static TOOLS: LazyLock<Vec<Value>> = LazyLock::new(|| {
    serde_json::from_str(include_str!("tools.json")).expect("versioned tool definitions")
});

struct PythonFloats;

impl serde_json::ser::Formatter for PythonFloats {
    fn write_f64<W: ?Sized + std::io::Write>(
        &mut self,
        writer: &mut W,
        value: f64,
    ) -> std::io::Result<()> {
        // Rust Debug and Python repr use shortest round-trippable digits and
        // the same scientific-notation thresholds. Python additionally keeps
        // the exponent sign and at least two exponent digits.
        let text = format!("{value:?}");
        if let Some((mantissa, exponent)) = text.split_once('e') {
            let exponent: i32 = exponent.parse().expect("formatted finite exponent");
            write!(writer, "{mantissa}e{exponent:+03}")
        } else {
            writer.write_all(text.as_bytes())
        }
    }
}

/// Match Python's canonical ensure_ascii=True representation for context sizing
/// and exact tool-message content. JSON object keys are sorted by serde_json.
pub(crate) fn canonical(value: &Value) -> String {
    let mut bytes = Vec::new();
    value
        .serialize(&mut serde_json::Serializer::with_formatter(
            &mut bytes,
            PythonFloats,
        ))
        .expect("JSON values are serializable");
    let encoded = String::from_utf8(bytes).expect("JSON is UTF-8");
    let mut output = String::with_capacity(encoded.len());
    for character in encoded.chars() {
        if character >= '\u{7f}' {
            for unit in character.encode_utf16(&mut [0; 2]) {
                use std::fmt::Write;
                write!(output, "\\u{unit:04x}").expect("string writing");
            }
        } else {
            output.push(character);
        }
    }
    output
}

pub(crate) fn encoded_size(value: &Value) -> u64 {
    canonical(value).len() as u64
}

#[derive(Default)]
pub(crate) struct ContextWindow {
    messages: Vec<Value>,
    tools: Vec<Value>,
    tokens: Option<u64>,
}

impl ContextWindow {
    pub fn observe(
        &mut self,
        messages: &[Value],
        tools: &[Value],
        prompt: Option<u64>,
        tokens: u64,
    ) {
        if let Some(prompt) = prompt.filter(|n| *n > 0) {
            self.messages = messages.to_vec();
            self.tools = tools.to_vec();
            self.tokens = Some(prompt.saturating_add(tokens));
        }
    }

    pub fn estimate(&self, messages: &[Value], tools: &[Value]) -> (u64, &'static str) {
        if let Some(tokens) = self.tokens
            && messages.starts_with(&self.messages)
        {
            let extra = encoded_size(&json!(&messages[self.messages.len()..]));
            let changed_tools = if tools == self.tools {
                0
            } else {
                encoded_size(&json!(tools))
            };
            return (
                tokens.saturating_add(extra).saturating_add(changed_tools),
                "server_usage_plus_new_bytes",
            );
        }
        (
            encoded_size(&json!({"messages": messages, "tools": tools})),
            "input_bytes",
        )
    }
}

pub(crate) fn message(role: &str, content: impl Into<String>) -> Value {
    json!({"role": role, "content": content.into()})
}

pub(crate) fn tool_message(name: &str, result: &Value) -> Value {
    let name = if name.is_empty() { "unknown" } else { name };
    json!({"role": "tool", "tool_name": name, "content": canonical(result)})
}

pub(crate) fn fit_tool_result(name: &str, result: Value, maximum: u64) -> Value {
    if encoded_size(&tool_message(name, &result)) <= maximum {
        return result;
    }
    let mut limited = json!({
        "context_truncated": true,
        "notice": "Tool output shortened to reserve space for the session handoff."
    });
    for key in [
        "exit_code",
        "timed_out",
        "bytes_written",
        "offset_bytes",
        "size_bytes",
    ] {
        if let Some(value) = result.get(key) {
            limited[key] = value.clone();
        }
    }
    for key in ["error", "stderr", "stdout", "content"] {
        if let Some(text) = result.get(key).and_then(Value::as_str) {
            let boundaries: Vec<usize> = text
                .char_indices()
                .map(|(i, _)| i)
                .chain([text.len()])
                .collect();
            let (mut low, mut high) = (0, boundaries.len() - 1);
            while low < high {
                let middle = (low + high).div_ceil(2);
                limited[key] = json!(&text[..boundaries[middle]]);
                if encoded_size(&tool_message(name, &limited)) <= maximum {
                    low = middle;
                } else {
                    high = middle - 1;
                }
            }
            if low > 0 {
                limited[key] = json!(&text[..boundaries[low]]);
            } else {
                limited.as_object_mut().unwrap().remove(key);
            }
        }
    }
    limited
}

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

/// Shared prompt construction and conservative initial-context validation. This
/// estimates bytes, not exact tokenizer usage or available GPU memory.
pub struct SessionInput<'a> {
    pub parameters: &'a playtime_core::RunParameters,
    pub protocol: &'a str,
    pub prompt: &'a str,
    pub inventory: &'a str,
}
impl SessionInput<'_> {
    pub(crate) fn messages(
        &self,
        session: u32,
        remaining: u64,
        memory: &crate::Memory,
    ) -> Vec<Value> {
        let l = &self.parameters.limits;
        let budget = json!({"generated_tokens_remaining": remaining,
            "wall_seconds_limit": l.wall_seconds, "session": session, "sessions_total": l.sessions});
        vec![
            message("system", self.protocol),
            message(
                "user",
                format!(
                    "{}\nSession {}/{}.\nBudget: {}\nMemento limit: {} UTF-8 bytes.\n\n{}\n\nMemento status: {}\n{}",
                    self.prompt,
                    session,
                    l.sessions,
                    canonical(&budget),
                    l.memento_bytes,
                    self.inventory,
                    memory.status,
                    memory.content
                ),
            ),
        ]
    }
    pub fn initial_context_required(&self) -> u64 {
        let l = &self.parameters.limits;
        let messages = self.messages(
            1,
            l.generated_tokens.get(),
            &crate::Memory {
                status: "missing".into(),
                content: String::new(),
            },
        );
        let (input, _) = ContextWindow::default().estimate(&messages, &TOOLS);
        let output = l.tokens_per_call.get().min(
            l.generated_tokens
                .get()
                .saturating_sub(l.handoff_tokens.get()),
        );
        input
            .saturating_add(output)
            .saturating_add(request_context_reserve(l, false))
            .saturating_add(1)
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_spelling_and_unicode_match_the_python_context_oracle() {
        let fixture: Value =
            serde_json::from_str(include_str!("../tests/fixtures/canonical.json")).unwrap();
        assert_eq!(canonical(&fixture["value"]), fixture["expected"]);
    }

    #[test]
    fn unicode_and_nested_tool_output_keep_the_python_byte_bound() {
        assert_eq!(
            canonical(&json!({"😀": "é\u{7f}"})),
            "{\"\\ud83d\\ude00\":\"\\u00e9\\u007f\"}"
        );
        let original = json!({"stdout": "\"\\\n\0😀".repeat(1000), "stderr": "failure ".repeat(1000), "exit_code": 1, "timed_out": false});
        for maximum in [512, 1024, 12000] {
            let fitted = fit_tool_result("exec", original.clone(), maximum);
            assert!(encoded_size(&tool_message("exec", &fitted)) <= maximum);
            assert_eq!(fitted["exit_code"], 1);
            assert_eq!(fitted["timed_out"], false);
        }
    }

    #[test]
    fn measured_context_requires_the_same_prefix() {
        let mut window = ContextWindow::default();
        let initial = vec![
            message("user", "Explore"),
            message("assistant", "Look around"),
        ];
        window.observe(&initial, &TOOLS, Some(100), 20);
        let mut current = initial.clone();
        current.push(message("tool", "Evidence"));
        assert_eq!(
            window.estimate(&current, &TOOLS),
            (
                120 + encoded_size(&json!(&current[2..])),
                "server_usage_plus_new_bytes"
            )
        );
        current[0] = message("user", "New session");
        assert_eq!(window.estimate(&current, &TOOLS).1, "input_bytes");
    }
}
