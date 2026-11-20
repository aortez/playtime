//! Terminal-only response buffering. Transport, journals and JSON events continue
//! streaming; the next effect is the presentation boundary for a model response.
use crate::{Result, runtime::Event};
use playtime_engine::EffectKind;
use serde_json::Value;
use std::{collections::VecDeque, io::Write, time::Instant};
use unicode_width::UnicodeWidthChar;

const MAX_BUFFER_BYTES: usize = 32 * 1024 * 1024;
const PREVIEW_CHARS: usize = 160;
const RECENT_TOOLS: usize = 32;
const MAX_TOOL_VIEW_BYTES: usize = 256 * 1024;

pub fn clean(text: &str) -> String {
    text.chars()
        .flat_map(|c| {
            if c.is_control() && !matches!(c, '\n' | '\t') {
                c.escape_default().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect()
}

struct Segment {
    thinking: bool,
    text: String,
}
struct Response {
    segments: Vec<Segment>,
    started: Instant,
    received: usize,
    stored: usize,
    preview: String,
    preview_done: bool,
    interrupted: bool,
    clipped: bool,
}
impl Response {
    fn new() -> Self {
        Self {
            segments: vec![],
            started: Instant::now(),
            received: 0,
            stored: 0,
            preview: String::new(),
            preview_done: false,
            interrupted: false,
            clipped: false,
        }
    }
    fn append(&mut self, thinking: bool, text: &str) {
        if text.is_empty() {
            return;
        }
        self.received = self.received.saturating_add(text.len());
        if self.segments.last().is_none_or(|s| s.thinking != thinking) {
            self.segments.push(Segment {
                thinking,
                text: String::new(),
            });
            self.preview.clear();
            self.preview_done = false;
        }
        if !self.preview_done {
            for c in text.chars() {
                if c == '\n' || c == '\r' {
                    if self.preview.trim().is_empty() {
                        self.preview.clear();
                        continue;
                    }
                    self.preview_done = true;
                    break;
                }
                if self.preview.chars().count() >= PREVIEW_CHARS {
                    self.preview_done = true;
                    break;
                }
                if c == '\t' {
                    self.preview.push(' ');
                } else if c.is_control() {
                    self.preview.extend(c.escape_default());
                } else {
                    self.preview.push(c);
                }
            }
        }
        let mut end = text.len().min(MAX_BUFFER_BYTES.saturating_sub(self.stored));
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        self.segments
            .last_mut()
            .unwrap()
            .text
            .push_str(&text[..end]);
        self.stored += end;
        self.clipped |= end < text.len();
    }
    fn activity(&self, frame: usize, columns: usize) -> String {
        let phase = if self.interrupted {
            "stopping"
        } else if self.segments.last().is_some_and(|s| s.thinking) {
            "thinking"
        } else {
            "response"
        };
        let spinner = ['|', '/', '-', '\\'][frame % 4];
        let elapsed = self.started.elapsed();
        // Text bytes include thinking and answer deltas, not token counts or
        // transport framing. Include the initial wait in this request average.
        let kib = self.received as f64 / 1024.0;
        let rate = if elapsed.is_zero() {
            0.0
        } else {
            kib / elapsed.as_secs_f64()
        };
        let text = format!(
            "[{phase} {spinner} {}s · {kib:.1} KiB · {rate:.1} KiB/s avg] {}",
            elapsed.as_secs(),
            if self.preview.is_empty() {
                "Waiting for response…"
            } else {
                self.preview.trim()
            }
        );
        fit_row(&text, columns.saturating_sub(1))
    }
}

fn fit_row(text: &str, columns: usize) -> String {
    let mut used = 0;
    let mut row = String::new();
    for c in text.chars() {
        let width = c.width().unwrap_or(0);
        if used + width > columns {
            break;
        }
        row.push(c);
        used += width;
    }
    row
}

struct PendingTool {
    number: usize,
    label: String,
    started: Instant,
}

struct ToolRecord {
    number: usize,
    label: String,
    summary: String,
    value: Option<Value>,
}

fn short(text: &str, chars: usize) -> String {
    let escaped = clean(text);
    let mut result: String = escaped.chars().take(chars).collect();
    if escaped.chars().count() > chars {
        result.push('…');
    }
    result.replace(['\n', '\t'], " ")
}

fn tool_label(name: &str, arguments: &Value) -> String {
    let detail = arguments["command"]
        .as_str()
        .or_else(|| arguments["path"].as_str())
        .or_else(|| arguments["process_id"].as_str());
    match detail {
        Some(detail) => format!("{}: {}", clean(name), short(detail, 120)),
        None => clean(name),
    }
}

fn tool_summary(value: &Value, elapsed: std::time::Duration) -> String {
    let seconds = value["duration_seconds"]
        .as_f64()
        .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
        .unwrap_or(elapsed.as_secs_f64());
    let mut parts = vec![if seconds < 1.0 {
        format!("{}ms", (seconds * 1000.0).round() as u64)
    } else {
        format!("{seconds:.1}s")
    }];
    if let Some(code) = value["exit_code"].as_i64() {
        parts.push(format!("exit {code}"));
    } else if let Some(running) = value["running"].as_bool() {
        parts.push(if running { "running" } else { "stopped" }.into());
    }
    if let Some(bytes) = value["bytes_written"].as_u64() {
        parts.push(format!("wrote {bytes} B"));
    }
    for key in ["stdout", "stderr", "content"] {
        if let Some(output) = value[key].as_str() {
            parts.push(format!("{key} {} B", output.len()));
        }
    }
    if value["timed_out"] == true {
        parts.push("TIMED OUT".into());
    }
    if value["output_truncated"] == true || value["truncated"] == true {
        parts.push("TRUNCATED".into());
    }
    if let Some(error) = value["error"].as_str() {
        parts.push(format!("ERROR: {}", short(error, 120)));
    }
    parts.join(" · ")
}

fn tool_preview(value: &Value) -> Option<String> {
    for key in ["stderr", "stdout", "content"] {
        if let Some(line) = value[key]
            .as_str()
            .and_then(|text| text.lines().find(|line| !line.trim().is_empty()))
        {
            return Some(format!("{key}: {}", short(line, 120)));
        }
    }
    None
}

pub struct Output<W: Write, D: Write> {
    out: W,
    diagnostics: D,
    json: bool,
    chunked: bool,
    response: Option<Response>,
    row_visible: bool,
    frame: usize,
    stream_thinking: bool,
    next_tool: usize,
    pending_tool: Option<PendingTool>,
    recent_tools: VecDeque<ToolRecord>,
}
impl<W: Write, D: Write> Output<W, D> {
    pub fn new(out: W, diagnostics: D, json: bool, chunked: bool) -> Self {
        Self {
            out,
            diagnostics,
            json,
            chunked: chunked && !json,
            response: None,
            row_visible: false,
            frame: 0,
            stream_thinking: false,
            next_tool: 1,
            pending_tool: None,
            recent_tools: VecDeque::new(),
        }
    }
    /// Inspect results observed in this live CLI. Durable session records remain
    /// the source for older turns and any result beyond the bounded view.
    pub fn inspect_tool(&mut self, number: Option<usize>) -> Result<()> {
        match number {
            None => {
                if self.recent_tools.is_empty() {
                    writeln!(self.out, "No completed tool results in this CLI yet.")?;
                }
                for record in &self.recent_tools {
                    writeln!(
                        self.out,
                        "[tool {} {} · {}]",
                        record.number, record.label, record.summary
                    )?;
                }
                if let Some(first) = self.recent_tools.front() {
                    writeln!(
                        self.out,
                        "Inspect with /tool N (retained from {}).",
                        first.number
                    )?;
                }
            }
            Some(number) => match self.recent_tools.iter().find(|item| item.number == number) {
                Some(record) => {
                    writeln!(
                        self.out,
                        "[tool {} {} · {}]",
                        number, record.label, record.summary
                    )?;
                    if let Some(value) = &record.value {
                        let mut metadata = value.clone();
                        for key in ["stdout", "stderr", "content"] {
                            if let Some(output) =
                                metadata.as_object_mut().and_then(|map| map.remove(key))
                                && let Some(output) = output.as_str()
                            {
                                writeln!(self.out, "[{key}]")?;
                                write!(self.out, "{}", clean(output))?;
                                if !output.ends_with('\n') {
                                    writeln!(self.out)?;
                                }
                            }
                        }
                        writeln!(
                            self.out,
                            "[result] {}",
                            clean(&serde_json::to_string(&metadata)?)
                        )?;
                    } else {
                        writeln!(
                            self.out,
                            "Result exceeds the 256 KiB CLI view; inspect the session records."
                        )?;
                    }
                }
                None => writeln!(
                    self.out,
                    "Tool {number} is not in the recent results. Use /tool to list them; older results remain in the session records."
                )?,
            },
        }
        self.out.flush()?;
        Ok(())
    }
    pub fn tick(&mut self, columns: usize) -> Result<()> {
        if let Some(response) = &self.response {
            write!(
                self.diagnostics,
                "\r\x1b[2K{}",
                response.activity(self.frame, columns)
            )?;
            self.diagnostics.flush()?;
            self.row_visible = true;
            self.frame = self.frame.wrapping_add(1);
        }
        Ok(())
    }
    fn clear_row(&mut self) -> Result<()> {
        if self.row_visible {
            write!(self.diagnostics, "\r\x1b[2K")?;
            self.diagnostics.flush()?;
            self.row_visible = false;
        }
        Ok(())
    }
    pub fn notice(&mut self, text: &str, cancelling: bool) -> Result<()> {
        if cancelling && let Some(response) = &mut self.response {
            response.interrupted = true;
        }
        self.clear_row()?;
        writeln!(self.diagnostics, "{}", clean(text))?;
        self.diagnostics.flush()?;
        Ok(())
    }
    pub fn finish(&mut self, interrupted: bool) -> Result<()> {
        self.clear_row()?;
        if let Some(response) = self.response.take() {
            if (interrupted || response.interrupted) && !response.segments.is_empty() {
                writeln!(self.out, "[partial response — interrupted]")?;
            }
            for segment in response.segments {
                if segment.text.is_empty() {
                    continue;
                }
                if segment.thinking {
                    write!(self.out, "\n[thinking] ")?;
                } else {
                    writeln!(self.out)?;
                }
                writeln!(self.out, "{}", clean(&segment.text))?;
            }
            if response.clipped {
                writeln!(
                    self.out,
                    "[display exceeds 32 MiB; remaining text is in the session records]"
                )?;
            }
            self.out.flush()?;
        }
        Ok(())
    }
    pub fn render(&mut self, event: &Event) -> Result<()> {
        if self.json {
            writeln!(
                self.out,
                "{}",
                serde_json::to_string(&serde_json::json!({"schema_version":1,"event":event}))?
            )?;
            self.out.flush()?;
            return Ok(());
        }
        match event {
            Event::Started { turn } => writeln!(
                self.diagnostics,
                "[turn {}]",
                clean(&turn.chars().take(17).collect::<String>())
            )?,
            Event::Effect { effect } => {
                // No later effect is exposed until this response has settled.
                // Flush before tool activity, retries, compaction or cleanup.
                self.finish(false)?;
                if let Some(pending) = self.pending_tool.take() {
                    writeln!(
                        self.diagnostics,
                        "[tool {} result unavailable; inspect the session record]",
                        pending.number
                    )?;
                }
                match &effect.kind {
                    EffectKind::Model(request) => {
                        self.stream_thinking = false;
                        writeln!(
                            self.diagnostics,
                            "\n[{:?}; input bound {} tokens; output cap {}]",
                            request.purpose, request.context_input_bound, request.maximum_tokens
                        )?;
                        if self.chunked {
                            self.response = Some(Response::new());
                        }
                    }
                    EffectKind::Tool(request) => {
                        let number = self.next_tool;
                        self.next_tool = self.next_tool.saturating_add(1);
                        let label = tool_label(&request.name, &request.arguments);
                        writeln!(self.diagnostics, "\n[tool {number} {label}]")?;
                        self.pending_tool = Some(PendingTool {
                            number,
                            label,
                            started: Instant::now(),
                        });
                    }
                    EffectKind::Cleanup => {
                        writeln!(self.diagnostics, "\n[stopping turn processes]")?
                    }
                    _ => (),
                }
            }
            Event::Chunk { chunk } => {
                for (field, thinking) in [("thinking", true), ("content", false)] {
                    if let Some(text) = chunk[field].as_str() {
                        if self.chunked {
                            self.response
                                .get_or_insert_with(Response::new)
                                .append(thinking, text);
                        } else {
                            if self.stream_thinking != thinking {
                                write!(
                                    self.out,
                                    "\n{}",
                                    if thinking { "[thinking] " } else { "" }
                                )?;
                                self.stream_thinking = thinking;
                            }
                            write!(self.out, "{}", clean(text))?;
                            self.out.flush()?;
                        }
                    }
                }
            }
            Event::ToolResult { value } => {
                let pending = self.pending_tool.take();
                let (number, label, elapsed) = if let Some(pending) = pending {
                    (pending.number, pending.label, pending.started.elapsed())
                } else {
                    let number = self.next_tool;
                    self.next_tool = self.next_tool.saturating_add(1);
                    (number, "unmatched result".into(), std::time::Duration::ZERO)
                };
                let view_available = serde_json::to_vec(value)?.len() <= MAX_TOOL_VIEW_BYTES;
                let mut summary = tool_summary(value, elapsed);
                if view_available {
                    writeln!(
                        self.diagnostics,
                        "[tool {number} {summary} · /tool {number}]"
                    )?;
                } else {
                    summary.push_str(" · CLI view unavailable; see session records");
                    writeln!(self.diagnostics, "[tool {number} {summary}]")?;
                }
                if let Some(preview) = tool_preview(value) {
                    writeln!(self.diagnostics, "  {}", preview)?;
                }
                self.recent_tools.push_back(ToolRecord {
                    number,
                    label,
                    summary,
                    value: view_available.then(|| value.clone()),
                });
                if self.recent_tools.len() > RECENT_TOOLS {
                    self.recent_tools.pop_front();
                }
            }
            Event::Stopped { result } => {
                self.finish(result.reason != playtime_core::StopReason::AgentFinished)?;
                if let Some(pending) = self.pending_tool.take() {
                    writeln!(
                        self.diagnostics,
                        "[tool {} result unavailable; inspect the session record]",
                        pending.number
                    )?;
                }
                writeln!(
                    self.diagnostics,
                    "\n[{:?}; {:.1}s; {} model calls; usage {}]",
                    result.reason,
                    result.elapsed_ms as f64 / 1000.0,
                    result.model_calls,
                    serde_json::to_string(&result.usage)?
                )?;
            }
        }
        self.diagnostics.flush()?;
        Ok(())
    }
}
impl<W: Write, D: Write> Drop for Output<W, D> {
    fn drop(&mut self) {
        // Preserve received partial text and restore the cursor on driver errors.
        let _ = self.finish(true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use playtime_engine::{Effect, ToolRequest};
    use serde_json::json;
    fn chunk(text: &str) -> Event {
        Event::Chunk {
            chunk: json!({"content":text}),
        }
    }
    fn tool() -> Event {
        Event::Effect {
            effect: Effect {
                id: 1,
                deadline_ms: 1000,
                kind: EffectKind::Tool(ToolRequest {
                    name: "exec".into(),
                    arguments: json!({"command":"pwd"}),
                }),
            },
        }
    }
    #[test]
    fn response_waits_for_boundary_and_activity_never_scrolls() {
        let mut output = Output::new(vec![], vec![], false, true);
        output.render(&chunk("First line\n")).unwrap();
        output.tick(80).unwrap();
        output.render(&chunk("second line\nthird line")).unwrap();
        output.tick(80).unwrap();
        assert!(output.out.is_empty());
        let activity = String::from_utf8(output.diagnostics.clone()).unwrap();
        assert!(!activity.contains('\n'));
        assert!(!activity.contains("second line"));
        assert!(activity.contains("First line"));
        output.render(&tool()).unwrap();
        assert_eq!(
            String::from_utf8(output.out.clone()).unwrap(),
            "\nFirst line\nsecond line\nthird line\n"
        );
        let before = output.out.clone();
        output.finish(false).unwrap();
        assert_eq!(output.out, before);
    }
    #[test]
    fn cancellation_keeps_partial_thinking_and_answer_and_clears_activity() {
        let mut output = Output::new(vec![], vec![], false, true);
        output
            .render(&Event::Chunk {
                chunk: json!({"thinking":"Plan\ncarefully", "content":"Answer so far"}),
            })
            .unwrap();
        output.tick(80).unwrap();
        output.notice("Stopping...", true).unwrap();
        output.finish(false).unwrap();
        let text = String::from_utf8(output.out.clone()).unwrap();
        assert!(text.contains("partial response — interrupted"));
        assert!(text.find("Plan\ncarefully").unwrap() < text.find("Answer so far").unwrap());
        assert!(!output.row_visible);
    }
    #[test]
    fn preview_skips_leading_blank_lines_but_keeps_the_original_response() {
        let mut output = Output::new(vec![], vec![], false, true);
        output.render(&chunk("\n \nFirst ")).unwrap();
        output.render(&chunk("line\nSecond line")).unwrap();
        output.tick(80).unwrap();
        let activity = String::from_utf8(output.diagnostics.clone()).unwrap();
        assert!(activity.contains("First line"));
        assert!(!activity.contains("Second line"));
        output.finish(false).unwrap();
        assert_eq!(
            String::from_utf8(output.out.clone()).unwrap(),
            "\n\n \nFirst line\nSecond line\n"
        );
    }
    #[test]
    fn preview_fits_narrow_terminals_and_escapes_control_characters() {
        let mut response = Response::new();
        response.append(false, "界界\x1b[31m\tmore\nnot in preview");
        for columns in [1, 12, 30, 80] {
            let row = response.activity(0, columns);
            assert!(row.chars().map(|c| c.width().unwrap_or(0)).sum::<usize>() < columns);
            assert!(!row.contains(['\x1b', '\t', '\n']));
        }
        assert!(!response.activity(0, 80).contains("not in preview"));
    }
    #[test]
    fn pipes_stream_and_json_remains_unbuffered_without_cursor_sequences() {
        for json in [false, true] {
            let mut output = Output::new(vec![], vec![], json, false);
            let event = chunk("hello");
            output.render(&event).unwrap();
            output.tick(80).unwrap();
            let text = String::from_utf8(output.out.clone()).unwrap();
            if json {
                assert_eq!(
                    serde_json::from_str::<serde_json::Value>(&text).unwrap(),
                    json!({"schema_version":1,"event":event})
                );
            } else {
                assert_eq!(text, "hello");
            }
            assert!(output.diagnostics.is_empty());
        }
    }

    #[test]
    fn tool_results_are_summarized_and_inspectable_across_turns() {
        let mut output = Output::new(vec![], vec![], false, true);
        output.render(&tool()).unwrap();
        output
            .render(&Event::ToolResult {
                value: json!({"stdout":"first\nsecond\n", "stderr":"warning\u{1b}[31m\n",
                    "exit_code":7,"output_truncated":true,"timed_out":false,
                    "duration_seconds":0.25}),
            })
            .unwrap();
        let live = String::from_utf8(output.diagnostics.clone()).unwrap();
        assert!(live.contains("tool 1 250ms · exit 7"));
        assert!(live.contains("TRUNCATED · /tool 1"));
        assert!(live.contains("stderr: warning\\u{1b}[31m"));
        assert!(!live.contains("second"));
        output
            .render(&Event::Started {
                turn: "next".into(),
            })
            .unwrap();
        output.inspect_tool(Some(1)).unwrap();
        let detail = String::from_utf8(output.out.clone()).unwrap();
        assert!(detail.contains("[stdout]\nfirst\nsecond\n"));
        assert!(detail.contains("[stderr]\nwarning\\u{1b}[31m\n"));
        assert!(detail.contains("\"exit_code\":7"));
        assert!(!detail.contains('\u{1b}'));
    }

    #[test]
    fn tool_view_is_bounded_and_marks_unobserved_results() {
        let mut output = Output::new(vec![], vec![], false, false);
        for number in 1..=RECENT_TOOLS + 1 {
            output.render(&tool()).unwrap();
            output
                .render(&Event::ToolResult {
                    value: json!({"path":format!("/tmp/{number}"),"bytes_written":number}),
                })
                .unwrap();
        }
        output.inspect_tool(None).unwrap();
        output.inspect_tool(Some(1)).unwrap();
        let list = String::from_utf8(output.out.clone()).unwrap();
        assert!(!list.contains("[tool 1 "));
        assert!(list.contains("[tool 2 "));
        assert!(list.contains("retained from 2"));
        assert!(list.contains("Tool 1 is not in the recent results"));
        output.render(&tool()).unwrap();
        output.render(&tool()).unwrap();
        assert!(
            String::from_utf8(output.diagnostics.clone())
                .unwrap()
                .contains("result unavailable")
        );
    }

    #[test]
    fn oversized_result_does_not_advertise_unavailable_cli_detail() {
        let mut output = Output::new(vec![], vec![], false, true);
        output.render(&tool()).unwrap();
        output
            .render(&Event::ToolResult {
                value: json!({"stdout":"x".repeat(MAX_TOOL_VIEW_BYTES)}),
            })
            .unwrap();
        let live = String::from_utf8(output.diagnostics.clone()).unwrap();
        assert!(live.contains("CLI view unavailable; see session records"));
        assert!(!live.contains("/tool 1"));
        output.inspect_tool(Some(1)).unwrap();
        assert!(
            String::from_utf8(output.out.clone())
                .unwrap()
                .contains("Result exceeds the 256 KiB CLI view")
        );
    }
}
