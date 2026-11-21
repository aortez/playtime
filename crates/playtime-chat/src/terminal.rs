//! Terminal input is separate from model streaming and the conversation driver.
//! Bracketed paste is one editable message; only a subsequent Enter submits it.
use crate::Result;
use rustix::{
    event::{PollFd, PollFlags, Timespec, poll},
    termios::{self, OptionalActions, Termios},
};
use std::{
    fs::File,
    io::{self, BufRead, IsTerminal, Write},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use unicode_width::UnicodeWidthChar;

const MAX: usize = playtime_engine::chat::MAX_USER_BYTES;
#[derive(Debug, PartialEq)]
pub enum Key {
    Text(String),
    Line(String),
    Enter,
    Escape,
    Interrupt,
    Eof,
    Backspace,
    Delete,
    Left,
    Right,
    Home,
    End,
    Up,
    Down,
    Clear,
    Warning(String),
}
#[derive(Default)]
struct Decoder {
    bytes: Vec<u8>,
    paste: Option<Vec<u8>>,
    overflow: bool,
    escape_since: Option<Instant>,
}
impl Decoder {
    fn feed(&mut self, bytes: &[u8]) -> Vec<Key> {
        self.bytes.extend_from_slice(bytes);
        let mut events = vec![];
        loop {
            if let Some(paste) = &mut self.paste {
                if let Some(end) = self.bytes.windows(6).position(|x| x == b"\x1b[201~") {
                    if paste.len() + end <= MAX {
                        paste.extend_from_slice(&self.bytes[..end]);
                    } else {
                        self.overflow = true;
                    }
                    self.bytes.drain(..end + 6);
                    let data = self.paste.take().unwrap();
                    events.push(if self.overflow {
                        Key::Warning("Paste exceeds 256 KiB; nothing was submitted.".into())
                    } else {
                        Key::Text(
                            String::from_utf8_lossy(&data)
                                .replace("\r\n", "\n")
                                .replace('\r', "\n"),
                        )
                    });
                    self.overflow = false;
                    continue;
                }
                let n = self.bytes.len().saturating_sub(5);
                if paste.len() + n <= MAX {
                    paste.extend_from_slice(&self.bytes[..n]);
                } else {
                    self.overflow = true;
                }
                self.bytes.drain(..n);
                break;
            }
            let Some(&byte) = self.bytes.first() else {
                self.escape_since = None;
                break;
            };
            if byte == 27 {
                let since = *self.escape_since.get_or_insert_with(Instant::now);
                if self.bytes.starts_with(b"\x1b[200~") {
                    self.bytes.drain(..6);
                    self.paste = Some(vec![]);
                    self.escape_since = None;
                    continue;
                }
                if self.bytes.len() == 1 && since.elapsed() >= Duration::from_millis(40) {
                    self.bytes.remove(0);
                    self.escape_since = None;
                    events.push(Key::Escape);
                    continue;
                }
                if self.bytes.len() > 1 && matches!(self.bytes[1], b'[' | b'O') {
                    if let Some(n) = self
                        .bytes
                        .iter()
                        .enumerate()
                        .skip(2)
                        .find_map(|(i, b)| (0x40..=0x7e).contains(b).then_some(i))
                    {
                        let sequence = &self.bytes[..=n];
                        let key = match sequence {
                            b"\x1b[A" => Some(Key::Up),
                            b"\x1b[B" => Some(Key::Down),
                            b"\x1b[C" => Some(Key::Right),
                            b"\x1b[D" => Some(Key::Left),
                            b"\x1b[H" | b"\x1bOH" | b"\x1b[1~" => Some(Key::Home),
                            b"\x1b[F" | b"\x1bOF" | b"\x1b[4~" => Some(Key::End),
                            b"\x1b[3~" => Some(Key::Delete),
                            _ => None,
                        };
                        if let Some(key) = key {
                            events.push(key);
                        }
                        self.bytes.drain(..=n);
                        self.escape_since = None;
                        continue;
                    }
                } else if self.bytes.len() > 1 {
                    // Alt-key sequences are not a standalone Escape cancellation.
                    self.bytes.remove(0);
                    self.escape_since = None;
                    continue;
                }
                if since.elapsed() >= Duration::from_millis(100) {
                    self.bytes.clear();
                    self.escape_since = None;
                }
                break;
            }
            self.escape_since = None;
            let key = match byte {
                3 => Some(Key::Interrupt),
                4 => Some(Key::Eof),
                b'\r' | b'\n' => Some(Key::Enter),
                8 | 127 => Some(Key::Backspace),
                1 => Some(Key::Home),
                5 => Some(Key::End),
                21 => Some(Key::Clear),
                _ => None,
            };
            if let Some(key) = key {
                self.bytes.remove(0);
                events.push(key);
                continue;
            }
            let n = if byte < 128 {
                1
            } else if byte & 0xe0 == 0xc0 {
                2
            } else if byte & 0xf0 == 0xe0 {
                3
            } else if byte & 0xf8 == 0xf0 {
                4
            } else {
                1
            };
            if self.bytes.len() < n {
                break;
            }
            if byte >= 32 || byte == b'\t' {
                events.push(Key::Text(String::from_utf8_lossy(&self.bytes[..n]).into()));
            }
            self.bytes.drain(..n);
        }
        events
    }
}

pub struct Console {
    rx: mpsc::Receiver<io::Result<Key>>,
    terminal: Option<Termios>,
    stop: Arc<AtomicBool>,
    reader: Option<thread::JoinHandle<()>>,
    history: Vec<String>,
}
impl Console {
    pub fn new() -> Result<Self> {
        let (tx, rx) = mpsc::channel(256);
        let stop = Arc::new(AtomicBool::new(false));
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            thread::spawn(move || {
                for line in io::stdin().lock().lines() {
                    if tx.blocking_send(line.map(Key::Line)).is_err() {
                        break;
                    }
                }
            });
            return Ok(Self {
                rx,
                terminal: None,
                stop,
                reader: None,
                history: vec![],
            });
        }
        let original = termios::tcgetattr(io::stdin())?;
        let mut mode = original.clone();
        mode.make_raw();
        mode.output_modes = original.output_modes;
        let input: File = rustix::io::dup(io::stdin())?.into();
        termios::tcsetattr(io::stdin(), OptionalActions::Now, &mode)?;
        let mut console = Self {
            rx,
            terminal: Some(original),
            stop: stop.clone(),
            reader: None,
            history: vec![],
        };
        print!("\x1b[?2004h");
        io::stdout().flush()?;
        console.reader = Some(thread::spawn(move || {
            let mut decoder = Decoder::default();
            let mut buffer = [0u8; 4096];
            while !stop.load(Ordering::Relaxed) {
                let mut fds = [PollFd::new(&input, PollFlags::IN)];
                let ready = poll(
                    &mut fds,
                    Some(&Timespec {
                        tv_sec: 0,
                        tv_nsec: 20_000_000,
                    }),
                );
                let events = match ready {
                    Ok(0) => decoder.feed(&[]),
                    Ok(_) => match rustix::io::read(&input, &mut buffer) {
                        Ok(0) => {
                            let _ = tx.blocking_send(Ok(Key::Eof));
                            break;
                        }
                        Ok(n) => decoder.feed(&buffer[..n]),
                        Err(e) => {
                            let _ = tx.blocking_send(Err(e.into()));
                            break;
                        }
                    },
                    Err(rustix::io::Errno::INTR) => continue,
                    Err(e) => {
                        let _ = tx.blocking_send(Err(e.into()));
                        break;
                    }
                };
                for event in events {
                    if tx.blocking_send(Ok(event)).is_err() {
                        return;
                    }
                }
            }
        }));
        Ok(console)
    }
    pub async fn recv(&mut self) -> Result<Option<Key>> {
        self.rx.recv().await.transpose().map_err(Into::into)
    }
    pub fn discard_pending(&mut self) {
        while self.rx.try_recv().is_ok() {}
    }
    pub async fn prompt(&mut self, shutdown: &CancellationToken) -> Result<Option<String>> {
        let mut text = String::new();
        let mut cursor = 0;
        let mut history = self.history.len();
        print!("\nyou> ");
        io::stdout().flush()?;
        loop {
            let key = tokio::select! {key=self.recv()=>key?,_=shutdown.cancelled()=>return Ok(None),_=tokio::signal::ctrl_c()=>return Ok(None)};
            match key {
                None => return Ok(None),
                Some(Key::Eof) if text.is_empty() => return Ok(None),
                Some(Key::Line(line)) => return Ok(Some(line)),
                Some(Key::Enter) => {
                    println!();
                    if !text.trim().is_empty() {
                        self.history.push(text.clone());
                        if self.history.len() > 64 {
                            self.history.remove(0);
                        }
                    }
                    return Ok(Some(text));
                }
                Some(Key::Interrupt) if text.is_empty() => return Ok(None),
                Some(Key::Escape | Key::Interrupt | Key::Clear) => {
                    text.clear();
                    cursor = 0;
                }
                Some(Key::Text(value)) => {
                    if text.len() + value.len() <= MAX {
                        text.insert_str(cursor, &value);
                        cursor += value.len();
                    } else {
                        println!("\nInput exceeds 256 KiB.");
                    }
                }
                Some(Key::Left) => {
                    cursor = text[..cursor]
                        .char_indices()
                        .next_back()
                        .map_or(0, |(i, _)| i)
                }
                Some(Key::Right) => {
                    cursor += text[cursor..].chars().next().map_or(0, char::len_utf8);
                }
                Some(Key::Home) => cursor = 0,
                Some(Key::End) => cursor = text.len(),
                Some(Key::Backspace) if cursor > 0 => {
                    let start = text[..cursor].char_indices().next_back().unwrap().0;
                    text.drain(start..cursor);
                    cursor = start;
                }
                Some(Key::Delete | Key::Eof) if cursor < text.len() => {
                    let end = cursor + text[cursor..].chars().next().unwrap().len_utf8();
                    text.drain(cursor..end);
                }
                Some(Key::Up) if history > 0 => {
                    history -= 1;
                    text = self.history[history].clone();
                    cursor = text.len();
                }
                Some(Key::Down) => {
                    history = (history + 1).min(self.history.len());
                    text = self.history.get(history).cloned().unwrap_or_default();
                    cursor = text.len();
                }
                Some(Key::Warning(warning)) => println!("\n{warning}"),
                _ => (),
            }
            if self.terminal.is_some() {
                redraw(&text, cursor)?;
            }
        }
    }
}
fn shown(c: char) -> String {
    match c {
        '\n' => " ↵ ".into(),
        '\t' => "    ".into(),
        c if c.is_control() => format!("\\u{{{:x}}}", c as u32),
        c => c.to_string(),
    }
}
fn width(text: &str) -> usize {
    text.chars().map(|c| c.width().unwrap_or(0)).sum()
}
fn redraw(text: &str, cursor: usize) -> io::Result<()> {
    let columns = termios::tcgetwinsize(io::stdout()).map_or(80, |w| usize::from(w.ws_col).max(20));
    let budget = columns.saturating_sub(7);
    let mut before = String::new();
    for c in text[..cursor].chars().rev() {
        let part = shown(c);
        if width(&before) + width(&part) > budget {
            break;
        }
        before.insert_str(0, &part);
    }
    let position = 5 + width(&before);
    let mut after = String::new();
    for c in text[cursor..].chars() {
        let part = shown(c);
        if width(&before) + width(&after) + width(&part) > budget {
            break;
        }
        after.push_str(&part);
    }
    print!("\r\x1b[2Kyou> {before}{after}\r\x1b[{position}C");
    io::stdout().flush()
}
impl Drop for Console {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.rx.close();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        if let Some(original) = &self.terminal {
            let _ = termios::tcsetattr(io::stdin(), OptionalActions::Now, original);
            let _ = write!(io::stdout(), "\x1b[?2004l");
            let _ = io::stdout().flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn paste_is_one_literal_message_even_with_escape_control_and_split_markers() {
        let mut parser = Decoder::default();
        let mut events = vec![];
        for b in "\x1b[200~first\r\nsecond\x1b\u{3}界\x1b[201~".bytes() {
            events.extend(parser.feed(&[b]));
        }
        assert_eq!(events, vec![Key::Text("first\nsecond\x1b\u{3}界".into())]);
        assert_eq!(parser.feed(b"\r"), vec![Key::Enter]);
    }
    #[test]
    fn escape_is_timed_but_arrow_sequences_do_not_cancel() {
        let mut parser = Decoder::default();
        assert!(parser.feed(b"\x1b").is_empty());
        assert_eq!(parser.feed(b"[D"), vec![Key::Left]);
        assert!(parser.feed(b"\x1b").is_empty());
        parser.escape_since = Some(Instant::now() - Duration::from_millis(50));
        assert_eq!(parser.feed(&[]), vec![Key::Escape]);
    }
    #[test]
    fn oversized_paste_is_discarded_until_its_end_marker() {
        let mut parser = Decoder::default();
        parser.feed(b"\x1b[200~");
        parser.feed(&vec![b'x'; MAX + 100]);
        assert!(matches!(
            parser.feed(b"\x1b[201~").as_slice(),
            [Key::Warning(_)]
        ));
        assert_eq!(
            parser.feed(b"ok"),
            vec![Key::Text("o".into()), Key::Text("k".into())]
        );
    }
}
