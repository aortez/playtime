//! The live Strata port. The daemon holds the host and freezes the endpoint the
//! hold lent; this worker verifies that same server process and streams each
//! request, and never resends one that may have reached the server.
use crate::docker::{CHUNK_INTERVAL, CHUNK_TEXT_BYTES};
use playtime_docker::CancellationToken;
use playtime_engine::{ModelRequest, Outcome};
use playtime_runner::{Error, Result, docker::StrataModel};
use playtime_strata::{Client, Endpoint, Limits, RemoteState, StreamEvent};
use serde_json::{Value, json};
use std::{collections::BTreeMap, mem::take, sync::mpsc};
use tokio::time::Instant;

pub(crate) struct Strata {
    client: Client,
    /// Set while a request may still be running on the server. Nothing more
    /// is sent, as in the chat driver.
    fenced: bool,
}
impl Strata {
    pub(crate) fn prepare(model: &StrataModel) -> crate::Result<Self> {
        let invalid = |_| crate::Error::Invalid("invalid frozen Strata model configuration");
        let endpoint = Endpoint::parse(&model.host.endpoint).map_err(invalid)?;
        let client = Client::new(endpoint, model.client_lock(), Limits::default())
            .map_err(invalid)?
            .with_server_start(model.host.started);
        Ok(Self {
            client,
            fenced: false,
        })
    }
    /// Refuses another model or server process, or a server with other work.
    pub(crate) async fn verify(
        &mut self,
        deadline: Instant,
        cancel: &CancellationToken,
    ) -> Result<()> {
        if self.fenced {
            return Err(fenced());
        }
        self.client
            .verify(deadline, cancel)
            .await
            .map(drop)
            .map_err(|e| Error::Invalid(format!("Strata model identity: {e}")))
    }
    /// Streamed deltas become chunks while the request runs, so partial output
    /// is visible live and stays recorded if the request fails. A final chunk
    /// records whether the request reached the server and how it ended.
    pub(crate) async fn complete(
        &mut self,
        id: u64,
        request: &ModelRequest,
        deadline: Instant,
        cancel: &CancellationToken,
        chunks: &mpsc::Sender<(u64, Value)>,
    ) -> Result<Outcome> {
        if self.fenced {
            return Err(fenced());
        }
        self.fenced = true;
        let (sender, mut receiver) = playtime_strata::events();
        let started = std::time::Instant::now();
        let mut pending = Deltas::default();
        let result = {
            let complete = self.client.complete(request, deadline, cancel, sender);
            tokio::pin!(complete);
            loop {
                let due = pending.since.map(|since| since + CHUNK_INTERVAL);
                tokio::select! {
                    result = &mut complete => break result,
                    Some(event) = receiver.recv() => pending.add(id, &event, chunks),
                    () = tokio::time::sleep_until(due.unwrap_or(deadline)), if due.is_some() => {
                        pending.flush(id, chunks);
                    }
                }
            }
        };
        while let Ok(event) = receiver.try_recv() {
            pending.add(id, &event, chunks);
        }
        pending.flush(id, chunks);
        let mut call = json!({"observation": "strata_call", "remote": result.remote,
            "events": pending.events, "received_bytes": result.received_bytes,
            "observed_ms": started.elapsed().as_millis() as u64});
        if let Some(terminal) = pending.terminal.take() {
            call["terminal"] = terminal;
        }
        // A retired effect owner discards chunks; nothing is lost while it drains.
        let _ = chunks.send((id, call));
        self.fenced = result.remote == RemoteState::Uncertain;
        Ok(result.into_outcome())
    }
}
fn fenced() -> Error {
    Error::Invalid("an earlier Strata request may still be running; nothing more is sent".into())
}

/// Text deltas not yet sent as a chunk, in the message shape the transcript
/// renderers read. Each event arrives about once per token, so they're
/// coalesced like native Ollama records.
#[derive(Default)]
struct Deltas {
    thinking: String,
    content: String,
    /// Function-call argument deltas by item, in arrival order.
    arguments: Vec<(String, String)>,
    /// Function names by item, from each item's announcement.
    names: BTreeMap<String, String>,
    /// Deltas since the last chunk.
    deltas: u64,
    since: Option<Instant>,
    /// Every event this request.
    events: u64,
    terminal: Option<Value>,
}
impl Deltas {
    fn add(&mut self, id: u64, event: &StreamEvent, chunks: &mpsc::Sender<(u64, Value)>) {
        self.events += 1;
        let data = &event.data;
        let delta = data["delta"].as_str().unwrap_or_default();
        match data["type"].as_str().unwrap_or_default() {
            "response.output_text.delta" => self.content.push_str(delta),
            "response.reasoning_text.delta" => self.thinking.push_str(delta),
            "response.function_call_arguments.delta" => {
                let item = data["item_id"].as_str().unwrap_or_default();
                match self.arguments.last_mut() {
                    Some((last, text)) if last == item => text.push_str(delta),
                    _ => self.arguments.push((item.into(), delta.into())),
                }
            }
            "response.output_item.added" => {
                let item = &data["item"];
                if let (Some(id), Some(name)) = (item["id"].as_str(), item["name"].as_str()) {
                    self.names.insert(id.into(), name.into());
                }
                return;
            }
            "response.completed" | "response.incomplete" | "response.failed" => {
                self.terminal = Some(terminal(data));
                return;
            }
            _ => return,
        }
        self.deltas += 1;
        self.since.get_or_insert_with(Instant::now);
        let arguments: usize = self.arguments.iter().map(|(_, text)| text.len()).sum();
        if self.thinking.len() + self.content.len() + arguments >= CHUNK_TEXT_BYTES {
            self.flush(id, chunks);
        }
    }
    fn flush(&mut self, id: u64, chunks: &mpsc::Sender<(u64, Value)>) {
        if self.deltas == 0 {
            return;
        }
        let mut message = json!({"role": "assistant"});
        for (key, text) in [
            ("thinking", take(&mut self.thinking)),
            ("content", take(&mut self.content)),
        ] {
            if !text.is_empty() {
                message[key] = text.into();
            }
        }
        let mut chunk =
            json!({"observation": "strata_deltas", "message": message, "deltas": self.deltas});
        if !self.arguments.is_empty() {
            let arguments = take(&mut self.arguments).into_iter();
            chunk["arguments"] = arguments
                .map(|(item, delta)| json!({"item_id": item, "name": self.names.get(&item), "delta": delta}))
                .collect();
        }
        (self.deltas, self.since) = (0, None);
        let _ = chunks.send((id, chunk));
    }
}
/// The terminal event without its output text, which the controller records
/// with the response. Status, usage, timings and errors stay as sent.
fn terminal(event: &Value) -> Value {
    let mut event = event.clone();
    if let Some(output) = event
        .pointer_mut("/response/output")
        .and_then(Value::as_array_mut)
    {
        for item in output {
            let parts = ["content", "summary"]
                .iter()
                .filter_map(|key| item[key].as_array())
                .flatten()
                .filter_map(|part| part["text"].as_str());
            let text_bytes =
                parts.map(str::len).sum::<usize>() + item["arguments"].as_str().map_or(0, str::len);
            if let Some(fields) = item.as_object_mut() {
                fields.retain(|key, _| {
                    matches!(
                        key.as_str(),
                        "id" | "type" | "status" | "role" | "name" | "call_id"
                    )
                });
                fields.insert("text_bytes".into(), text_bytes.into());
            }
        }
    }
    event
}
