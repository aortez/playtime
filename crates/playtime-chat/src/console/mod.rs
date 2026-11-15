//! A single terminal output owner, separate from the shared runtime. Presentation
//! observations and explicit actions are also usable by the offline console CLI.
mod composer;
pub mod markdown;
pub mod presentation;
mod terminal;

use crate::{Result, runtime::Event};
pub use composer::{Composer, MAX_DRAFT_BYTES};
use crossterm::event::{self, Event as TerminalEvent, KeyCode, KeyEventKind, KeyModifiers};
pub use presentation::{Observation, Presentation};
use serde::Serialize;
use std::{
    io::{self, IsTerminal},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};
use tokio::sync::mpsc as async_mpsc;
use tokio_util::sync::CancellationToken;

pub const HELP: &str = "Enter sends when idle; Ctrl-J or Alt-Enter inserts a newline. Paste never sends. Arrows/Home/End edit; Ctrl-Z/Y undo/redo; Alt-Up/Down recalls history and restores your draft. During work Enter keeps the draft; Ctrl-C or /stop stops and waits for cleanup. /raw [number] shows a recent recorded tool result. /artifacts lists retained files; /artifact retain PATH --caption \"Description\" [--source SOURCE] copies a project file after a completed turn; /artifact info ID, /artifact open ID, /artifact export ID --output NEW_DIRECTORY. Quote paths with spaces. Retained files are not sent to the model. /help shows keys; /quit exits. Completed output stays in terminal scrollback. Drafts/history are in memory for this process only.";

#[derive(Debug, Serialize)]
#[serde(tag = "type", content = "text", rename_all = "snake_case")]
pub enum Action {
    Submit(String),
    Artifact(crate::artifacts::ArtifactCommand),
    Stop,
    Quit,
}
pub(super) enum Command {
    Observation(Observation),
    Active(bool),
    Input(TerminalEvent, Instant),
    Shutdown,
}
type Active = Arc<Mutex<Option<CancellationToken>>>;
fn cancel(active: &Active) {
    if let Some(token) = active.lock().unwrap().as_ref() {
        token.cancel();
    }
}
pub struct Console {
    commands: mpsc::SyncSender<Command>,
    pub actions: async_mpsc::Receiver<Action>,
    active: Active,
    thread: Option<thread::JoinHandle<io::Result<()>>>,
}
impl Console {
    pub fn supported() -> bool {
        io::stdin().is_terminal()
            && io::stdout().is_terminal()
            && !matches!(std::env::var("TERM").as_deref(), Ok("dumb") | Ok(""))
    }
    pub fn start(banner: String) -> Result<Self> {
        Self::start_inner(banner, false)
    }
    pub fn start_artifact_browser(banner: String) -> Result<Self> {
        Self::start_inner(banner, true)
    }
    fn start_inner(banner: String, artifact_browser: bool) -> Result<Self> {
        let (commands, rx) = mpsc::sync_channel(256);
        let (actions, action_rx) = async_mpsc::channel(16);
        let active: Active = Arc::default();
        let (ready, started) = mpsc::sync_channel(1);
        let control = active.clone();
        let input_tx = commands.clone();
        let handle = thread::Builder::new().name("chat-terminal".into()).spawn(move || {
            let mut terminal = match terminal::Terminal::new(artifact_browser) {
                Ok(terminal) => { let _ = ready.send(Ok(())); terminal },
                Err(error) => { let _ = ready.send(Err(error.to_string())); return Err(error); }
            };
            let stop = Arc::new(AtomicBool::new(false));
            let reader_stop = stop.clone();
            let reader_control = control.clone();
            let reader_actions = actions.clone();
            let reader = thread::spawn(move || {
                // Input cancellation never waits behind rendering or stdout writes.
                while !reader_stop.load(Ordering::Relaxed) {
                    let event = match event::poll(Duration::from_millis(30)) {
                        Ok(false) => continue,
                        Ok(true) => event::read(),
                        Err(e) => Err(e),
                    };
                    let Ok(event) = event else { cancel(&reader_control); let _ = reader_actions.try_send(Action::Quit); break; };
                    if matches!(event, TerminalEvent::Key(key) if key.kind != KeyEventKind::Release && key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)) {
                        cancel(&reader_control);
                    }
                    if input_tx.try_send(Command::Input(event, Instant::now())).is_err() {
                        cancel(&reader_control);
                        let _ = reader_actions.try_send(Action::Quit);
                        break;
                    }
                }
            });
            let result = terminal.run(rx, actions, &control, banner);
            stop.store(true, Ordering::Relaxed);
            let _ = reader.join();
            if result.is_err() { cancel(&control); }
            result
        })?;
        match started.recv()? {
            Ok(()) => Ok(Self {
                commands,
                actions: action_rx,
                active,
                thread: Some(handle),
            }),
            Err(error) => {
                let _ = handle.join();
                Err(error.into())
            }
        }
    }
    fn send(&self, command: Command) -> Result<()> {
        self.commands.try_send(command).map_err(|_| {
            cancel(&self.active);
            "terminal closed or overloaded; stopping work".into()
        })
    }
    pub fn observe(&self, observation: Observation) -> Result<()> {
        self.send(Command::Observation(observation))
    }
    pub fn event(&self, event: &Event) -> Result<()> {
        self.observe(event.into())
    }
    pub fn notice(&self, text: impl Into<String>) -> Result<()> {
        self.observe(Observation::Notice { text: text.into() })
    }
    pub fn set_active(&self, token: Option<CancellationToken>) -> Result<()> {
        let active = token.is_some();
        *self.active.lock().unwrap() = token;
        self.send(Command::Active(active))
    }
    pub fn finish(mut self) -> Result<()> {
        self.close().map_err(Into::into)
    }
    fn close(&mut self) -> io::Result<()> {
        if let Some(handle) = self.thread.take() {
            let _ = self.commands.send(Command::Shutdown);
            handle
                .join()
                .map_err(|_| io::Error::other("terminal thread panicked"))??;
        }
        Ok(())
    }
}
impl Drop for Console {
    fn drop(&mut self) {
        let _ = self.close();
    }
}
