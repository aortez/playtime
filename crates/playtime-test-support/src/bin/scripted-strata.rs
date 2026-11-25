//! A scripted Strata endpoint for trying the chat console by hand, as
//! `scripts/chat-sandbox.py` does. It is the Strata crate's fixture server with
//! that fixture's model lock, and needs no model or GPU.
//!
//! `scripted-strata DIR [--pace-ms N]` writes `DIR/model.lock.json`, then
//! `DIR/endpoint` once it accepts requests. Each response request takes the
//! first `DIR/replies/*.json` in name order and renames it to `*.json.used`.
//! When none is queued, a short message that quotes the last user message
//! answers. Each request is summarized in `DIR/requests.jsonl`, and its body is
//! kept in `DIR/requests/NNNN.json`.
#[path = "../../../playtime-strata/tests/support/mod.rs"]
mod strata;

use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};
use strata::{
    Stream,
    server::{Reply, Server},
};

const USAGE: &str = "usage: scripted-strata DIR [--pace-ms N]";

/// One queued reply. Its items stream in this order: reasoning, message, calls.
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Spec {
    think: Option<String>,
    text: Option<String>,
    #[serde(default)]
    calls: Vec<Call>,
    #[serde(default)]
    end: End,
    /// Milliseconds before each event. Zero sends the reply at once.
    pace_ms: Option<u64>,
    /// Fields that replace the fixture's usage in a completed or incomplete
    /// response, such as `input_tokens`.
    usage: Option<Map<String, Value>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Call {
    name: String,
    arguments: Value,
}

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
enum End {
    #[default]
    Completed,
    /// Stopped at the output limit.
    Incomplete,
    Failed,
    /// No terminal event. The items arrive at once, and the connection stays
    /// open until the client leaves.
    Stall,
    /// No terminal event, and the connection closes.
    Drop,
}

struct Queue {
    dir: PathBuf,
    pace: u64,
    served: u64,
}

impl Queue {
    fn next(&mut self, body: &Value) -> Reply {
        self.served += 1;
        let input = serde_json::to_string(body).map_or(0, |text| tokens(&text));
        let make = |spec: &Spec, pace| build(spec, pace, self.served, input);
        let (source, reply) = match self.queued() {
            Some(path) => {
                let name = path.file_name().unwrap().to_string_lossy().into_owned();
                let spec = fs::read(&path)
                    .map_err(|error| error.to_string())
                    .and_then(|bytes| {
                        serde_json::from_slice::<Spec>(&bytes).map_err(|error| error.to_string())
                    });
                if let Err(error) = fs::rename(&path, path.with_extension("json.used")) {
                    eprintln!("scripted-strata: could not mark {name} used: {error}");
                }
                match spec {
                    Ok(spec) => (name, make(&spec, self.pace)),
                    Err(error) => {
                        eprintln!("scripted-strata: {name} is not a valid reply: {error}");
                        let failed = Spec {
                            end: End::Failed,
                            ..Spec::default()
                        };
                        (format!("{name} (invalid)"), make(&failed, 0))
                    }
                }
            }
            None => {
                let text = format!(
                    "(scripted-strata: nothing was queued for request {}) You said: {}",
                    self.served,
                    preview(last_user(body))
                );
                let spec = Spec {
                    text: Some(text),
                    ..Spec::default()
                };
                ("default".to_owned(), make(&spec, self.pace))
            }
        };
        if let Err(error) = self.record(body, &source) {
            let n = self.served;
            eprintln!("scripted-strata: could not record request {n}: {error}");
        }
        reply
    }

    /// The first queued reply in name order. Names starting with a dot are
    /// still being written.
    fn queued(&self) -> Option<PathBuf> {
        let mut names: Vec<PathBuf> = fs::read_dir(self.dir.join("replies"))
            .ok()?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| {
                let name = path.file_name().unwrap().to_string_lossy();
                !name.starts_with('.') && name.ends_with(".json")
            })
            .collect();
        names.sort();
        names.into_iter().next()
    }

    fn record(&self, body: &Value, source: &str) -> std::io::Result<()> {
        let n = self.served;
        fs::write(
            self.dir.join("requests").join(format!("{n:04}.json")),
            serde_json::to_vec_pretty(body)?,
        )?;
        let summary = json!({
            "request": n,
            "reply": source,
            "input_items": body["input"].as_array().map_or(0, Vec::len),
            "last_user": preview(last_user(body)),
        });
        let mut log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join("requests.jsonl"))?;
        writeln!(log, "{summary}")
    }
}

/// The reply's events. Usage counts about four bytes a token: `input` for the
/// request, and the reply's own text for its output.
fn build(spec: &Spec, default_pace: u64, request: u64, input: u64) -> Reply {
    let mut stream = Stream::new();
    if let Some(think) = &spec.think {
        stream.item("reasoning", think, "completed");
    }
    if let Some(text) = &spec.text {
        stream.item("message", text, "completed");
    }
    for call in &spec.calls {
        let arguments = call.arguments.to_string();
        stream.named("function_call", &call.name, &arguments, "completed");
    }
    match spec.end {
        End::Completed => stream.terminal("completed"),
        End::Incomplete => stream.terminal("incomplete"),
        End::Failed => stream.terminal("failed"),
        End::Stall | End::Drop => {}
    }
    if matches!(spec.end, End::Completed | End::Incomplete)
        && let Some(usage) = stream
            .events
            .last_mut()
            .and_then(|event| event.get_mut("response"))
            .and_then(|response| response.get_mut("usage"))
    {
        let count = |text: &Option<String>| text.as_deref().map_or(0, tokens);
        let reasoning = count(&spec.think);
        let calls: u64 = (spec.calls.iter())
            .map(|call| tokens(&call.arguments.to_string()))
            .sum();
        let output = (reasoning + count(&spec.text) + calls).max(1);
        *usage = json!({
            "input_tokens": input,
            "output_tokens": output,
            "total_tokens": input + output,
            "input_tokens_details": {"cached_tokens": 0},
            "output_tokens_details": {"reasoning_tokens": reasoning},
        });
        if let (Some(fields), Some(usage)) = (&spec.usage, usage.as_object_mut()) {
            usage.extend(fields.clone());
        }
    }
    for event in &mut stream.events {
        renumber(event, request);
    }
    let pace = spec.pace_ms.unwrap_or(default_pace);
    match spec.end {
        End::Stall => Reply::Stall(stream.bytes()),
        _ if pace == 0 => Reply::Events(stream.bytes()),
        _ => Reply::Paced(stream.frames(), Duration::from_millis(pace)),
    }
}

/// Adds the request number to the response, item and call IDs. The fixture
/// numbers items within one response, but a conversation's call IDs must
/// differ across responses, as Strata's do.
fn renumber(value: &mut Value, request: u64) {
    match value {
        Value::Array(values) => {
            for value in values {
                renumber(value, request);
            }
        }
        Value::Object(fields) => {
            for (key, value) in fields.iter_mut() {
                if matches!(key.as_str(), "id" | "item_id" | "call_id")
                    && let Some((kind, rest)) = value.as_str().and_then(|id| id.split_once('_'))
                {
                    *value = json!(format!("{kind}_{request}_{rest}"));
                } else {
                    renumber(value, request);
                }
            }
        }
        _ => {}
    }
}

fn tokens(text: &str) -> u64 {
    (text.len() as u64).div_ceil(4)
}

fn last_user(body: &Value) -> &str {
    body["input"]
        .as_array()
        .and_then(|items| items.iter().rev().find(|item| item["role"] == "user"))
        .and_then(|item| item["content"].as_str())
        .unwrap_or("")
}

/// The first line, shortened to 120 characters.
fn preview(text: &str) -> String {
    let line = text.trim().lines().next().unwrap_or("");
    let mut short: String = line.chars().take(120).collect();
    if short.len() < line.len() {
        short.push('…');
    }
    short
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let dir = PathBuf::from(args.next().ok_or(USAGE)?);
    let mut pace = 6;
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--pace-ms" => pace = args.next().ok_or(USAGE)?.parse()?,
            _ => return Err(USAGE.into()),
        }
    }
    for name in ["replies", "requests"] {
        fs::create_dir_all(dir.join(name))?;
    }
    let lock = serde_json::to_vec_pretty(&strata::lock())?;
    fs::write(dir.join("model.lock.json"), lock)?;
    let server = Server::new([]);
    let mut queue = Queue {
        dir: dir.clone(),
        pace,
        served: 0,
    };
    server.state.lock().unwrap().fallback = Some(Box::new(move |body| queue.next(body)));
    publish(&dir, &server.endpoint)?;
    println!("scripted-strata {}", server.endpoint);
    loop {
        std::thread::park();
    }
}

/// Writes the endpoint last and whole, so a reader that finds it can connect.
fn publish(dir: &Path, endpoint: &str) -> std::io::Result<()> {
    let partial = dir.join(".endpoint");
    fs::write(&partial, endpoint)?;
    fs::rename(&partial, dir.join("endpoint"))
}

fn main() {
    if let Err(error) = run() {
        eprintln!("scripted-strata: {error}");
        std::process::exit(1);
    }
}
