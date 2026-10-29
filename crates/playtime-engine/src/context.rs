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

/// The densest server measurement of this run: a request's prompt tokens and
/// the canonical bytes of the same messages and tools. Unmeasured input is
/// estimated from this rate instead of counting every byte as a token.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
pub struct Calibration {
    pub tokens: u64,
    pub bytes: u64,
}

/// Unmeasured text is assumed this much denser than the densest measured
/// prompt, as a fraction: 3/2. Summary transcripts leave out thinking, so they
/// pack more tokens per byte than the work prompts they come from; the
/// 30-minute Framework runs measured up to 1.47x. This is a heuristic, not a
/// bound: content denser than the measured prompts, such as base64, hex or
/// minified code, is underestimated until the server measures it, and capping
/// the estimate at the byte count only stops it exceeding bytes; it does not
/// keep the byte bound's conservatism. Work requests count their unmeasured
/// tail in bytes, so the estimate only ever covers the summary transcript and
/// the replacement, and the summary request measures the transcript before
/// the replacement check.
const DENSITY_NUMERATOR: u64 = 3;
const DENSITY_DENOMINATOR: u64 = 2;

/// Tokens for `bytes` of unmeasured input and the basis that produced them.
pub(crate) fn calibrated_tokens(
    bytes: u64,
    calibration: Option<Calibration>,
) -> (u64, &'static str) {
    match calibration {
        Some(Calibration {
            tokens,
            bytes: measured,
        }) if measured > 0 && tokens > 0 => {
            let scaled = u128::from(bytes) * u128::from(tokens) * u128::from(DENSITY_NUMERATOR);
            let estimate = scaled.div_ceil(u128::from(measured) * u128::from(DENSITY_DENOMINATOR));
            (
                u64::try_from(estimate).unwrap_or(u64::MAX).min(bytes),
                "calibrated_bytes",
            )
        }
        _ => (bytes, "input_bytes"),
    }
}

#[derive(Default)]
pub(crate) struct ContextWindow {
    messages: Vec<Value>,
    tools: Vec<Value>,
    tokens: Option<u64>,
    calibration: Option<Calibration>,
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

    /// Record the server's prompt measurement of a request's exact input. The
    /// densest measurement wins, so content that tokenizes badly, such as
    /// base64, only makes later estimates more conservative.
    pub fn calibrate(&mut self, messages: &[Value], tools: &[Value], prompt: Option<u64>) {
        let Some(tokens) = prompt.filter(|n| *n > 0) else {
            return;
        };
        let bytes = encoded_size(&json!({"messages": messages, "tools": tools}));
        if bytes == 0 {
            return;
        }
        let candidate = Calibration { tokens, bytes };
        let denser = self.calibration.is_none_or(|current| {
            u128::from(candidate.tokens) * u128::from(current.bytes)
                > u128::from(current.tokens) * u128::from(candidate.bytes)
        });
        if denser {
            self.calibration = Some(candidate);
        }
    }

    pub fn calibration(&self) -> Option<Calibration> {
        self.calibration
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
        calibrated_tokens(
            encoded_size(&json!({"messages": messages, "tools": tools})),
            self.calibration,
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
    fit_tool_result_with_notice(
        name,
        result,
        maximum,
        "Tool output shortened to reserve space for the session handoff.",
    )
}

pub(crate) fn fit_tool_result_with_notice(
    name: &str,
    result: Value,
    maximum: u64,
    notice: &str,
) -> Value {
    if encoded_size(&tool_message(name, &result)) <= maximum {
        return result;
    }
    let mut limited = json!({
        "context_truncated": true,
        "notice": notice
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

    #[test]
    fn denser_content_is_underestimated_until_the_server_measures_it() {
        let mut window = ContextWindow::default();
        let prose = vec![message("user", "words ".repeat(800))];
        let bytes = encoded_size(&json!({"messages": prose, "tools": &*TOOLS}));
        // Prose at five bytes per token.
        window.calibrate(&prose, &TOOLS, Some(bytes / 5));
        // A base64 dump at two bytes per token is estimated at three tokens
        // per ten bytes, below its real five per ten, until it is measured.
        let dense_bytes = 10_000;
        let real = dense_bytes / 2;
        let (estimate, basis) = calibrated_tokens(dense_bytes, window.calibration());
        assert_eq!(
            (estimate, basis),
            ((dense_bytes * 3).div_ceil(10), "calibrated_bytes")
        );
        assert!(estimate < real);
        // The server measures such a prompt; the densest measurement wins and
        // later estimates cover it, at three tokens per four bytes.
        let dense = vec![message("tool", "A".repeat(10_000))];
        let dense_total = encoded_size(&json!({"messages": dense, "tools": &*TOOLS}));
        window.calibrate(&dense, &TOOLS, Some(dense_total / 2));
        assert_eq!(
            window.calibration(),
            Some(Calibration {
                tokens: dense_total / 2,
                bytes: dense_total
            })
        );
        let (after, _) = calibrated_tokens(dense_bytes, window.calibration());
        assert_eq!(after, (dense_bytes * 3).div_ceil(4));
        assert!(after >= real && after < dense_bytes);
        // A later sparse prompt does not loosen it.
        window.calibrate(&prose, &TOOLS, Some(bytes / 5));
        assert_eq!(window.calibration().unwrap().bytes, dense_total);
    }

    #[test]
    fn calibration_keeps_the_densest_measurement_and_never_exceeds_bytes() {
        let mut window = ContextWindow::default();
        let messages = vec![message("user", "x".repeat(4000))];
        assert_eq!(
            calibrated_tokens(900, window.calibration()),
            (900, "input_bytes")
        );
        // Four bytes per token, then a denser prompt at two, then a sparser one
        // that must not replace it.
        let bytes = encoded_size(&json!({"messages": messages, "tools": &*TOOLS}));
        window.calibrate(&messages, &TOOLS, Some(bytes / 4));
        assert_eq!(
            window.calibration(),
            Some(Calibration {
                tokens: bytes / 4,
                bytes
            })
        );
        window.calibrate(&messages, &TOOLS, Some(bytes / 2));
        window.calibrate(&messages, &TOOLS, Some(bytes / 8));
        window.calibrate(&messages, &TOOLS, None);
        window.calibrate(&messages, &TOOLS, Some(0));
        let calibration = window.calibration().unwrap();
        assert_eq!(calibration.tokens, bytes / 2);
        // Two bytes per token, assumed half again as dense: three tokens per
        // four bytes, rounded up, and never more than one per byte.
        assert_eq!(
            calibrated_tokens(4000, Some(calibration)).0,
            (4000 * 3_u64).div_ceil(4)
        );
        assert_eq!(
            calibrated_tokens(4000, Some(calibration)).1,
            "calibrated_bytes"
        );
        assert_eq!(
            calibrated_tokens(
                10,
                Some(Calibration {
                    tokens: 10,
                    bytes: 10
                })
            ),
            (10, "calibrated_bytes")
        );
        assert_eq!(
            calibrated_tokens(
                10,
                Some(Calibration {
                    tokens: 0,
                    bytes: 10
                })
            )
            .1,
            "input_bytes"
        );
        let other = vec![message("user", "New session")];
        assert_eq!(window.estimate(&other, &TOOLS).1, "calibrated_bytes");
        assert!(
            window.estimate(&other, &TOOLS).0
                < encoded_size(&json!({"messages": other, "tools": &*TOOLS}))
        );
    }
}
