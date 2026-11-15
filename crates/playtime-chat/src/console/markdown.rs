//! Incremental, line-oriented Markdown for native scrollback. Only the unfinished
//! line is repainted; completed lines never depend on later model context.
use pulldown_cmark::{Event, Parser, Tag, TagEnd};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

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

#[derive(Default)]
pub struct Markdown {
    pending: String,
    fence: Option<(char, usize)>,
}
impl Markdown {
    pub fn push(&mut self, text: &str) -> Vec<Line<'static>> {
        self.pending.push_str(&clean(text));
        let mut lines = Vec::new();
        while let Some(end) = self.pending.find('\n') {
            let rest = self.pending.split_off(end + 1);
            let line = std::mem::replace(&mut self.pending, rest);
            lines.push(self.line(line.trim_end_matches('\n')));
        }
        // Bound repaint cost even for a model emitting one enormous line.
        while self.pending.len() > 4096 {
            let mut end = 4096;
            while !self.pending.is_char_boundary(end) {
                end -= 1;
            }
            let rest = self.pending.split_off(end);
            let line = std::mem::replace(&mut self.pending, rest);
            lines.push(Line::raw(line));
            lines.push(Line::styled(
                "[long line continues]",
                Style::default().dim(),
            ));
        }
        lines
    }
    pub fn preview(&self) -> Line<'static> {
        // An incomplete construct remains literal until its line completes.
        Line::raw(self.pending.clone())
    }
    pub fn flush(&mut self) -> Vec<Line<'static>> {
        let text = std::mem::take(&mut self.pending);
        let lines = if text.is_empty() {
            vec![]
        } else {
            vec![self.line(&text)]
        };
        self.fence = None;
        lines
    }
    fn line(&mut self, text: &str) -> Line<'static> {
        let trimmed = text.trim_start();
        let first = trimmed.chars().next().unwrap_or(' ');
        let count = trimmed.chars().take_while(|c| *c == first).count();
        if matches!(first, '`' | '~') && count >= 3 {
            match self.fence {
                None => self.fence = Some((first, count)),
                Some((marker, length))
                    if first == marker && count >= length && trimmed[count..].trim().is_empty() =>
                {
                    self.fence = None
                }
                _ => (),
            }
            return Line::styled(text.to_owned(), Style::default().fg(Color::Cyan));
        }
        if self.fence.is_some() {
            return Line::styled(text.replace('\t', "    "), Style::default().fg(Color::Cyan));
        }
        inline(text)
    }
}
fn inline(text: &str) -> Line<'static> {
    let mut spans = Vec::new();
    let mut styles = vec![Style::default()];
    let mut links = Vec::new();
    for event in Parser::new(text) {
        let style = *styles.last().unwrap();
        match event {
            Event::Start(tag) => {
                let next = match tag {
                    Tag::Strong | Tag::Heading { .. } => style.add_modifier(Modifier::BOLD),
                    Tag::Emphasis => style.add_modifier(Modifier::ITALIC),
                    Tag::Link { dest_url, .. } => {
                        links.push(dest_url.to_string());
                        style.fg(Color::Blue).underlined()
                    }
                    Tag::Item => {
                        let marker = text
                            .trim_start()
                            .split_once(' ')
                            .map(|(s, _)| s)
                            .unwrap_or("-");
                        spans.push(Span::raw(format!("{marker} ")));
                        style
                    }
                    Tag::BlockQuote(_) => {
                        spans.push(Span::raw("│ "));
                        style
                    }
                    Tag::CodeBlock(_) => style.fg(Color::Cyan),
                    _ => style,
                };
                styles.push(next);
            }
            Event::End(tag) => {
                styles.pop();
                if tag == TagEnd::Link
                    && let Some(url) = links.pop()
                {
                    spans.push(Span::styled(
                        format!(" ({url})"),
                        Style::default().fg(Color::Blue),
                    ));
                }
            }
            Event::Text(s) | Event::Html(s) | Event::InlineHtml(s) => {
                spans.push(Span::styled(s.into_string(), style))
            }
            Event::Code(s) => spans.push(Span::styled(s.into_string(), style.fg(Color::Cyan))),
            Event::SoftBreak | Event::HardBreak => spans.push(Span::raw(" ")),
            Event::Rule => spans.push(Span::raw("────────────────")),
            _ => (),
        }
    }
    // Markdown entities can decode controls after the input was sanitized.
    // Escape at the final text boundary too, including decoded link targets.
    for span in &mut spans {
        span.content = clean(&span.content).into();
    }
    if spans.is_empty() {
        Line::raw(text.to_owned())
    } else {
        Line::from(spans)
    }
}
