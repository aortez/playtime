//! Presentation observations, not a second controller or a durable transcript.
//! Raw tool records remain in the session; this bounded cache serves /raw.
use super::markdown::{Markdown, clean};
use crate::runtime::Event;
use playtime_engine::EffectKind;
use ratatui::{
    style::{Color, Style},
    text::Line,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::VecDeque;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Observation {
    User {
        text: String,
    },
    Notice {
        text: String,
    },
    Phase {
        text: String,
    },
    Delta {
        text: String,
        #[serde(default)]
        thinking: bool,
    },
    Tool {
        name: String,
        arguments: Value,
    },
    ToolResult {
        value: Value,
    },
    Finished {
        text: String,
    },
}
impl From<&Event> for Observation {
    fn from(event: &Event) -> Self {
        match event {
            Event::Started { turn } => Self::Notice {
                text: format!("Turn {turn}"),
            },
            Event::Effect { effect } => match &effect.kind {
                EffectKind::Model(request) => Self::Phase {
                    text: format!(
                        "{:?} · input bound {} tokens · output cap {}",
                        request.purpose, request.context_input_bound, request.maximum_tokens
                    ),
                },
                EffectKind::Tool(request) => Self::Tool {
                    name: request.name.clone(),
                    arguments: request.arguments.clone(),
                },
                EffectKind::Cleanup => Self::Phase {
                    text: "Cleanup · stopping owned turn processes".into(),
                },
                EffectKind::StartWorkspace => Self::Phase {
                    text: "Preparing project tools".into(),
                },
                _ => Self::Notice {
                    text: "Controller activity".into(),
                },
            },
            Event::Chunk { chunk } => Self::Delta {
                text: chunk[if chunk.get("thinking").is_some() {
                    "thinking"
                } else {
                    "content"
                }]
                .as_str()
                .unwrap_or_default()
                .into(),
                thinking: chunk.get("thinking").is_some(),
            },
            Event::ToolResult { value } => Self::ToolResult {
                value: value.clone(),
            },
            Event::Stopped { result } => Self::Finished {
                text: format!(
                    "{:?} · {:.1}s · {} model calls · usage {}",
                    result.reason,
                    result.elapsed_ms as f64 / 1000.0,
                    result.model_calls,
                    serde_json::to_string(&result.usage).unwrap_or_else(|_| "unavailable".into())
                ),
            },
        }
    }
}
#[derive(Default)]
pub struct Presentation {
    markdown: Markdown,
    speaker: Option<bool>,
    pub phase: String,
    tools: VecDeque<(u64, String)>,
    next_tool: u64,
}
impl Presentation {
    pub fn preview(&self) -> Line<'static> {
        self.markdown.preview()
    }
    pub fn finish(&mut self) -> Vec<Line<'static>> {
        self.speaker = None;
        self.markdown.flush()
    }
    pub fn observe(&mut self, event: Observation) -> Vec<Line<'static>> {
        if let Observation::Delta { text, thinking } = event {
            let mut out = Vec::new();
            if self.speaker != Some(thinking) {
                out.extend(self.finish());
                out.push(label(
                    if thinking { "Reasoning" } else { "Assistant" },
                    Color::Green,
                ));
                self.speaker = Some(thinking);
            }
            out.extend(self.markdown.push(&text));
            return out;
        }
        let mut out = self.finish();
        match event {
            Observation::User { text } => {
                out.push(label("You", Color::Blue));
                out.extend(clean(&text).lines().map(|s| Line::raw(s.to_owned())));
            }
            Observation::Notice { text } => out.extend(
                clean(&text)
                    .lines()
                    .map(|s| Line::styled(s.to_owned(), Style::default().dim())),
            ),
            Observation::Phase { text } => {
                self.phase = clean(&text);
                out.push(label(&self.phase, Color::Yellow));
            }
            Observation::Tool { name, arguments } => {
                self.phase = format!("Tool {}", clean(&name));
                out.push(label(&self.phase, Color::Yellow));
                let raw = serde_json::to_string_pretty(&arguments).unwrap_or_default();
                out.extend(preview(
                    &raw,
                    6,
                    700,
                    "full arguments remain in session records",
                ));
            }
            Observation::ToolResult { value } => {
                self.next_tool += 1;
                let raw = serde_json::to_string_pretty(&value).unwrap_or_default();
                // Runtime results are bounded, but replay input is not trusted.
                if raw.len() <= 128 * 1024 {
                    self.tools.push_back((self.next_tool, raw));
                }
                while self.tools.len() > 16
                    || self.tools.iter().map(|(_, s)| s.len()).sum::<usize>() > 512 * 1024
                {
                    self.tools.pop_front();
                }
                out.push(label(
                    &format!("Tool result #{} · /raw {}", self.next_tool, self.next_tool),
                    Color::Yellow,
                ));
                if let Some(code) = value["exit_code"].as_i64() {
                    out.push(label(
                        &format!("exit {code}"),
                        if code == 0 { Color::Green } else { Color::Red },
                    ));
                }
                let mut any = false;
                for key in ["error", "stdout", "stderr", "content"] {
                    if let Some(text) = value[key].as_str().filter(|s| !s.is_empty()) {
                        any = true;
                        out.push(label(
                            key,
                            if key == "error" || key == "stderr" {
                                Color::Red
                            } else {
                                Color::Yellow
                            },
                        ));
                        out.extend(preview(text, 6, 700, "/raw for recorded result"));
                    }
                }
                if !any {
                    out.extend(preview(
                        &value.to_string(),
                        6,
                        700,
                        "/raw for recorded result",
                    ));
                }
                if value["output_truncated"] == true || value["truncated"] == true {
                    out.push(label(
                        "[tool output truncated upstream; /raw cannot recover omitted bytes]",
                        Color::Yellow,
                    ));
                }
            }
            Observation::Finished { text } => {
                self.phase = "Saving turn · waiting for runtime completion".into();
                out.push(label(&clean(&text), Color::Yellow));
            }
            Observation::Delta { .. } => unreachable!(),
        }
        out
    }
    pub fn raw(&self, id: Option<u64>) -> Vec<Line<'static>> {
        let selected = id.or_else(|| self.tools.back().map(|(id, _)| *id));
        match self.tools.iter().find(|(id, _)| Some(*id) == selected) {
            Some((id, text)) => std::iter::once(label(
                &format!("Raw tool result #{id} (terminal controls escaped)"),
                Color::Yellow,
            ))
            .chain(clean(text).lines().map(|s| Line::raw(s.to_owned())))
            .collect(),
            None => vec![Line::raw(
                "Tool result is not in the bounded preview cache. Full recorded evidence remains in the printed session directory.",
            )],
        }
    }
}
fn label(text: &str, color: Color) -> Line<'static> {
    Line::styled(text.to_owned(), Style::default().fg(color).bold())
}
fn preview(text: &str, max_lines: usize, max_bytes: usize, source: &str) -> Vec<Line<'static>> {
    let mut bytes = 0;
    let mut out = Vec::new();
    for line in text.split_inclusive('\n').take(max_lines) {
        let mut take = (max_bytes - bytes).min(line.len());
        while !line.is_char_boundary(take) {
            take -= 1;
        }
        out.push(Line::raw(clean(line[..take].trim_end_matches('\n'))));
        bytes += take;
        if take < line.len() || bytes == max_bytes {
            break;
        }
    }
    if bytes < text.len() {
        out.push(Line::styled(
            format!(
                "[{} bytes omitted from preview; {source}]",
                text.len() - bytes
            ),
            Style::default().fg(Color::Yellow),
        ));
    }
    out
}
