use serde::Serialize;
use serde_json::{Value, json};

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::memento_contract::TOOLS;

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
