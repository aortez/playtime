#![allow(dead_code)]
use playtime_core::RequestId;
use playtime_engine::ModelRequest;
use playtime_ollama::*;
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::time::Instant;

pub fn lock() -> ModelLock {
    ModelLock::parse(include_bytes!("../../../../models/gpt-oss-20b.lock.json")).unwrap()
}
pub fn request() -> ModelRequest {
    ModelRequest {
        request_id: RequestId::new("fixture-request").unwrap(),
        call: 1,
        handoff: false,
        sampling_seed: 17,
        maximum_tokens: 512,
        messages: vec![json!({"role":"user","content":"fixture é"})],
        tools: vec![
            json!({"type":"function","function":{"name":"fixture","parameters":{"type":"object"}}}),
        ],
        context_input_bound: 100,
        context_basis: "fixture".into(),
        context_reserve: 512,
    }
}
pub fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(5)
}
pub fn metadata() -> Vec<Value> {
    vec![
        json!({"version":Profile::VERSION}),
        json!({"models":[{"name":Profile::MODEL,"digest":Profile::DIGEST}]}),
        lock().definition,
    ]
}
pub fn response(status: u16, body: impl Into<Vec<u8>>) -> HttpResponse {
    HttpResponse {
        status,
        content_encoding: None,
        body: Box::pin(futures_util::stream::iter([Ok(body.into())])),
    }
}
pub struct Fake {
    pub pending: Mutex<VecDeque<HttpResponse>>,
    pub seen: Arc<Mutex<Vec<Request>>>,
}
impl Fake {
    pub fn new(responses: impl IntoIterator<Item = HttpResponse>) -> Self {
        Self {
            pending: Mutex::new(responses.into_iter().collect()),
            seen: Arc::default(),
        }
    }
    pub fn ready(extra: impl IntoIterator<Item = HttpResponse>) -> Self {
        Self::new(
            metadata()
                .into_iter()
                .map(|v| response(200, serde_json::to_vec(&v).unwrap()))
                .chain(extra),
        )
    }
}
#[async_trait::async_trait]
impl Transport for Fake {
    async fn send(&self, request: Request) -> Result<HttpResponse> {
        self.seen.lock().unwrap().push(request);
        self.pending
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| Failure::new(FailureKind::Transport, "unexpected fixture dispatch"))
    }
}
