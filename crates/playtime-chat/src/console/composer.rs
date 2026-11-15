use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    style::Style,
    widgets::{Block, Borders},
};
use ratatui_textarea::{CursorMove, TextArea, WrapMode};
use std::collections::VecDeque;
use unicode_segmentation::UnicodeSegmentation;

pub const MAX_DRAFT_BYTES: usize = 64 * 1024;
pub struct Composer {
    pub editor: TextArea<'static>,
    history: VecDeque<String>,
    history_index: Option<usize>,
    saved_draft: String,
}
impl Default for Composer {
    fn default() -> Self {
        Self {
            editor: editor(),
            history: VecDeque::new(),
            history_index: None,
            saved_draft: String::new(),
        }
    }
}
fn editor() -> TextArea<'static> {
    let mut editor = TextArea::default();
    editor.set_wrap_mode(WrapMode::Glyph);
    editor.set_cursor_line_style(Style::default());
    editor.set_max_histories(64);
    editor.set_block(
        Block::default()
            .borders(Borders::TOP)
            .title(" You · draft "),
    );
    editor
}
impl Composer {
    pub fn text(&self) -> String {
        self.editor.lines().join("\n")
    }
    fn set(&mut self, text: &str) {
        self.editor = editor();
        self.editor.insert_str(text);
    }
    pub fn paste(&mut self, text: &str) -> bool {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        // Paste is always data, including /quit and control bytes. Never interpret
        // terminal escapes in a user draft or silently submit pasted newlines.
        let text = super::markdown::clean(&text);
        if self.text().len() + text.len() > MAX_DRAFT_BYTES {
            return false;
        }
        self.editor.insert_str(text);
        true
    }
    pub fn take(&mut self) -> String {
        let text = self.text();
        if self.history.back() != Some(&text) {
            self.history.push_back(text.clone());
        }
        while self.history.len() > 50
            || self.history.iter().map(String::len).sum::<usize>() > 512 * 1024
        {
            self.history.pop_front();
        }
        self.history_index = None;
        self.saved_draft.clear();
        self.editor = editor();
        text
    }
    pub fn history(&mut self, older: bool) {
        if self.history.is_empty() {
            return;
        }
        let index = match (self.history_index, older) {
            (None, true) => {
                self.saved_draft = self.text();
                Some(self.history.len() - 1)
            }
            (Some(i), true) => Some(i.saturating_sub(1)),
            (Some(i), false) if i + 1 < self.history.len() => Some(i + 1),
            _ => None,
        };
        let text = index
            .map(|i| self.history[i].clone())
            .unwrap_or_else(|| self.saved_draft.clone());
        if self.history_index.is_some() || index.is_some() {
            self.set(&text);
        }
        self.history_index = index;
    }
    pub fn key(&mut self, key: KeyEvent) -> bool {
        if key.kind == KeyEventKind::Release {
            return true;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Up if alt => self.history(true),
            KeyCode::Down if alt => self.history(false),
            KeyCode::Char('z') if ctrl => {
                self.editor.undo();
            }
            KeyCode::Char('y') if ctrl => {
                self.editor.redo();
            }
            KeyCode::Enter | KeyCode::Char('j') if key.code == KeyCode::Enter || ctrl => {
                return self.paste("\n");
            }
            KeyCode::Char(c) if !ctrl && !alt => return self.paste(&c.to_string()),
            KeyCode::Tab => return self.paste("    "),
            KeyCode::Backspace if !ctrl && !alt && self.editor.selection_range().is_none() => {
                self.editor.start_selection();
                self.grapheme_move(false);
                self.editor.delete_char();
            }
            KeyCode::Left | KeyCode::Right if key.modifiers.is_empty() => {
                self.editor.cancel_selection();
                self.grapheme_move(key.code == KeyCode::Right)
            }
            KeyCode::Delete
                if key.modifiers.is_empty() && self.editor.selection_range().is_none() =>
            {
                let before = self.editor.cursor();
                self.editor.start_selection();
                self.grapheme_move(true);
                if self.editor.cursor() != before {
                    self.editor.delete_char();
                } else {
                    // An empty selection falls back to backspace in delete_char.
                    self.editor.cancel_selection();
                }
            }
            _ => {
                self.editor.input(key);
            }
        }
        if self.text().len() > MAX_DRAFT_BYTES {
            self.editor.undo();
            return false;
        }
        true
    }
    fn grapheme_move(&mut self, forward: bool) {
        let ratatui_textarea::DataCursor(row, col) = self.editor.cursor();
        let line = &self.editor.lines()[row];
        let part: String = if forward {
            line.chars().skip(col).collect()
        } else {
            line.chars().take(col).collect()
        };
        let mut graphemes = part.graphemes(true);
        let count = if forward {
            graphemes.next()
        } else {
            graphemes.next_back()
        }
        .map_or(1, |s| s.chars().count());
        for _ in 0..count {
            self.editor.move_cursor(if forward {
                CursorMove::Forward
            } else {
                CursorMove::Back
            });
        }
    }
}
