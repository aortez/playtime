use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{get, post},
};
use playtime_client::Client;
use playtime_ollama::Profile;
use playtime_protocol::api::*;
use playtime_server::{auth::Token, fixture::Fixture, http::Server, models::ModelBrowser};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

#[derive(Clone)]
struct Metadata(Arc<Mutex<MetadataState>>);
struct MetadataState {
    tags: Value,
    show: Value,
    version: String,
    calls: Vec<String>,
    loaded: Option<Value>,
}
async fn version(State(s): State<Metadata>) -> Json<Value> {
    let mut s = s.0.lock().unwrap();
    s.calls.push("version".into());
    Json(json!({"version":s.version}))
}
async fn tags(State(s): State<Metadata>) -> Json<Value> {
    let mut s = s.0.lock().unwrap();
    s.calls.push("tags".into());
    Json(s.tags.clone())
}
async fn show(State(s): State<Metadata>, Json(body): Json<Value>) -> Json<Value> {
    let mut s = s.0.lock().unwrap();
    s.calls
        .push(format!("show:{}", body["model"].as_str().unwrap()));
    Json(s.show.clone())
}
async fn loaded(State(s): State<Metadata>) -> Result<Json<Value>, StatusCode> {
    let mut s = s.0.lock().unwrap();
    s.calls.push("ps".into());
    s.loaded
        .clone()
        .map(Json)
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)
}
#[tokio::test]
async fn authenticated_browser_keeps_metadata_separate_from_execution() {
    let lock: Value =
        serde_json::from_str(include_str!("../../../models/gpt-oss-20b.lock.json")).unwrap();
    let mut definition = lock["definition"].clone();
    definition["modelfile"] = lock["modelfile"].clone();
    let metadata = Metadata(Arc::new(Mutex::new(MetadataState {
        tags: json!({"models":[
            {"name":Profile::MODEL,"digest":Profile::DIGEST},
            {"name":"alias/quoted\"model:latest","digest":Profile::DIGEST},
            {"name":"remote:cloud","digest":Profile::DIGEST,"remote_host":"https://remote.invalid"}
        ]}),
        show: definition,
        version: Profile::VERSION.into(),
        calls: vec![],
        loaded: None,
    })));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = Router::new()
        .route("/api/version", get(version))
        .route("/api/tags", get(tags))
        .route("/api/show", post(show))
        .route("/api/ps", get(loaded))
        .with_state(metadata.clone());
    let fake = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let fixture = Arc::new(
        Fixture::new(ServiceId::new("models-test").unwrap())
            .with_model_browser(ModelBrowser::new(&format!("http://{address}")).unwrap()),
    );
    let token = Token::generate().unwrap();
    let server = Server::new(fixture, token.clone())
        .unwrap()
        .listen("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let client = Client::connect(&server.endpoint(), token, None)
        .await
        .unwrap();
    let inventory = client.model_inventory().await.unwrap();
    assert_eq!(inventory.models.len(), 3);
    assert!(inventory.loaded_warning.is_some());
    assert!(
        inventory
            .models
            .iter()
            .all(|m| m.loaded == ModelLoaded::Unknown)
    );
    assert_eq!(
        inventory
            .models
            .iter()
            .find(|m| m.name == "remote:cloud")
            .unwrap()
            .status,
        ModelMetadataStatus::Rejected
    );
    let request = |name: &str| ModelInspectRequest {
        name: name.into(),
        expected_digest: Some(Profile::DIGEST.into()),
    };
    let inspected = client
        .inspect_model(&request(Profile::MODEL))
        .await
        .unwrap();
    assert_eq!(inspected.status, ModelMetadataStatus::Candidate);
    assert!(inspected.reviewed_capabilities.is_some());
    assert_eq!(
        inspected.definition_sha256.as_deref(),
        Some(Profile::DEFINITION)
    );
    metadata.0.lock().unwrap().loaded =
        Some(json!({"models":[{"name":Profile::MODEL,"digest":Profile::DIGEST}]}));
    let loaded = client.model_inventory().await.unwrap();
    assert!(loaded.loaded_warning.is_none());
    assert_eq!(
        loaded
            .models
            .iter()
            .find(|m| m.name == Profile::MODEL)
            .unwrap()
            .loaded,
        ModelLoaded::Loaded
    );
    assert_eq!(
        loaded
            .models
            .iter()
            .find(|m| m.name == "alias/quoted\"model:latest")
            .unwrap()
            .loaded,
        ModelLoaded::NotLoaded
    );
    let again = client
        .inspect_model(&request(Profile::MODEL))
        .await
        .unwrap();
    assert_eq!(again.digest, inspected.digest);
    assert_eq!(again.definition_sha256, inspected.definition_sha256);
    assert_eq!(again.reviewed_capabilities, inspected.reviewed_capabilities);
    let alias = client
        .inspect_model(&request("alias/quoted\"model:latest"))
        .await
        .unwrap();
    assert_eq!(alias.status, ModelMetadataStatus::Candidate);
    assert!(alias.reviewed_capabilities.is_none());
    assert_eq!(
        client
            .inspect_model(&request("missing"))
            .await
            .unwrap()
            .status,
        ModelMetadataStatus::Missing
    );
    assert_eq!(
        client
            .inspect_model(&request("remote:cloud"))
            .await
            .unwrap()
            .status,
        ModelMetadataStatus::Rejected
    );
    let changed = ModelInspectRequest {
        expected_digest: Some("a".repeat(64)),
        ..request(Profile::MODEL)
    };
    assert_eq!(
        client.inspect_model(&changed).await.unwrap().status,
        ModelMetadataStatus::Changed
    );
    metadata.0.lock().unwrap().version = "unknown-version".into();
    assert_eq!(
        client
            .inspect_model(&request(Profile::MODEL))
            .await
            .unwrap()
            .status,
        ModelMetadataStatus::Unverified
    );
    metadata.0.lock().unwrap().version = Profile::VERSION.into();
    metadata.0.lock().unwrap().show["capabilities"] = json!(["completion"]);
    let no_tools = client
        .inspect_model(&request(Profile::MODEL))
        .await
        .unwrap();
    assert!(no_tools.reviewed_capabilities.is_none());
    assert!(
        no_tools
            .reason
            .unwrap()
            .contains("does not advertise tools")
    );
    metadata.0.lock().unwrap().show["capabilities"] = Value::Null;
    assert_eq!(
        client
            .inspect_model(&request(Profile::MODEL))
            .await
            .unwrap()
            .status,
        ModelMetadataStatus::Unverified
    );
    metadata.0.lock().unwrap().show["remote_model"] = json!("delegated");
    assert_eq!(
        client
            .inspect_model(&request(Profile::MODEL))
            .await
            .unwrap()
            .status,
        ModelMetadataStatus::Rejected
    );
    // Failed metadata doesn't take down the service or grant execution authority.
    fake.abort();
    let _ = fake.await;
    assert!(client.model_inventory().await.is_err());
    assert!(client.schema().await.is_ok());
    assert!(
        metadata
            .0
            .lock()
            .unwrap()
            .calls
            .iter()
            .all(|op| op == "version" || op == "tags" || op == "ps" || op.starts_with("show:"))
    );
    server.stop().await.unwrap();
}
