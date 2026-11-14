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
/// keep the byte bound's conservatism. Requests add their new bytes one to
/// one to a measured prefix or, when a response brought no measurement, to
/// the admitted estimate of the request before them, so the estimate only
/// ever covers the summary transcript and the replacement, and the summary
/// request measures the transcript before the replacement check.
const DENSITY_NUMERATOR: u64 = 3;
const DENSITY_DENOMINATOR: u64 = 2;

/// The basis of an estimate that extends a server-measured prefix.
const MEASURED_PREFIX: &str = "server_usage_plus_new_bytes";
/// The basis of an estimate that extends an admitted calibrated estimate
/// whose response brought no measurement.
const ESTIMATED_PREFIX: &str = "calibrated_prefix_plus_new_bytes";

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
pub struct ContextWindow {
    messages: Vec<Value>,
    tools: Vec<Value>,
    tokens: Option<u64>,
    /// The basis later estimates report when they extend the anchor.
    basis: &'static str,
    calibration: Option<Calibration>,
}

impl ContextWindow {
    /// Anchor the window on a request and its response. A server prompt
    /// measurement anchors the exact input. Without one, a request that
    /// extended an anchor keeps it, so its tail stays counted in bytes, and a
    /// request estimated whole at the calibrated rate, such as the replacement
    /// after a compaction, anchors the window at its admitted estimate: the
    /// requests after it add their new bytes one to one instead of being
    /// rescaled whole, new tool output included. Byte-counted input needs no
    /// anchor; its bytes already bound every later request.
    pub fn observe(
        &mut self,
        messages: &[Value],
        tools: &[Value],
        prompt: Option<u64>,
        tokens: u64,
        admitted: u64,
        admitted_basis: &str,
    ) {
        let (anchor, basis) = match prompt.filter(|n| *n > 0) {
            Some(prompt) => (prompt, MEASURED_PREFIX),
            None if admitted_basis == "calibrated_bytes" || admitted_basis == ESTIMATED_PREFIX => {
                (admitted, ESTIMATED_PREFIX)
            }
            None => return,
        };
        self.messages = messages.to_vec();
        self.tools = tools.to_vec();
        self.tokens = Some(anchor.saturating_add(tokens));
        self.basis = basis;
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
                self.basis,
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

/// The least that runs of base64 must total in a `read_file` slice before
/// they are withheld.
const BASE64_GUARD_BYTES: usize = 1024;
/// A run of base64 characters this long is base64.
const BASE64_RUN: usize = 64;
const BASE64_NOTE: &str = "Runs of base64 in this slice were replaced by markers that say their size. Use exec to read, decode or summarize the file as is, or offset_bytes past slice_bytes to read on.";

/// A `read_file` result as the model receives it. Runs of `BASE64_RUN` or more
/// base64 characters, when one of them holds a `+` or `/` (hex, digits and
/// letters alone never do) and they total `BASE64_GUARD_BYTES` or more, teach
/// the model nothing at a high price in context: embedded images in an HTML
/// page cost run 8 a compaction. Each such run becomes a marker with its byte
/// count, the rest of the slice stays, and `base64` says what was withheld.
/// The archive keeps the whole slice as the unabridged result.
pub(crate) fn withhold_base64(mut result: Value) -> Value {
    let Some(content) = result["content"].as_str() else {
        return result;
    };
    let runs = base64_runs(content);
    let withheld: usize = runs.iter().map(|(start, end)| end - start).sum();
    if withheld < BASE64_GUARD_BYTES {
        return result;
    }
    let mut shown = String::with_capacity(content.len() - withheld + runs.len() * 40);
    let mut last = 0;
    for &(start, end) in &runs {
        shown.push_str(&content[last..start]);
        shown.push_str(&format!("[… {} bytes of base64 withheld …]", end - start));
        last = end;
    }
    shown.push_str(&content[last..]);
    let slice_bytes = content.len();
    result["content"] = json!(shown);
    result["base64"] = json!({
        "withheld_bytes": withheld,
        "slice_bytes": slice_bytes,
        "note": BASE64_NOTE,
    });
    result
}

/// The byte ranges of the runs of `BASE64_RUN` or more base64 characters in
/// `content`, when at least one of them holds a `+` or `/`; otherwise none.
/// Base64 characters are ASCII, so every range ends on a character boundary.
fn base64_runs(content: &str) -> Vec<(usize, usize)> {
    let bytes = content.as_bytes();
    let mut runs = Vec::new();
    let mut symbols = false;
    let mut run: Option<(usize, bool)> = None;
    for (i, &byte) in bytes
        .iter()
        .enumerate()
        .chain(std::iter::once((bytes.len(), &b'\n')))
    {
        let is_base64 = byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'=');
        match (run, is_base64) {
            (None, true) => run = Some((i, matches!(byte, b'+' | b'/'))),
            (Some((start, symbol)), true) => {
                run = Some((start, symbol || matches!(byte, b'+' | b'/')));
            }
            (Some((start, symbol)), false) => {
                if i - start >= BASE64_RUN {
                    runs.push((start, i));
                    symbols |= symbol;
                }
                run = None;
            }
            (None, false) => {}
        }
    }
    if symbols { runs } else { Vec::new() }
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
        "context_omitted_bytes": {},
        "notice": notice
    });
    for key in [
        "exit_code",
        "timed_out",
        "duration_seconds",
        "output_truncated",
        "truncated",
        "bytes_written",
        "offset_bytes",
        "size_bytes",
        "base64",
    ] {
        if let Some(value) = result.get(key) {
            limited[key] = value.clone();
        }
    }
    let fields: Vec<_> = ["error", "stderr", "stdout", "content", "path"]
        .into_iter()
        .filter_map(|key| result.get(key).and_then(Value::as_str).map(|s| (key, s)))
        .collect();
    // Reserve every field before allocating text. An empty stderr must remain
    // distinguishable from one whose diagnostics were omitted.
    for &(key, text) in &fields {
        set_context_text(&mut limited, key, "", text.len());
        if encoded_size(&json!(text)) <= encoded_size(&limited[key]) {
            set_context_text(&mut limited, key, text, 0);
        }
    }
    if encoded_size(&tool_message(name, &limited)) > maximum {
        // The inline markers already explain the loss; status takes priority
        // over the redundant policy notice at the minimum result allowance.
        limited.as_object_mut().unwrap().remove("notice");
    }
    // If mandatory metadata alone cannot fit, leave it intact. Admission's
    // minimum covers workshop results; the controller rejects an oversized
    // envelope instead of delivering it beyond the frozen allowance.
    for (key, text) in fields {
        set_context_text(&mut limited, key, text, 0);
        if encoded_size(&tool_message(name, &limited)) <= maximum {
            continue;
        }
        let boundaries: Vec<usize> = text.char_indices().map(|(i, _)| i).collect();
        if boundaries.is_empty() {
            continue;
        }
        let (mut low, mut high) = (0, boundaries.len() - 1);
        while low < high {
            let middle = (low + high).div_ceil(2);
            // Reserve the original count's digits while searching. Updating
            // the actual count here can shrink both marker and metadata at a
            // power of ten, making the encoded size non-monotonic.
            set_context_text(&mut limited, key, &text[..boundaries[middle]], text.len());
            if encoded_size(&tool_message(name, &limited)) <= maximum {
                low = middle;
            } else {
                high = middle - 1;
            }
        }
        let retained = boundaries[low];
        set_context_text(&mut limited, key, &text[..retained], text.len() - retained);
    }
    limited
}

fn set_context_text(result: &mut Value, key: &str, prefix: &str, omitted: usize) {
    result["context_omitted_bytes"][key] = json!(omitted);
    result[key] = if omitted == 0 {
        json!(prefix)
    } else {
        json!(format!(
            "{prefix}\n[... {omitted} UTF-8 bytes omitted from context; tail dropped]"
        ))
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::memento_contract::TOOLS;

    #[test]
    fn base64_runs_are_withheld_as_markers_and_the_rest_of_the_slice_stays() {
        // A PNG's base64, with the `+` and `/` that binary data brings.
        let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";
        let base64 = |n: usize| png.repeat(n);
        let result =
            |content: String| json!({"content": content, "offset_bytes": 0, "truncated": true});
        // A gallery page: the markup stays, each image becomes a marker.
        let page = format!(
            "<figure><img src=\"data:image/png;base64,{}\"><figcaption>Voronoi</figcaption></figure>\n<figure><img src=\"data:image/png;base64,{}\"><figcaption>Mandelbrot</figcaption></figure>\n",
            base64(30),
            base64(40)
        );
        let kept = withhold_base64(result(page.clone()));
        let shown = kept["content"].as_str().unwrap();
        assert_eq!(
            shown,
            format!(
                "<figure><img src=\"data:image/png;base64,[… {} bytes of base64 withheld …]\"><figcaption>Voronoi</figcaption></figure>\n<figure><img src=\"data:image/png;base64,[… {} bytes of base64 withheld …]\"><figcaption>Mandelbrot</figcaption></figure>\n",
                png.len() * 30,
                png.len() * 40
            )
        );
        assert_eq!(kept["base64"]["withheld_bytes"], png.len() * 70);
        assert_eq!(kept["base64"]["slice_bytes"], page.len());
        assert_eq!(kept["base64"]["note"], BASE64_NOTE);
        assert_eq!(kept["offset_bytes"], 0);
        assert_eq!(kept["truncated"], true);
        // Markup beyond a tenth of the slice changes nothing.
        let heavy = format!(
            "{}{}",
            "<div class=\"card\">caption text</div>\n".repeat(50),
            base64(160)
        );
        assert!(withhold_base64(result(heavy)).get("base64").is_some());
        // Under a kilobyte of base64, nothing is withheld: prose with a long
        // token, or a short file.
        let prose = format!(
            "{} {}\n",
            "The quick brown fox jumps over the lazy dog.".repeat(40),
            base64(2)
        );
        assert_eq!(withhold_base64(result(prose.clone()))["content"], prose);
        let short = base64(10);
        assert!(short.len() < BASE64_GUARD_BYTES);
        assert_eq!(withhold_base64(result(short.clone()))["content"], short);
        // Base64 in lines of 76 is withheld, and a line without a symbol with
        // it, since another line has one; a short last line stays.
        let body = base64(40);
        let chunks: Vec<&str> = body
            .as_bytes()
            .chunks(76)
            .map(|c| std::str::from_utf8(c).unwrap())
            .collect();
        let lines = format!("{}\n{}\n", chunks.join("\n"), "A".repeat(76));
        let cut = withhold_base64(result(lines.clone()));
        assert!(!cut["content"].as_str().unwrap().contains(&"A".repeat(76)));
        let long: usize = chunks.iter().map(|c| c.len()).filter(|n| *n >= 64).sum();
        assert_eq!(cut["base64"]["withheld_bytes"], long + 76);
        // Hex digests, nucleotides, digits and short base64 fields stay.
        let digests =
            "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08  file\n".repeat(20);
        assert_eq!(withhold_base64(result(digests.clone()))["content"], digests);
        let fasta = format!(">seq\n{}", "ACGTACGTACGTACGTACGT".repeat(4).repeat(20));
        assert_eq!(withhold_base64(result(fasta.clone()))["content"], fasta);
        let digits =
            "3141592653589793238462643383279502884197169399375105820974944592\n".repeat(20);
        assert_eq!(withhold_base64(result(digits.clone()))["content"], digits);
        let fields = (0..40)
            .map(|i| format!("{{\"id\": {i}, \"icon\": \"{}\"}}", &base64(1)[..40]))
            .collect::<Vec<_>>()
            .join(",\n");
        assert!(fields.len() >= BASE64_GUARD_BYTES);
        assert_eq!(withhold_base64(result(fields.clone()))["content"], fields);
        // A run ending at a multibyte character ends on its boundary.
        let wide = format!("{}é{}", base64(15), base64(15));
        let cut = withhold_base64(result(wide));
        assert_eq!(
            cut["content"],
            format!(
                "[… {0} bytes of base64 withheld …]é[… {0} bytes of base64 withheld …]",
                png.len() * 15
            )
        );
        // Without a result text, nothing happens.
        assert_eq!(
            withhold_base64(json!({"error": "missing file"})),
            json!({"error": "missing file"})
        );
    }

    #[test]
    fn numeric_spelling_and_unicode_match_the_python_context_oracle() {
        let fixture: Value =
            serde_json::from_str(include_str!("../tests/fixtures/canonical.json")).unwrap();
        assert_eq!(canonical(&fixture["value"]), fixture["expected"]);
    }

    fn assert_context_excerpt(original: &Value, fitted: &Value, key: &str) {
        let before = original[key].as_str().unwrap();
        let after = fitted[key].as_str().unwrap();
        let omitted = fitted["context_omitted_bytes"][key].as_u64().unwrap() as usize;
        if omitted == 0 {
            assert_eq!(after, before);
        } else {
            let marker =
                format!("\n[... {omitted} UTF-8 bytes omitted from context; tail dropped]");
            let retained = after.strip_suffix(&marker).expect("visible tail marker");
            assert!(before.starts_with(retained));
            assert_eq!(retained.len() + omitted, before.len());
        }
    }

    #[test]
    fn fitting_keeps_complete_results_unchanged() {
        let original = json!({"exit_code":0,"stdout":"ok\n","stderr":"",
            "timed_out":false,"duration_seconds":0.001,"output_truncated":false});
        let maximum = encoded_size(&tool_message("exec", &original));
        assert_eq!(fit_tool_result("exec", original.clone(), maximum), original);
    }

    #[test]
    fn context_cut_preserves_upstream_truncation_timing_and_empty_stderr() {
        // The workshop already capped this output; its boolean cannot tell us
        // how many bytes it discarded. Our count describes only the next cut.
        let original = json!({"exit_code":0,"stdout":"x".repeat(16384),"stderr":"",
            "timed_out":false,"duration_seconds":0.001,"output_truncated":true});
        let fitted = fit_tool_result_with_notice(
            "exec",
            original.clone(),
            16384,
            "Tool output shortened to fit the exchange allowance.",
        );
        assert!(encoded_size(&tool_message("exec", &fitted)) <= 16384);
        assert_eq!(fitted["context_truncated"], true);
        for key in [
            "exit_code",
            "timed_out",
            "duration_seconds",
            "output_truncated",
            "stderr",
        ] {
            assert_eq!(fitted[key], original[key]);
        }
        assert_context_excerpt(&original, &fitted, "stdout");
        assert_context_excerpt(&original, &fitted, "stderr");
        assert!(fitted["context_omitted_bytes"]["stdout"].as_u64().unwrap() > 0);
        assert_eq!(fitted["context_omitted_bytes"]["stderr"], 0);
    }

    #[test]
    fn stderr_diagnostics_take_priority_over_stdout() {
        let original = json!({"exit_code":1,"stdout":"x".repeat(16384),
            "stderr":"fatal: missing input\n","timed_out":false,
            "duration_seconds":0.125,"output_truncated":true});
        let fitted = fit_tool_result("exec", original.clone(), 512);
        assert!(encoded_size(&tool_message("exec", &fitted)) <= 512);
        assert_eq!(fitted["stderr"], original["stderr"]);
        assert_context_excerpt(&original, &fitted, "stderr");
        assert_context_excerpt(&original, &fitted, "stdout");
    }

    #[test]
    fn context_cut_preserves_read_slice_and_base64_provenance() {
        let original = json!({"content":format!("{}\n{}", "a+/b".repeat(1000), "é😀".repeat(1000)),
            "offset_bytes":400,"truncated":true});
        let guarded = withhold_base64(original);
        for maximum in [512, 1024] {
            let fitted = fit_tool_result("read_file", guarded.clone(), maximum);
            assert!(encoded_size(&tool_message("read_file", &fitted)) <= maximum);
            for key in ["offset_bytes", "truncated", "base64"] {
                assert_eq!(fitted[key], guarded[key]);
            }
            assert_context_excerpt(&guarded, &fitted, "content");
        }
    }

    #[test]
    fn unicode_and_nested_tool_output_keep_the_python_byte_bound() {
        assert_eq!(
            canonical(&json!({"😀": "é\u{7f}"})),
            "{\"\\ud83d\\ude00\":\"\\u00e9\\u007f\"}"
        );
        let original = json!({"stdout": "\"\\\n\0😀".repeat(1000), "stderr": "failure ".repeat(1000),
            "exit_code": 1, "timed_out": true, "duration_seconds": 30.001, "output_truncated": true});
        for maximum in (512..768).chain([1024, 12000]) {
            let fitted = fit_tool_result("exec", original.clone(), maximum);
            assert!(encoded_size(&tool_message("exec", &fitted)) <= maximum);
            assert_eq!(fitted["exit_code"], 1);
            assert_eq!(fitted["timed_out"], true);
            assert_eq!(fitted["duration_seconds"], 30.001);
            assert_eq!(fitted["output_truncated"], true);
            assert_context_excerpt(&original, &fitted, "stdout");
            assert_context_excerpt(&original, &fitted, "stderr");
        }
    }

    #[test]
    fn measured_context_requires_the_same_prefix() {
        let mut window = ContextWindow::default();
        let initial = vec![
            message("user", "Explore"),
            message("assistant", "Look around"),
        ];
        window.observe(&initial, &TOOLS, Some(100), 20, 0, "input_bytes");
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
    fn a_response_without_a_measurement_anchors_the_admitted_estimate() {
        let mut window = ContextWindow::default();
        let measured = vec![message("user", "Explore"), message("assistant", "Look")];
        window.observe(&measured, &TOOLS, Some(100), 20, 0, "input_bytes");
        window.calibrate(&measured, &TOOLS, Some(100));
        // A request extending the measured prefix keeps it when its response
        // brings no measurement: its tail stays counted in bytes.
        let mut extended = measured.clone();
        extended.push(message("tool", "Evidence"));
        let (admitted, basis) = window.estimate(&extended, &TOOLS);
        assert_eq!(basis, "server_usage_plus_new_bytes");
        let mut answered = extended.clone();
        answered.push(message("assistant", "Noted"));
        window.observe(&answered, &TOOLS, None, 7, admitted, basis);
        let mut next = answered.clone();
        next.push(message("tool", "More evidence"));
        assert_eq!(
            window.estimate(&next, &TOOLS),
            (
                120 + encoded_size(&json!(&next[2..])),
                "server_usage_plus_new_bytes"
            )
        );
        // A replacement matches no measured prefix and is estimated whole at
        // the calibrated rate. When its response brings no measurement, it
        // anchors that estimate, and later requests add their bytes to it.
        let replacement = vec![message("user", "Explore"), message("assistant", "Summary")];
        let (admitted, basis) = window.estimate(&replacement, &TOOLS);
        assert_eq!(basis, "calibrated_bytes");
        let mut answered = replacement.clone();
        answered.push(message("assistant", "Continuing"));
        window.observe(&answered, &TOOLS, None, 9, admitted, basis);
        let mut next = answered.clone();
        next.push(message("tool", "x".repeat(1716)));
        assert_eq!(
            window.estimate(&next, &TOOLS),
            (
                admitted + 9 + encoded_size(&json!(&next[3..])),
                "calibrated_prefix_plus_new_bytes"
            )
        );
        // The anchor moves forward with each unmeasured response, and a
        // measured one restores the server basis.
        let (admitted, basis) = window.estimate(&next, &TOOLS);
        let mut answered = next.clone();
        answered.push(message("assistant", "Still going"));
        window.observe(&answered, &TOOLS, None, 4, admitted, basis);
        let mut after = answered.clone();
        after.push(message("tool", "y".repeat(500)));
        assert_eq!(
            window.estimate(&after, &TOOLS),
            (
                admitted + 4 + encoded_size(&json!(&after[5..])),
                "calibrated_prefix_plus_new_bytes"
            )
        );
        let (admitted, basis) = window.estimate(&after, &TOOLS);
        let mut answered = after.clone();
        answered.push(message("assistant", "Measured"));
        window.observe(&answered, &TOOLS, Some(900), 3, admitted, basis);
        let mut last = answered.clone();
        last.push(message("tool", "z"));
        assert_eq!(
            window.estimate(&last, &TOOLS),
            (
                903 + encoded_size(&json!(&last[7..])),
                "server_usage_plus_new_bytes"
            )
        );
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
