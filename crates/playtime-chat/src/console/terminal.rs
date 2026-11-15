use super::{Action, Active, Command, Composer, HELP, Observation, Presentation, cancel};
use crossterm::{
    cursor::{MoveTo, MoveToColumn, Show},
    event::{
        DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEventKind, KeyModifiers,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    TerminalOptions, Viewport,
    backend::CrosstermBackend,
    layout::Rect,
    style::{Color, Style},
    text::Line,
    widgets::{Paragraph, Widget, Wrap},
};
use std::{
    io::{self, Write},
    sync::mpsc,
    time::{Duration, Instant},
};
use tokio::sync::mpsc as async_mpsc;

struct Restore;
impl Drop for Restore {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), DisableBracketedPaste, Show, MoveToColumn(0));
    }
}
pub(super) struct Terminal {
    terminal: ratatui::Terminal<CrosstermBackend<io::Stdout>>,
    _restore: Restore,
    composer: Composer,
    view: Presentation,
    busy: bool,
    ready_since: Instant,
    started: Option<Instant>,
    hint: String,
    artifact_browser: bool,
}
impl Terminal {
    pub fn new(artifact_browser: bool) -> io::Result<Self> {
        enable_raw_mode()?;
        let restore = Restore;
        execute!(io::stdout(), EnableBracketedPaste)?;
        let terminal = ratatui::Terminal::with_options(
            CrosstermBackend::new(io::stdout()),
            TerminalOptions {
                viewport: Viewport::Inline(9),
            },
        )?;
        Ok(Self {
            terminal,
            _restore: restore,
            composer: Composer::default(),
            view: Presentation::default(),
            busy: false,
            ready_since: Instant::now(),
            started: None,
            hint: String::new(),
            artifact_browser,
        })
    }
    fn print(&mut self, lines: Vec<Line<'static>>) -> io::Result<()> {
        self.terminal.autoresize()?;
        let width = self.terminal.size()?.width.max(1);
        // Each insertion is bounded by a source line, never the whole transcript.
        for line in lines {
            let paragraph = Paragraph::new(line).wrap(Wrap { trim: false });
            let count = paragraph.line_count(width).max(1).min(u16::MAX as usize) as u16;
            self.terminal
                .insert_before(count, |buf| paragraph.render(buf.area, buf))?;
        }
        Ok(())
    }
    fn observe(&mut self, event: Observation) -> io::Result<()> {
        let lines = self.view.observe(event);
        self.print(lines)
    }
    fn draw(&mut self) -> io::Result<()> {
        let preview = self.view.preview();
        let status = if self.busy {
            format!(
                "{} · {:.0}s · usage pending",
                self.view.phase,
                self.started.map_or(0.0, |s| s.elapsed().as_secs_f64())
            )
        } else if self.artifact_browser {
            "Ready · Enter to run command · /artifacts".into()
        } else {
            "Ready · Enter to send · /help".into()
        };
        let hint = if !self.hint.is_empty() {
            self.hint.clone()
        } else if self.busy {
            "Draft stays here · Enter does not queue · Ctrl-C stops".into()
        } else {
            "Ctrl-J newline · Alt-↑/↓ history · Ctrl-Z/Y undo/redo".into()
        };
        let editor = &self.composer.editor;
        self.terminal.draw(|frame| {
            let area = frame.area();
            let preview_height = if preview.width() == 0 {
                0
            } else {
                3.min(area.height.saturating_sub(4))
            };
            let status_y = area.y + preview_height;
            if preview_height > 0 {
                let preview = Paragraph::new(preview).wrap(Wrap { trim: false });
                let rows = preview.line_count(area.width.max(1));
                frame.render_widget(
                    preview.scroll((
                        rows.saturating_sub(preview_height as usize)
                            .min(u16::MAX as usize) as u16,
                        0,
                    )),
                    Rect::new(area.x, area.y, area.width, preview_height),
                );
            }
            frame.render_widget(
                Paragraph::new(status).style(Style::default().fg(Color::Yellow)),
                Rect::new(area.x, status_y, area.width, 1.min(area.height)),
            );
            let editor_y = status_y.saturating_add(1);
            let editor_height = area.bottom().saturating_sub(editor_y).saturating_sub(1);
            frame.render_widget(
                editor,
                Rect::new(area.x, editor_y, area.width, editor_height),
            );
            if area.height > 1 {
                frame.render_widget(
                    Paragraph::new(hint).style(Style::default().dim()),
                    Rect::new(area.x, area.bottom() - 1, area.width, 1),
                );
            }
        })?;
        Ok(())
    }
    pub fn run(
        &mut self,
        rx: mpsc::Receiver<Command>,
        actions: async_mpsc::Sender<Action>,
        active: &Active,
        banner: String,
    ) -> io::Result<()> {
        self.observe(Observation::Notice { text: banner })?;
        self.observe(Observation::Notice {
            text: "Inline composer · /help for keys. No automatic follow-up queue.".into(),
        })?;
        let mut dirty = true;
        let mut last_draw = Instant::now() - Duration::from_secs(1);
        loop {
            if dirty && last_draw.elapsed() >= Duration::from_millis(33) {
                self.draw()?;
                dirty = false;
                last_draw = Instant::now();
            }
            match rx.recv_timeout(Duration::from_millis(30)) {
                Ok(Command::Shutdown) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if self.busy && last_draw.elapsed() >= Duration::from_secs(1) {
                        dirty = true;
                    }
                    continue;
                }
                Ok(Command::Observation(event)) => self.observe(event)?,
                Ok(Command::Active(value)) => {
                    self.busy = value;
                    self.started = value.then(Instant::now);
                    if !value {
                        // Input read while output was blocked must not turn into
                        // a submission when a queued completion reaches us first.
                        self.ready_since = Instant::now();
                        self.view.phase = "Ready".into();
                        self.hint.clear();
                    }
                }
                Ok(Command::Input(event, received)) => {
                    self.input(event, received, &actions, active)?
                }
            }
            dirty = true;
        }
        let lines = self.view.finish();
        self.print(lines)?;
        if !self.composer.text().is_empty() {
            self.observe(Observation::Notice {
                text: format!("Unsent draft (not submitted):\n{}", self.composer.text()),
            })?;
        }
        let origin = self.terminal.get_frame().area().y;
        self.terminal.clear()?;
        execute!(io::stdout(), MoveTo(0, origin))?;
        io::stdout().flush()?;
        Ok(())
    }
    fn input(
        &mut self,
        event: Event,
        received: Instant,
        actions: &async_mpsc::Sender<Action>,
        active: &Active,
    ) -> io::Result<()> {
        match event {
            Event::Paste(text) => {
                self.hint = if self.composer.paste(&text) {
                    String::new()
                } else {
                    "Paste refused: draft limit is 64 KiB".into()
                };
            }
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                match key.code {
                    KeyCode::Char('c') if ctrl => {
                        if self.busy {
                            cancel(active);
                            self.hint = "Stopping; waiting for owned cleanup…".into();
                            send(actions, Action::Stop)?;
                        } else {
                            send(actions, Action::Quit)?;
                        }
                    }
                    KeyCode::Char('d') if ctrl && self.composer.text().is_empty() => {
                        cancel(active);
                        send(actions, Action::Quit)?;
                    }
                    KeyCode::Enter if key.modifiers.is_empty() => {
                        let text = self.composer.text();
                        match text.trim() {
                            "/help" => { self.composer.take(); self.observe(Observation::Notice { text: HELP.into() })?; }
                            "/stop" => { self.composer.take(); cancel(active); send(actions, Action::Stop)?; }
                            "/quit" | "/exit" => { self.composer.take(); cancel(active); send(actions, Action::Quit)?; }
                            command if command == "/raw" || command.starts_with("/raw ") => {
                                let id = command.strip_prefix("/raw ").map(str::parse::<u64>).transpose();
                                match id {
                                    Ok(id) => { self.composer.take(); self.print(self.view.raw(id))?; }
                                    Err(_) => self.hint = "Use /raw or /raw NUMBER".into(),
                                }
                            }
                            "" => (),
                            command if command == "/artifacts" || command == "/artifact" || command.starts_with("/artifact ") => {
                                if self.busy || received < self.ready_since {
                                    self.hint = "Wait for completion before artifact operations; command kept.".into();
                                } else {
                                    match crate::artifacts::parse_action(command) {
                                        Ok(command) => { self.composer.take(); send(actions, Action::Artifact(command))?; },
                                        Err(error) => { self.observe(Observation::Notice { text: error.to_string() })?; },
                                    }
                                }
                            }
                            _ if self.busy || received < self.ready_since => self.hint = "Draft kept. Press Enter after completion to send it.".into(),
                            _ if self.artifact_browser => self.hint = "Use /artifacts or /artifact commands; this browser does not submit model messages.".into(),
                            _ => {
                                let text = self.composer.take();
                                self.observe(Observation::User { text: text.clone() })?;
                                self.busy = true;
                                self.started = Some(Instant::now());
                                self.view.phase = "Starting turn".into();
                                self.hint.clear();
                                send(actions, Action::Submit(text))?;
                            }
                        }
                    }
                    _ => {
                        self.hint = if self.composer.key(key) {
                            String::new()
                        } else {
                            "Draft limit is 64 KiB".into()
                        };
                    }
                }
            }
            _ => (),
        }
        Ok(())
    }
}
fn send(actions: &async_mpsc::Sender<Action>, action: Action) -> io::Result<()> {
    actions
        .try_send(action)
        .map_err(|_| io::Error::other("console action consumer closed or overloaded"))
}
