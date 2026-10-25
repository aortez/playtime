mod common;
use playtime_client::{Client, registrations::ModelRegistrationStore};
use playtime_ollama::Profile;
use playtime_protocol::api::*;
use playtime_server::{
    auth::Token, fixture::Fixture, http::Server, lifecycle::Ledger, models::ModelBrowser,
    registrations::Registry,
};
use playtime_test_support::model_metadata::FakeOllama;
use serde_json::json;
use std::{sync::Arc, time::Duration};

fn key(s: &str) -> IdempotencyKey {
    IdempotencyKey::new(s).unwrap()
}
fn inspect(name: &str) -> ModelInspectRequest {
    ModelInspectRequest {
        name: name.into(),
        expected_digest: None,
    }
}
async fn setup(root: &common::Temp, fake: &FakeOllama) -> (Ledger, Registry) {
    let ledger = Ledger::open(&root.0).await.unwrap();
    let registry = Registry::new(
        ledger.clone(),
        Some(Arc::new(ModelBrowser::new(&fake.endpoint).unwrap())),
    );
    (ledger, registry)
}

#[tokio::test]
async fn explicit_acceptance_deduplicates_complete_identity_and_replays_offline_after_restart() {
    let root = common::Temp::new();
    let fake = FakeOllama::start().await;
    let (ledger, registry) = setup(&root, &fake).await;
    let service = ServiceId::new(ledger.service_id().as_str()).unwrap();
    let fixture = Arc::new(Fixture::new(service.clone()).with_model_registry(registry.clone()));
    let token = Token::generate().unwrap();
    let server = Server::new(fixture.clone(), token.clone())
        .unwrap()
        .listen("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let client = Client::connect(&server.endpoint(), token, None)
        .await
        .unwrap();
    let review = client.review_model(&inspect(Profile::MODEL)).await.unwrap();
    assert!(client.registered_models().await.unwrap().items.is_empty());
    let pending = client
        .prepare_model_registration(review.request.unwrap())
        .unwrap();
    let store = ModelRegistrationStore::new(&root.0.join("client"));
    let receipt = store.save(&pending, client.endpoint()).unwrap();
    let accepted = client.register_model(&pending).await.unwrap();
    assert!(!accepted.replayed);
    let value = client
        .registered_model(&accepted.registration.registration_id)
        .await
        .unwrap();
    assert_eq!(
        value.definition["template"],
        fake.metadata.0.lock().unwrap().show["template"]
    );
    assert!(value.definition.get("modelfile").is_none());
    // A new key accepts the same full identity without manufacturing another record.
    let duplicate = client
        .prepare_model_registration(pending.request.clone())
        .unwrap();
    assert_eq!(
        client
            .register_model(&duplicate)
            .await
            .unwrap()
            .registration,
        accepted.registration
    );
    assert_eq!(client.registered_models().await.unwrap().items.len(), 1);
    let mut conflict = pending.clone();
    conflict.request.identity.name = "alias:latest".into();
    assert_eq!(
        client
            .register_model(&conflict)
            .await
            .unwrap_err()
            .api_code(),
        Some("idempotency_conflict")
    );
    // Treat the successful response as lost. Recovery must use the original disk receipt.
    fake.stop();
    let recovered = client
        .lookup_model_registration(&store.read(&pending.key).unwrap().unwrap().pending)
        .await
        .unwrap();
    assert_eq!(recovered.registration, accepted.registration);
    assert!(client.register_model(&pending).await.unwrap().replayed);
    store.acknowledge(&receipt, &recovered).unwrap();
    assert!(store.list().unwrap().is_empty());
    assert!(store.discard(&receipt).is_err());
    assert!(
        receipt
            .check_target(&ServiceId::new("other").unwrap(), client.endpoint())
            .is_err()
    );
    assert!(
        receipt
            .check_target(&service, "http://127.0.0.1:1")
            .is_err()
    );
    assert!(
        fixture
            .calls()
            .iter()
            .all(|c| !matches!(c.operation, "admit" | "cancel" | "recipe_save"))
    );
    assert!(fake.metadata.0.lock().unwrap().calls.iter().all(|call| {
        call == "version" || call == "tags" || call == "ps" || call.starts_with("show:")
    }));
    server.stop().await.unwrap();
    drop(client);
    drop(fixture);
    drop(registry);
    drop(ledger);
    let ledger = Ledger::open(&root.0).await.unwrap();
    assert_eq!(ledger.service_id().as_str(), service.as_str());
    let offline = Registry::new(ledger, None);
    offline.check().await.unwrap();
    assert_eq!(
        offline
            .get(value.summary.registration_id.clone())
            .await
            .unwrap(),
        value
    );
    assert!(
        offline
            .register(pending.key, pending.request.clone())
            .await
            .unwrap()
            .replayed
    );
    assert_eq!(
        offline
            .register(key("fresh-offline"), pending.request)
            .await
            .unwrap_err()
            .code(),
        "backend_unavailable"
    );
}

#[tokio::test]
async fn registration_reinspects_drift_and_preserves_aliases_and_revised_definitions() {
    let root = common::Temp::new();
    let fake = FakeOllama::start().await;
    let (ledger, registry) = setup(&root, &fake).await;
    let original = registry
        .review(inspect(Profile::MODEL))
        .await
        .unwrap()
        .request
        .unwrap();
    let first = registry
        .register(key("first"), original.clone())
        .await
        .unwrap();
    let show = fake.metadata.0.lock().unwrap().show.clone();
    let tags = fake.metadata.0.lock().unwrap().tags.clone();
    for field in [
        "digest",
        "missing",
        "remote",
        "template",
        "capabilities",
        "version",
        "endpoint",
    ] {
        let mut request = original.clone();
        {
            let mut s = fake.metadata.0.lock().unwrap();
            match field {
                "digest" => s.tags["models"][0]["digest"] = json!("b".repeat(64)),
                "missing" => s.tags = json!({"models":[]}),
                "remote" => s.show["remote_host"] = json!("https://remote.invalid"),
                "template" => s.show["template"] = json!("changed"),
                "capabilities" => s.show["capabilities"] = json!(["completion"]),
                "version" => s.version = "unreviewed".into(),
                "endpoint" => request.endpoint_binding = "f".repeat(64),
                _ => unreachable!(),
            }
        }
        assert_eq!(
            registry
                .register(key(field), request)
                .await
                .unwrap_err()
                .code(),
            "model_metadata_changed",
            "{field}"
        );
        assert_eq!(
            registry.lookup(key(field)).await.unwrap_err().code(),
            "not_found"
        );
        let mut s = fake.metadata.0.lock().unwrap();
        s.show = show.clone();
        s.tags = tags.clone();
        s.version = Profile::VERSION.into();
    }
    // Loaded state and generated local blob paths are deliberately not identity.
    {
        let mut s = fake.metadata.0.lock().unwrap();
        s.loaded = tags;
        s.show["modelfile"] = json!("FROM /new/local/path");
    }
    assert_eq!(
        registry
            .register(key("loaded"), original.clone())
            .await
            .unwrap()
            .registration,
        first.registration
    );
    let alias = registry
        .review(inspect("alias:latest"))
        .await
        .unwrap()
        .request
        .unwrap();
    assert!(alias.identity.reviewed_capabilities.is_none());
    let alias = registry.register(key("alias"), alias).await.unwrap();
    assert_ne!(
        alias.registration.registration_id,
        first.registration.registration_id
    );
    fake.metadata.0.lock().unwrap().show["template"] = json!("new canonical definition");
    let changed = registry
        .review(inspect(Profile::MODEL))
        .await
        .unwrap()
        .request
        .unwrap();
    let changed = registry.register(key("changed"), changed).await.unwrap();
    assert_ne!(
        changed.registration.registration_id,
        first.registration.registration_id
    );
    fake.metadata.0.lock().unwrap().show["capabilities"] = json!(["completion"]);
    let incompatible = registry
        .review(inspect(Profile::MODEL))
        .await
        .unwrap()
        .request
        .unwrap();
    assert!(incompatible.identity.reviewed_capabilities.is_none());
    let incompatible = registry
        .register(key("incompatible"), incompatible)
        .await
        .unwrap();
    assert_ne!(
        incompatible.registration.registration_id,
        changed.registration.registration_id
    );
    assert_eq!(registry.list().await.unwrap().items.len(), 4);
    assert_eq!(
        registry
            .get(first.registration.registration_id)
            .await
            .unwrap()
            .definition,
        serde_json::from_str::<serde_json::Value>(include_str!(
            "../../../models/gpt-oss-20b.lock.json"
        ))
        .unwrap()["definition"]
    );
    // An unreviewed request cannot cross either endpoint or service boundaries.
    let another = FakeOllama::start().await;
    let other_endpoint = Registry::new(
        ledger,
        Some(Arc::new(ModelBrowser::new(&another.endpoint).unwrap())),
    );
    assert_eq!(
        other_endpoint
            .register(key("other-endpoint"), original.clone())
            .await
            .unwrap_err()
            .code(),
        "model_metadata_changed"
    );
    let other_root = common::Temp::new();
    let (_, other_service) = setup(&other_root, &fake).await;
    assert_eq!(
        other_service
            .register(key("other-service"), original)
            .await
            .unwrap_err()
            .code(),
        "model_metadata_changed"
    );
}

#[tokio::test]
async fn concurrent_acceptance_is_bounded_and_retry_uses_the_original_key() {
    let root = common::Temp::new();
    let fake = FakeOllama::start().await;
    let (_, registry) = setup(&root, &fake).await;
    let request = registry
        .review(inspect(Profile::MODEL))
        .await
        .unwrap()
        .request
        .unwrap();
    let calls = fake.metadata.0.lock().unwrap().calls.len();
    fake.metadata.0.lock().unwrap().delay = Duration::from_millis(100);
    let task = tokio::spawn({
        let r = registry.clone();
        let q = request.clone();
        async move { r.register(key("same"), q).await }
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        while fake.metadata.0.lock().unwrap().calls.len() == calls {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        registry
            .register(key("same"), request.clone())
            .await
            .unwrap_err()
            .code(),
        "model_registration_busy"
    );
    let result = task.await.unwrap().unwrap();
    assert_eq!(
        registry
            .register(key("same"), request)
            .await
            .unwrap()
            .registration,
        result.registration
    );
    assert_eq!(registry.list().await.unwrap().items.len(), 1);
}

#[tokio::test]
async fn metadata_limits_and_corrupt_storage_fail_without_deleting_records() {
    let root = common::Temp::new();
    let fake = FakeOllama::start().await;
    let (_, registry) = setup(&root, &fake).await;
    let request = registry
        .review(inspect(Profile::MODEL))
        .await
        .unwrap()
        .request
        .unwrap();
    let first = registry
        .register(key("first"), request.clone())
        .await
        .unwrap();
    let original_show = fake.metadata.0.lock().unwrap().show.clone();
    fake.metadata.0.lock().unwrap().show["template"] =
        json!("x".repeat(MODEL_DEFINITION_BYTES + 1));
    assert_eq!(
        registry
            .register(
                key("large"),
                registry
                    .review(inspect(Profile::MODEL))
                    .await
                    .unwrap()
                    .request
                    .unwrap()
            )
            .await
            .unwrap_err()
            .code(),
        "limit_exceeded"
    );
    assert_eq!(registry.list().await.unwrap().items.len(), 1);
    let db = rusqlite::Connection::open(root.0.join(".playtime/service.sqlite3")).unwrap();
    fake.metadata.0.lock().unwrap().show = original_show.clone();
    // Ordinary storage contention is unavailable, not a corruption diagnosis.
    db.execute_batch("BEGIN IMMEDIATE").unwrap();
    assert_eq!(
        registry
            .register(key("storage-busy"), request.clone())
            .await
            .unwrap_err()
            .code(),
        "backend_unavailable"
    );
    db.execute_batch("ROLLBACK").unwrap();
    assert_eq!(
        registry
            .register(key("storage-busy"), request)
            .await
            .unwrap()
            .registration,
        first.registration
    );
    fake.metadata.0.lock().unwrap().show["capabilities"] = json!(vec!["\"".repeat(128); 64]);
    let oversized_request = registry
        .review(inspect(Profile::MODEL))
        .await
        .unwrap()
        .request
        .unwrap();
    assert_eq!(
        registry
            .register(key("oversized-request"), oversized_request)
            .await
            .unwrap_err()
            .code(),
        "limit_exceeded"
    );
    db.execute("UPDATE model_registrations SET record='{}'", [])
        .unwrap();
    assert_eq!(
        registry.check().await.unwrap_err().code(),
        "model_registry_corrupt"
    );
    assert_eq!(
        registry
            .get(first.registration.registration_id)
            .await
            .unwrap_err()
            .code(),
        "model_registry_corrupt"
    );
    assert_eq!(
        db.query_row("SELECT count(*) FROM model_registrations", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn client_disconnect_after_commit_retains_the_original_acceptance() {
    let root = common::Temp::new();
    let fake = FakeOllama::start().await;
    let (ledger, registry) = setup(&root, &fake).await;
    let service = ServiceId::new(ledger.service_id().as_str()).unwrap();
    let fixture = Arc::new(Fixture::new(service.clone()).with_model_registry(registry.clone()));
    let mut faults = common::faults::Faults::new(fixture);
    faults.admission_delay = Duration::from_millis(300);
    let token = Token::generate().unwrap();
    let server = Server::new(Arc::new(faults), token.clone())
        .unwrap()
        .listen("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let client = Client::connect(&server.endpoint(), token.clone(), None)
        .await
        .unwrap();
    let request = client
        .review_model(&inspect(Profile::MODEL))
        .await
        .unwrap()
        .request
        .unwrap();
    let pending = client.prepare_model_registration(request).unwrap();
    let store = ModelRegistrationStore::new(&root.0.join("client"));
    let receipt = store.save(&pending, client.endpoint()).unwrap();
    let response = reqwest::Client::new()
        .post(format!("{}/api/v1/models/registrations", server.endpoint()))
        .bearer_auth(token.expose())
        .header(SERVICE_HEADER, service.as_str())
        .header("idempotency-key", pending.key.as_str())
        .json(&pending.request)
        .timeout(Duration::from_millis(100))
        .send()
        .await;
    assert!(response.unwrap_err().is_timeout());
    let response = client.lookup_model_registration(&pending).await.unwrap();
    // Local acknowledgment failure must leave the original request recoverable.
    let directory = root.0.join("client/model-registrations");
    std::fs::rename(&directory, directory.with_extension("held")).unwrap();
    assert!(store.acknowledge(&receipt, &response).is_err());
    std::fs::rename(directory.with_extension("held"), &directory).unwrap();
    assert_eq!(store.list().unwrap()[0].pending, pending);
    fake.stop();
    let replay = client.register_model(&pending).await.unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.registration, response.registration);
    store.acknowledge(&receipt, &replay).unwrap();
    assert!(store.list().unwrap().is_empty());
    assert_eq!(registry.list().await.unwrap().items.len(), 1);
    server.stop().await.unwrap();
}

#[tokio::test]
async fn finite_record_and_key_budgets_refuse_new_acceptance_but_keep_replays() {
    for record_limit in [true, false] {
        let root = common::Temp::new();
        let fake = FakeOllama::start().await;
        let (_, registry) = setup(&root, &fake).await;
        let request = registry
            .review(inspect(Profile::MODEL))
            .await
            .unwrap()
            .request
            .unwrap();
        let first = registry
            .register(key("first"), request.clone())
            .await
            .unwrap();
        let original = registry
            .get(first.registration.registration_id.clone())
            .await
            .unwrap();
        let mut db = rusqlite::Connection::open(root.0.join(".playtime/service.sqlite3")).unwrap();
        let tx = db.transaction().unwrap();
        if record_limit {
            for i in 1..MODEL_REGISTRATION_LIMIT {
                let mut value = original.clone();
                value.summary.registration_id =
                    ModelRegistrationId::new(format!("model-{i}")).unwrap();
                value.summary.identity.name = format!("name:{i}");
                let identity = playtime_ollama::json::canonical(
                    &serde_json::to_value(&value.summary.identity).unwrap(),
                    12 << 10,
                )
                .unwrap();
                let record = serde_json::to_string(&value).unwrap();
                let request = ModelRegistrationRequest {
                    endpoint_binding: request.endpoint_binding.clone(),
                    identity: value.summary.identity,
                };
                tx.execute("INSERT INTO model_registrations(id,identity_hash,digest,record) VALUES(?1,?2,?3,?4)",rusqlite::params![value.summary.registration_id.as_str(),playtime_ollama::json::hash(&identity),playtime_ollama::json::hash(record.as_bytes()),record]).unwrap();
                tx.execute(
                    "INSERT INTO model_acceptances VALUES(?1,?2,?3)",
                    rusqlite::params![
                        format!("key-{i}"),
                        serde_json::to_string(&request).unwrap(),
                        value.summary.registration_id.as_str()
                    ],
                )
                .unwrap();
            }
        } else {
            for i in 1..4096 {
                tx.execute(
                    "INSERT INTO model_acceptances VALUES(?1,?2,?3)",
                    rusqlite::params![
                        format!("key-{i}"),
                        serde_json::to_string(&request).unwrap(),
                        first.registration.registration_id.as_str()
                    ],
                )
                .unwrap();
            }
        }
        tx.commit().unwrap();
        registry.check().await.unwrap();
        assert_eq!(
            registry.list().await.unwrap().items.len(),
            if record_limit {
                MODEL_REGISTRATION_LIMIT
            } else {
                1
            }
        );
        let extra = registry
            .review(inspect("alias:latest"))
            .await
            .unwrap()
            .request
            .unwrap();
        assert_eq!(
            registry
                .register(key("extra"), extra)
                .await
                .unwrap_err()
                .code(),
            "model_registry_full"
        );
        assert_eq!(
            registry.lookup(key("extra")).await.unwrap_err().code(),
            "not_found"
        );
        assert!(
            registry
                .register(key("first"), request)
                .await
                .unwrap()
                .replayed
        );
    }
}

#[tokio::test]
async fn ledger_seven_adds_empty_registry_without_rewriting_saved_recipes() {
    let root = common::Temp::new();
    let ledger = Ledger::open(&root.0).await.unwrap();
    let service = ledger.service_id().clone();
    let recipes = playtime_server::daemon::catalog::load(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../recipes"),
    )
    .unwrap();
    let catalog = playtime_server::catalog::Catalog::new(ledger.clone(), recipes).unwrap();
    let source = catalog
        .definition(&RecipeId::new("curiosity").unwrap())
        .await
        .unwrap();
    let saved = catalog
        .save(
            key("saved"),
            RecipeMutation::Clone {
                request: CloneRecipeRequest {
                    harness: None,
                    model_registration: None,
                    source: RecipeSource {
                        execution_recipe_id: source.execution_recipe_id,
                        definition_digest: source.definition_digest,
                    },
                    name: "Preserved".into(),
                    prompt: source.prompt,
                    model_settings: None,
                },
            },
        )
        .await
        .unwrap();
    drop(catalog);
    drop(ledger);
    let db = rusqlite::Connection::open(root.0.join(".playtime/service.sqlite3")).unwrap();
    db.execute_batch(
        "DROP TABLE model_acceptances; DROP TABLE model_registrations; PRAGMA user_version=6;",
    )
    .unwrap();
    drop(db);
    let ledger = Ledger::open(&root.0).await.unwrap();
    assert_eq!(ledger.service_id(), &service);
    assert!(
        Registry::new(ledger.clone(), None)
            .list()
            .await
            .unwrap()
            .items
            .is_empty()
    );
    let catalog = playtime_server::catalog::Catalog::new(ledger, vec![]).unwrap();
    assert_eq!(
        catalog
            .definition(&saved.definition.execution_recipe_id)
            .await
            .unwrap(),
        saved.definition
    );
    let db = rusqlite::Connection::open(root.0.join(".playtime/service.sqlite3")).unwrap();
    assert_eq!(
        db.query_row("PRAGMA user_version", [], |r| r.get::<_, u32>(0))
            .unwrap(),
        7
    );
}
