mod common;
#[path = "support/model_scenes.rs"]
#[allow(dead_code)]
mod model_scenes;
use playtime_client::{Client, config::Options, registrations::ModelRegistrationStore};
use playtime_protocol::api::*;
use playtime_server::{
    fixture::Fixture, http::Server, lifecycle::Ledger, models::ModelBrowser,
    registrations::Registry,
};
use playtime_test_support::{TestRoot, model_metadata::FakeOllama};
use playtime_ui::{backend::Backend, model::*, models, registrations};
use std::{sync::Arc, time::Duration};

async fn until(m: &mut Model, b: &mut Backend, done: impl Fn(&Model) -> bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            b.pump(m);
            if done(m) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "{} {:?} {}",
            m.notice, m.registrations.mutation, m.registrations.notice
        )
    });
}
#[tokio::test]
async fn workbench_registers_only_explicitly_and_recovers_the_original_receipt() {
    let root = TestRoot::new().unwrap();
    let fake = FakeOllama::start().await;
    let ledger = Ledger::open(&root.0).await.unwrap();
    let service = ServiceId::new(ledger.service_id().as_str()).unwrap();
    let registry = Registry::new(
        ledger.clone(),
        Some(Arc::new(ModelBrowser::new(&fake.endpoint).unwrap())),
    );
    let fixture = Arc::new(Fixture::new(service.clone()).with_model_registry(registry.clone()));
    let credentials = playtime_local::Credentials::initialize(&root.join("credentials")).unwrap();
    let server = Server::new(fixture, credentials.token.clone())
        .unwrap()
        .listen("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    credentials
        .publish(&Connection {
            schema_version: 1,
            service_id: service,
            endpoint: server.endpoint(),
            token_file: credentials.token_path.to_string_lossy().into(),
        })
        .unwrap();
    let options = Options {
        connection_file: Some(credentials.connection_path.clone()),
        state_dir: Some(root.join("state")),
        ..Default::default()
    };
    let mut b = Backend::start(egui::Context::default());
    let mut m = Model::default();
    m.connect(options.clone());
    until(&mut m, &mut b, |m| {
        m.registrations.receipts_ready && !m.recipes.is_empty()
    })
    .await;
    let recipe = m.recipe.clone();
    m.browse_models();
    until(&mut m, &mut b, |m| {
        m.models.inventory.is_some() && m.registrations.receipts_ready
    })
    .await;
    m.inspect_model(ModelInspectRequest {
        name: "gpt-oss:20b".into(),
        expected_digest: None,
    });
    until(&mut m, &mut b, |m| m.models.inspection.is_some()).await;
    assert!(registry.list().await.unwrap().items.is_empty());
    fake.metadata.0.lock().unwrap().delay = Duration::from_millis(100);
    m.register_inspected_model();
    m.register_inspected_model();
    assert_eq!(
        m.commands
            .iter()
            .filter(|c| matches!(c.action, Action::RegisterModel(_)))
            .count(),
        1
    );
    // Closing the metadata browser does not abort the durable mutation.
    m.close_models();
    until(&mut m, &mut b, |m| {
        matches!(m.registrations.mutation, Mutation::Accepted)
            && m.registrations.receipts_ready
            && m.registrations.items.len() == 1
    })
    .await;
    assert_eq!(m.recipe, recipe);
    let original = m.registrations.items[0].clone();
    // Reconnect while the old backend job still owns an unacknowledged request.
    let request = registry
        .review(ModelInspectRequest {
            name: "gpt-oss:20b".into(),
            expected_digest: None,
        })
        .await
        .unwrap()
        .request
        .unwrap();
    m.command(Action::RegisterModel(request));
    until(&mut m, &mut b, |m| {
        matches!(m.registrations.mutation, Mutation::Submitting)
    })
    .await;
    m.connect(options.clone());
    until(&mut m, &mut b, |m| {
        m.registrations.receipt_error.contains("still in progress")
    })
    .await;
    assert!(!m.registrations.receipts_ready);
    let receipts = ModelRegistrationStore::new(&root.join("state"));
    until(&mut m, &mut b, |_| receipts.list().unwrap().is_empty()).await;
    m.command(Action::ModelRegistrationReceipts);
    until(&mut m, &mut b, |m| m.registrations.receipts_ready).await;
    assert!(matches!(m.registrations.mutation, Mutation::Idle));
    assert!(m.registrations.receipt_error.is_empty());
    assert_eq!(registry.list().await.unwrap().items.len(), 1);
    // A failed private write must refuse the request before contacting Ollama.
    let request = registry
        .review(ModelInspectRequest {
            name: "gpt-oss:20b".into(),
            expected_digest: None,
        })
        .await
        .unwrap()
        .request
        .unwrap();
    let calls = fake.metadata.0.lock().unwrap().calls.len();
    let directory = root.join("state/model-registrations");
    std::fs::rename(&directory, directory.with_extension("held")).unwrap();
    std::fs::write(&directory, b"not a directory").unwrap();
    m.command(Action::RegisterModel(request));
    until(&mut m, &mut b, |m| {
        matches!(m.registrations.mutation, Mutation::Rejected(_))
    })
    .await;
    assert_eq!(fake.metadata.0.lock().unwrap().calls.len(), calls);
    std::fs::remove_file(&directory).unwrap();
    std::fs::rename(directory.with_extension("held"), directory).unwrap();
    drop(b);
    // Commit from another workbench without acknowledging locally, then restart ours.
    let client = Client::from_connection(&credentials.connection_path)
        .await
        .unwrap();
    let request = client
        .review_model(&ModelInspectRequest {
            name: "alias:latest".into(),
            expected_digest: None,
        })
        .await
        .unwrap()
        .request
        .unwrap();
    let pending = client.prepare_model_registration(request).unwrap();
    let store = ModelRegistrationStore::new(&root.join("state"));
    store.save(&pending, client.endpoint()).unwrap();
    let accepted = client.register_model(&pending).await.unwrap();
    fake.stop();
    let mut b = Backend::start(egui::Context::default());
    let mut m = Model::default();
    m.connect(options.clone());
    until(&mut m, &mut b, |m| !m.registrations.receipts.is_empty()).await;
    m.command(Action::RecoverModelRegistration(pending.key, false));
    until(&mut m, &mut b, |m| {
        matches!(m.registrations.mutation, Mutation::Accepted)
            && m.registrations.receipts_ready
            && m.registrations.receipts.is_empty()
    })
    .await;
    m.select_registration(accepted.registration.registration_id.clone());
    until(&mut m, &mut b, |m| m.registrations.definition.is_some()).await;
    assert_eq!(
        m.registrations.definition.as_ref().unwrap().value.summary,
        accepted.registration
    );
    assert_ne!(
        original.registration_id,
        accepted.registration.registration_id
    );
    drop(b);
    server.stop().await.unwrap();
    // Same ledger, no configured Ollama endpoint: registered records still have a UI.
    let offline = Arc::new(
        Fixture::new(client.service_id().clone()).with_model_registry(Registry::new(ledger, None)),
    );
    let server = Server::new(offline, credentials.token.clone())
        .unwrap()
        .listen("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    credentials
        .publish(&Connection {
            schema_version: 1,
            service_id: client.service_id().clone(),
            endpoint: server.endpoint(),
            token_file: credentials.token_path.to_string_lossy().into(),
        })
        .unwrap();
    let mut b = Backend::start(egui::Context::default());
    let mut m = Model::default();
    m.connect(options);
    until(&mut m, &mut b, |m| m.registrations.enabled).await;
    assert!(!m.models.enabled);
    m.browse_models();
    until(&mut m, &mut b, |m| m.registrations.items.len() == 2).await;
    assert!(m.models.open && m.registrations.show_saved);
    m.select_registration(original.registration_id);
    until(&mut m, &mut b, |m| m.registrations.definition.is_some()).await;
    drop(b);
    server.stop().await.unwrap();
}

#[tokio::test]
async fn stale_reads_and_receipt_scans_cannot_change_current_registration_or_draft() {
    let mut app = common::recipe_editor().await;
    let m = &mut app.model;
    let draft = m.authoring.draft.as_ref().unwrap().request().unwrap();
    let recipe = m.recipe.clone();
    m.models.enabled = true;
    m.registrations.enabled = true;
    m.registrations.receipts_ready = true;
    m.browse_models();
    let mut view = models::inspection(model_scenes::inspection("gpt-oss:20b"));
    view.review = Some(Box::new(ModelRegistrationRequest {
        endpoint_binding: "a".repeat(64),
        identity: RegisteredModelIdentity::from_inspection(&view.value).unwrap(),
    }));
    m.models.inspection = Some(view);
    m.commands.clear();
    m.registrations.receipts_ready = true;
    m.register_inspected_model();
    let submitted = m.tag;
    m.refresh_models();
    m.close_models();
    assert!(m.registrations.busy());
    m.apply(Reply {
        tag: submitted,
        run: None,
        data: Data::ModelRegistrationProgress(Mutation::Unresolved("reply lost".into())),
    });
    m.apply(Reply {
        tag: submitted,
        run: None,
        data: Data::ModelRegistrationReceipts(vec![]),
    });
    assert!(matches!(m.registrations.mutation, Mutation::Unresolved(_)));
    assert!(!m.registrations.receipts_ready);
    let old = m.tag;
    m.select_registration(ModelRegistrationId::new("second").unwrap());
    let identity =
        RegisteredModelIdentity::from_inspection(&model_scenes::inspection("first")).unwrap();
    m.apply(Reply {
        tag: old,
        run: None,
        data: Data::ModelRegistration(registrations::prepare(
            Arc::new(RegisteredModel {
                schema_version: 1,
                summary: ModelRegistrationSummary {
                    registration_id: ModelRegistrationId::new("first").unwrap(),
                    identity,
                },
                definition: serde_json::json!({}),
            }),
            0,
        )),
    });
    assert!(m.registrations.definition.is_none());
    assert_eq!(
        m.authoring.draft.as_ref().unwrap().request().unwrap(),
        draft
    );
    assert_eq!(m.recipe, recipe);
    m.connect(Default::default());
    m.apply(Reply {
        tag: old,
        run: None,
        data: Data::ModelRegistrationReceipts(vec![]),
    });
    assert!(!m.registrations.receipts_ready);
}
