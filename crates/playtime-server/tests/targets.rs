//! Authenticated target metadata contracts with two fake Ollama servers; no inference.
mod common;
use playtime_client::Client;
use playtime_protocol::api::*;
use playtime_server::{auth::Token, fixture::Fixture, http::Server};
use playtime_test_support::model_metadata::FakeOllama;
use std::sync::Arc;
fn target(id: &str, endpoint: &str) -> InferenceTarget {
    InferenceTarget {
        id: id.into(),
        name: id.into(),
        provider: InferenceProvider::Ollama,
        endpoint: endpoint.into(),
        profile: None,
    }
}
#[tokio::test]
async fn target_scope_selects_metadata_without_fallback_or_inference() {
    let bird = FakeOllama::start().await;
    let framework = FakeOllama::start().await;
    let targets = InferenceTargets {
        schema_version: 1,
        targets: vec![
            target("bird", &bird.endpoint),
            target("framework", &framework.endpoint),
        ],
    };
    let root = common::Temp::new();
    let ledger = playtime_server::lifecycle::Ledger::open(&root.0)
        .await
        .unwrap();
    let registry = playtime_server::registrations::Registry::new(ledger, None);
    let app = Fixture::new(ServiceId::new("targets-test").unwrap())
        .with_model_registry(registry)
        .with_targets(targets.clone())
        .unwrap();
    let token = Token::generate().unwrap();
    let server = Server::new(Arc::new(app), token.clone())
        .unwrap()
        .listen("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let client = Client::connect(&server.endpoint(), token.clone(), None)
        .await
        .unwrap();
    assert_eq!(client.inference_targets().await.unwrap(), targets);
    let response = reqwest::Client::new()
        .get(format!("{}/api/v1/models", server.endpoint()))
        .bearer_auth(token.expose())
        .header(TARGET_HEADER, "bird")
        .header(TARGET_HEADER, "framework")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
    assert!(bird.metadata.0.lock().unwrap().calls.is_empty());
    assert!(framework.metadata.0.lock().unwrap().calls.is_empty());
    assert_eq!(
        client.model_inventory().await.unwrap_err().api_code(),
        Some("invalid_parameters"),
        "no implicit first target"
    );
    let a = client.with_target(Some("bird".into())).unwrap();
    let b = client.with_target(Some("framework".into())).unwrap();
    a.model_inventory().await.unwrap();
    assert!(framework.metadata.0.lock().unwrap().calls.is_empty());
    b.model_inventory().await.unwrap();
    let request = ModelInspectRequest {
        name: playtime_ollama::Profile::MODEL.into(),
        expected_digest: Some(playtime_ollama::Profile::DIGEST.into()),
    };
    let ar = a.review_model(&request).await.unwrap();
    let br = b.review_model(&request).await.unwrap();
    assert_eq!(
        ar.request.as_ref().unwrap().identity,
        br.request.as_ref().unwrap().identity
    );
    assert_ne!(
        ar.request.unwrap().endpoint_binding,
        br.request.unwrap().endpoint_binding
    );
    framework.stop();
    assert!(b.model_inventory().await.is_err());
    a.model_inventory().await.unwrap();
    assert!(
        client
            .with_target(Some("missing".into()))
            .unwrap()
            .model_inventory()
            .await
            .is_err()
    );
    assert!(client.with_target(Some("../bird".into())).is_err());
    assert!(
        bird.metadata
            .0
            .lock()
            .unwrap()
            .calls
            .iter()
            .all(|c| !c.starts_with("unexpected:"))
    );
    server.stop().await.unwrap();
    bird.stop();
}
#[tokio::test]
async fn untargeted_metadata_without_discovery_stays_unsupported() {
    let app = Fixture::new(ServiceId::new("no-discovery-test").unwrap());
    let token = Token::generate().unwrap();
    let server = Server::new(Arc::new(app), token.clone())
        .unwrap()
        .listen("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let response = reqwest::Client::new()
        .get(format!("{}/api/v1/models", server.endpoint()))
        .bearer_auth(token.expose())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
    let error: serde_json::Value = response.json().await.unwrap();
    assert_eq!(error["code"], "unsupported_capability");
    server.stop().await.unwrap();
}
#[test]
fn configuration_is_bounded_and_rejects_ambiguous_or_unsupported_targets() {
    let valid = InferenceTargets {
        schema_version: 1,
        targets: vec![target("bird", "http://127.0.0.1:11434")],
    };
    playtime_server::targets::validate(&valid).unwrap();
    let mut bad = valid.clone();
    bad.targets.push(bad.targets[0].clone());
    assert!(playtime_server::targets::validate(&bad).is_err());
    for endpoint in [
        "https://localhost:11434",
        "http://u:p@localhost",
        "http://localhost/?token=x",
        "http://localhost/#x",
    ] {
        let mut bad = valid.clone();
        bad.targets[0].endpoint = endpoint.into();
        assert!(
            playtime_server::targets::validate(&bad).is_err(),
            "{endpoint}"
        );
    }
    let mut bad = serde_json::to_value(&valid).unwrap();
    bad["targets"][0]["provider"] = "invented".into();
    assert!(serde_json::from_value::<InferenceTargets>(bad).is_err());
    let mut bad = valid;
    bad.schema_version = 2;
    assert!(!bad.valid());
}
#[test]
fn a_strata_target_names_a_launcher_profile_and_no_endpoint() {
    let file: InferenceTargets = serde_json::from_value(serde_json::json!({
        "schema_version": 1,
        "targets": [
            {"id": "bird", "name": "Bird", "provider": "ollama", "endpoint": "http://127.0.0.1:11434"},
            {"id": "framework-strata", "name": "Framework · Strata", "provider": "strata",
             "profile": "framework-gyro-m-fast"},
        ],
    }))
    .unwrap();
    playtime_server::targets::validate(&file).unwrap();
    let strata = &file.targets[1];
    assert_eq!(strata.provider, InferenceProvider::Strata);
    assert_eq!(strata.endpoint, "");
    // It serializes as written, and an Ollama target keeps its shape.
    let written = serde_json::to_value(&file).unwrap();
    assert_eq!(
        written["targets"][1],
        serde_json::json!({"id": "framework-strata", "name": "Framework · Strata",
            "provider": "strata", "profile": "framework-gyro-m-fast"})
    );
    assert!(written["targets"][0].get("profile").is_none());
    for (index, field, value) in [
        (1, "endpoint", serde_json::json!("http://127.0.0.1:8080")),
        (1, "profile", serde_json::Value::Null),
        (1, "profile", serde_json::json!("")),
        (1, "profile", serde_json::json!("../framework")),
        (1, "profile", serde_json::json!("x".repeat(65))),
        (0, "profile", serde_json::json!("framework-gyro-m-fast")),
    ] {
        let mut bad = written.clone();
        bad["targets"][index][field] = value.clone();
        let bad: InferenceTargets = serde_json::from_value(bad).unwrap();
        assert!(
            playtime_server::targets::validate(&bad).is_err(),
            "{index} {field} {value}"
        );
    }
    let mut unknown = written;
    unknown["targets"][1]["launcher"] = "/opt/launcher".into();
    assert!(serde_json::from_value::<InferenceTargets>(unknown).is_err());
}
