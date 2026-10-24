//! Ephemeral Ollama metadata only. Unexpected routes (including inference) fail.
use axum::{
    Json, Router,
    extract::State,
    http::{StatusCode, Uri},
    routing::{get, post},
};
use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

pub struct MetadataState {
    pub tags: Value,
    pub show: Value,
    pub version: String,
    pub loaded: Value,
    pub calls: Vec<String>,
    pub delay: Duration,
}
#[derive(Clone)]
pub struct Metadata(pub Arc<Mutex<MetadataState>>);
pub struct FakeOllama {
    pub metadata: Metadata,
    pub endpoint: String,
    task: tokio::task::JoinHandle<()>,
}
impl FakeOllama {
    pub async fn start() -> Self {
        let lock: Value =
            serde_json::from_str(include_str!("../../../models/gpt-oss-20b.lock.json")).unwrap();
        let mut show = lock["definition"].clone();
        show["modelfile"] = lock["modelfile"].clone();
        let metadata = Metadata(Arc::new(Mutex::new(MetadataState {
            tags: json!({"models":[
                {"name":lock["name"], "digest":lock["digest"]},
                {"name":"alias:latest", "digest":lock["digest"]}
            ]}),
            show,
            version: lock["ollama_version"].as_str().unwrap().into(),
            loaded: json!({"models":[]}),
            calls: vec![],
            delay: Duration::ZERO,
        })));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new()
            .route("/api/version", get(version))
            .route("/api/tags", get(tags))
            .route("/api/show", post(show_model))
            .route("/api/ps", get(loaded))
            .fallback(unexpected)
            .with_state(metadata.clone());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            metadata,
            endpoint,
            task,
        }
    }
    pub fn stop(&self) {
        self.task.abort();
    }
}
impl Drop for FakeOllama {
    fn drop(&mut self) {
        self.stop();
    }
}
async fn version(State(s): State<Metadata>) -> Json<Value> {
    let (value, delay) = {
        let mut s = s.0.lock().unwrap();
        s.calls.push("version".into());
        (json!({"version":s.version}), s.delay)
    };
    tokio::time::sleep(delay).await;
    Json(value)
}
async fn tags(State(s): State<Metadata>) -> Json<Value> {
    let mut s = s.0.lock().unwrap();
    s.calls.push("tags".into());
    Json(s.tags.clone())
}
async fn show_model(State(s): State<Metadata>, Json(body): Json<Value>) -> Json<Value> {
    let mut s = s.0.lock().unwrap();
    s.calls
        .push(format!("show:{}", body["model"].as_str().unwrap()));
    Json(s.show.clone())
}
async fn loaded(State(s): State<Metadata>) -> Json<Value> {
    let mut s = s.0.lock().unwrap();
    s.calls.push("ps".into());
    Json(s.loaded.clone())
}

async fn unexpected(State(s): State<Metadata>, uri: Uri) -> StatusCode {
    s.0.lock().unwrap().calls.push(format!("unexpected:{uri}"));
    StatusCode::NOT_FOUND
}
