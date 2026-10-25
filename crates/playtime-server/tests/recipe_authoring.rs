mod common;
use common::Temp;
use playtime_client::{Client, recipes::RecipeSaveStore};
use playtime_core::RunMode;
use playtime_protocol::api::*;
use playtime_runner::docker::Recipe as RunRecipe;
use playtime_server::{
    auth::Token, catalog::Catalog, daemon::catalog::load, fixture::Fixture, http::Server,
    lifecycle::Ledger,
};
use std::{collections::BTreeSet, path::Path, sync::Arc};

fn source_recipe() -> RunRecipe {
    load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../recipes"))
        .unwrap()
        .into_iter()
        .find(|r| r.id == "curiosity")
        .unwrap()
}
fn clone_request(definition: &RecipeDefinition, name: &str) -> RecipeMutation {
    RecipeMutation::Clone {
        request: CloneRecipeRequest {
            harness: None,
            model_registration: None,
            model_settings: None,
            source: RecipeSource {
                execution_recipe_id: definition.execution_recipe_id.clone(),
                definition_digest: definition.definition_digest.clone(),
            },
            name: name.into(),
            prompt: format!("Prompt for {name}"),
        },
    }
}
fn revise(definition: &RecipeDefinition, name: &str) -> RecipeMutation {
    RecipeMutation::Revise {
        recipe_id: definition.saved.as_ref().unwrap().recipe_id.clone(),
        request: ReviseRecipeRequest {
            harness: None,
            model_registration: None,
            model_settings: None,
            expected_head: definition.execution_recipe_id.clone(),
            name: name.into(),
            prompt: format!("Prompt for {name}"),
        },
    }
}
fn key(value: &str) -> IdempotencyKey {
    IdempotencyKey::new(value).unwrap()
}
async fn setup(root: &Temp) -> (Ledger, Catalog, RecipeDefinition) {
    let ledger = Ledger::open(&root.0).await.unwrap();
    let catalog = Catalog::new(ledger.clone(), vec![source_recipe()]).unwrap();
    catalog.check().await.unwrap();
    let definition = catalog
        .definition(&RecipeId::new("curiosity").unwrap())
        .await
        .unwrap();
    (ledger, catalog, definition)
}
#[tokio::test]
async fn harness_selection_is_exact_inherited_and_replayable_without_execution() {
    let root = Temp::new();
    let (_, catalog, source) = setup(&root).await;
    let request = clone_request(&source, "legacy client");
    let json = serde_json::to_value(&request).unwrap();
    assert!(json["request"].get("harness").is_none());
    let restored = serde_json::from_value(json).unwrap();
    let a = catalog
        .save(key("legacy"), restored)
        .await
        .unwrap()
        .definition;
    assert_eq!(a.inherited["harness"], source.inherited["harness"]);
    let expected = catalog.harness(&a.execution_recipe_id).await.unwrap();
    for id in ["unknown_v1", "MEMENTO_TURNS_V1", "memento_turns_v2"] {
        let mut request = revise(&a, "explicit");
        if let RecipeMutation::Revise { request, .. } = &mut request {
            request.harness = Some(HarnessId::new(id).unwrap());
        }
        assert_eq!(
            catalog
                .save(key("selection"), request)
                .await
                .unwrap_err()
                .code(),
            "invalid_parameters"
        );
    }
    assert_eq!(
        catalog
            .head(&a.saved.as_ref().unwrap().recipe_id)
            .await
            .unwrap()
            .revision,
        1
    );
    assert_eq!(
        catalog.lookup(&key("selection")).await.unwrap_err().code(),
        "not_found"
    );
    let mut request = revise(&a, "explicit");
    if let RecipeMutation::Revise { request, .. } = &mut request {
        request.harness = Some(HarnessId::new(MEMENTO_TURNS_V1).unwrap());
    }
    let b = catalog
        .save(key("selection"), request.clone())
        .await
        .unwrap()
        .definition;
    assert_eq!(
        catalog
            .harness(&b.execution_recipe_id)
            .await
            .unwrap()
            .harness,
        expected.harness
    );
    assert_eq!(
        catalog.harness(&a.execution_recipe_id).await.unwrap(),
        expected
    );
    assert!(
        catalog
            .save(key("selection"), request)
            .await
            .unwrap()
            .replayed
    );
    assert!(
        catalog
            .save(key("legacy"), clone_request(&source, "legacy client"))
            .await
            .unwrap()
            .replayed
    );

    let old = common::Harness::new().await;
    let omitted = clone_request(&source, "old service");
    old.client.prepare_recipe_save(omitted.clone()).unwrap();
    let mut explicit = omitted;
    if let RecipeMutation::Clone { request } = &mut explicit {
        request.harness = Some(HarnessId::new(MEMENTO_TURNS_V1).unwrap());
    }
    assert!(matches!(
        old.client.prepare_recipe_save(explicit),
        Err(playtime_client::Error::UnsupportedCapability)
    ));
    old.server.stop().await.unwrap();
}

#[tokio::test]
async fn unknown_retained_harness_remains_readable_and_editable_without_fallback() {
    let root = Temp::new();
    let (_, catalog, source) = setup(&root).await;
    let a = catalog
        .save(key("a"), clone_request(&source, "retained"))
        .await
        .unwrap()
        .definition;
    let (mut frozen, _) = catalog
        .frozen(a.execution_recipe_id.as_str())
        .await
        .unwrap();
    frozen.harness = "future_v2".into();
    let db = rusqlite::Connection::open(root.0.join(".playtime/service.sqlite3")).unwrap();
    // Simulate an immutable definition retained from another build, with its
    // actual digest. This is synthetic historical data, never an executed run.
    db.execute(
        "UPDATE recipe_revisions SET definition=?1,digest=?2 WHERE execution=?3",
        rusqlite::params![
            serde_json::to_string(&frozen).unwrap(),
            frozen.digest().unwrap(),
            a.execution_recipe_id.as_str()
        ],
    )
    .unwrap();
    let retained = catalog.definition(&a.execution_recipe_id).await.unwrap();
    let view = catalog
        .harness(&retained.execution_recipe_id)
        .await
        .unwrap();
    assert_eq!(view.harness.id, "future_v2");
    assert!(!view.harness.supported);
    let edited = catalog
        .save(key("edit"), revise(&retained, "offline edit"))
        .await
        .unwrap()
        .definition;
    let (edited, _) = catalog
        .frozen(edited.execution_recipe_id.as_str())
        .await
        .unwrap();
    assert_eq!(edited.harness, "future_v2");
    assert!(edited.executable().is_err());
}
#[tokio::test]
async fn immutable_revisions_conflicts_replay_and_restart_without_source_catalog() {
    let root = Temp::new();
    let (ledger, catalog, source) = setup(&root).await;
    let request = clone_request(&source, "A");
    let a = catalog.save(key("clone"), request.clone()).await.unwrap();
    let family = &a.definition.saved.as_ref().unwrap().recipe_id;
    let (one, two) = tokio::join!(
        catalog.save(key("one"), revise(&a.definition, "B")),
        catalog.save(key("two"), revise(&a.definition, "C"))
    );
    assert!(one.is_ok() ^ two.is_ok());
    let b = match (one, two) {
        (Ok(b), Err(e)) | (Err(e), Ok(b)) => {
            assert_eq!(e.code(), "head_conflict");
            b
        }
        _ => panic!(),
    };
    assert_eq!(catalog.head(family).await.unwrap().revision, 2);
    assert_eq!(
        catalog
            .definition(&a.definition.execution_recipe_id)
            .await
            .unwrap(),
        a.definition
    );
    assert_eq!(a.definition.seeds, b.definition.seeds);
    let replay = catalog.save(key("clone"), request.clone()).await.unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.definition, a.definition);
    assert_eq!(
        catalog
            .save(key("clone"), clone_request(&source, "different"))
            .await
            .unwrap_err()
            .code(),
        "idempotency_conflict"
    );
    catalog.check().await.unwrap();
    drop(catalog);
    drop(ledger);
    let ledger = Ledger::open(&root.0).await.unwrap();
    let catalog = Catalog::new(ledger, vec![]).unwrap();
    catalog.check().await.unwrap();
    assert_eq!(
        catalog
            .save(key("clone"), request)
            .await
            .unwrap()
            .definition,
        a.definition
    );
    assert_eq!(
        catalog
            .definition(&b.definition.execution_recipe_id)
            .await
            .unwrap(),
        b.definition
    );
    let c = catalog
        .save(key("offline-edit"), revise(&b.definition, "Offline edit"))
        .await
        .unwrap();
    assert_eq!(c.definition.saved.unwrap().revision, 3);
    let copy = catalog
        .save(
            key("offline-clone"),
            clone_request(&a.definition, "Offline clone"),
        )
        .await
        .unwrap();
    assert_eq!(copy.definition.seeds, a.definition.seeds);
}
#[tokio::test]
async fn saved_head_pages_keep_membership_order_and_selected_execution_during_revisions() {
    let root = Temp::new();
    let (ledger, catalog, source) = setup(&root).await;
    let mut originals = vec![];
    for i in 0..5 {
        originals.push(
            catalog
                .save(
                    key(&format!("s{i}")),
                    clone_request(&source, &format!("recipe{i}")),
                )
                .await
                .unwrap()
                .definition,
        );
    }
    let first = catalog
        .page(
            PageQuery {
                limit: Some(2),
                cursor: None,
            },
            None,
            None,
        )
        .await
        .unwrap();
    let cursor = first.next_cursor.clone().unwrap();
    // Revise an already-returned head and the very next unreturned head.
    catalog
        .save(key("revise-first"), revise(&originals[0], "zzz"))
        .await
        .unwrap();
    let next = catalog
        .save(key("revise-next"), revise(&originals[2], "aaa"))
        .await
        .unwrap();
    catalog
        .save(key("new-family"), clone_request(&source, "late"))
        .await
        .unwrap();
    let query = PageQuery {
        limit: Some(2),
        cursor: Some(cursor.clone()),
    };
    let second = catalog.page(query.clone(), None, None).await.unwrap();
    assert_eq!(
        second.items[0].recipe_id,
        next.definition.execution_recipe_id
    );
    assert_eq!(catalog.page(query, None, None).await.unwrap(), second);
    let third = catalog
        .page(
            PageQuery {
                limit: Some(2),
                cursor: second.next_cursor.clone(),
            },
            None,
            None,
        )
        .await
        .unwrap();
    assert!(third.next_cursor.is_none());
    let rows = first
        .items
        .iter()
        .chain(&second.items)
        .chain(&third.items)
        .collect::<Vec<_>>();
    assert_eq!(rows.len(), 5);
    assert_eq!(
        rows.iter()
            .map(|r| r.saved.as_ref().unwrap().recipe_id.clone())
            .collect::<BTreeSet<_>>()
            .len(),
        5
    );
    assert_eq!(first.items[0].recipe_id, originals[0].execution_recipe_id);
    assert_eq!(
        catalog
            .page(PageQuery::default(), None, None)
            .await
            .unwrap()
            .items
            .len(),
        6
    );
    // Saved-only cursors survive a source-catalog change; picker cursors do not.
    let picker = catalog
        .page(
            PageQuery {
                limit: Some(1),
                cursor: None,
            },
            None,
            Some(vec![]),
        )
        .await
        .unwrap();
    let empty = Catalog::new(ledger.clone(), vec![]).unwrap();
    assert!(
        empty
            .page(
                PageQuery {
                    cursor: Some(cursor),
                    limit: Some(1)
                },
                None,
                None
            )
            .await
            .is_ok()
    );
    assert_eq!(
        empty
            .page(
                PageQuery {
                    cursor: picker.next_cursor,
                    limit: Some(1)
                },
                None,
                Some(vec![])
            )
            .await
            .unwrap_err()
            .code(),
        "invalid_cursor"
    );
    let family = originals[2].saved.as_ref().unwrap().recipe_id.clone();
    let history = catalog
        .page(
            PageQuery {
                limit: Some(1),
                cursor: None,
            },
            Some(family.clone()),
            None,
        )
        .await
        .unwrap();
    catalog
        .save(key("third-revision"), revise(&next.definition, "third"))
        .await
        .unwrap();
    let query = PageQuery {
        limit: Some(1),
        cursor: history.next_cursor,
    };
    let history = catalog
        .page(query.clone(), Some(family), None)
        .await
        .unwrap();
    assert_eq!(
        history.items[0].recipe_id,
        next.definition.execution_recipe_id
    );
    assert!(history.next_cursor.is_none());
    assert_eq!(
        catalog
            .page(query.clone(), None, None)
            .await
            .unwrap_err()
            .code(),
        "invalid_cursor"
    );
    let other_root = Temp::new();
    let (_, other, _) = setup(&other_root).await;
    assert_eq!(
        other.page(query, None, None).await.unwrap_err().code(),
        "invalid_cursor"
    );
}
#[tokio::test]
async fn rollback_does_not_move_head_or_consume_key_and_corruption_is_not_unavailability() {
    let root = Temp::new();
    let (ledger, catalog, source) = setup(&root).await;
    let a = catalog
        .save(key("a"), clone_request(&source, "a"))
        .await
        .unwrap();
    let db = rusqlite::Connection::open(root.0.join(".playtime/service.sqlite3")).unwrap();
    db.execute_batch("CREATE TRIGGER fail_save BEFORE INSERT ON recipe_saves BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
    assert!(
        catalog
            .save(key("b"), revise(&a.definition, "b"))
            .await
            .is_err()
    );
    assert_eq!(
        catalog
            .head(&a.definition.saved.as_ref().unwrap().recipe_id)
            .await
            .unwrap()
            .revision,
        1
    );
    assert_eq!(
        catalog.lookup(&key("b")).await.unwrap_err().code(),
        "not_found"
    );
    assert_eq!(
        db.query_row::<i64, _, _>("SELECT count(*) FROM recipe_revisions", [], |r| r.get(0))
            .unwrap(),
        1
    );
    db.execute_batch("DROP TRIGGER fail_save").unwrap();
    catalog
        .save(key("b"), revise(&a.definition, "b"))
        .await
        .unwrap();
    let mut changed = source_recipe();
    changed.prompt = "Changed source".into();
    let changed = Catalog::new(ledger, vec![changed]).unwrap();
    assert_eq!(
        changed
            .save(key("conflict"), clone_request(&source, "clone"))
            .await
            .unwrap_err()
            .code(),
        "source_conflict"
    );
    assert_eq!(
        catalog.lookup(&key("conflict")).await.unwrap_err().code(),
        "not_found"
    );
    changed.check().await.unwrap();
    db.execute(
        "UPDATE recipe_revisions SET definition='{}' WHERE execution=?1",
        [a.definition.execution_recipe_id.as_str()],
    )
    .unwrap();
    assert_eq!(changed.check().await.unwrap_err().code(), "catalog_corrupt");
}
#[tokio::test]
async fn actual_http_client_and_private_receipts_work_without_execution() {
    let root = Temp::new();
    let (ledger, catalog, source) = setup(&root).await;
    let service = ServiceId::new(ledger.service_id().as_str()).unwrap();
    let fixture = Arc::new(Fixture::with_catalog(service.clone(), catalog.clone()));
    let token = Token::generate().unwrap();
    let mut faults = common::faults::Faults::new(fixture);
    faults.admission_delay = std::time::Duration::from_millis(100);
    let server = Server::new(Arc::new(faults), token.clone())
        .unwrap()
        .listen("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let client = Client::connect(&server.endpoint(), token.clone(), Some(service.clone()))
        .await
        .unwrap();
    assert_eq!(
        client
            .recipe_definition(&source.execution_recipe_id)
            .await
            .unwrap(),
        source
    );
    let mut settings = client
        .recipe_model_settings(&source)
        .await
        .unwrap()
        .settings;
    let harnesses = client.harnesses().await.unwrap();
    assert_eq!(harnesses.harnesses.len(), 1);
    assert_eq!(harnesses.harnesses[0].id.as_str(), MEMENTO_TURNS_V1);
    let original_harness = client.recipe_harness(&source).await.unwrap();
    assert_eq!(original_harness.harness.id, MEMENTO_TURNS_V1);
    for case in 0..3 {
        let mut changed = source.clone();
        match case {
            0 => changed.definition_digest = "b".repeat(64),
            1 => changed.inherited["harness"] = serde_json::json!("future_v2"),
            _ => {
                changed.inherited["recipe"]["parameters"]["limits"]["sessions"] =
                    serde_json::json!(1)
            }
        }
        assert!(matches!(
            client.recipe_harness(&changed).await,
            Err(playtime_client::Error::InvalidResponse)
        ));
    }
    settings.temperature = 0.0;
    settings.reasoning = RecipeReasoning::ModelDefault;
    let mut request = clone_request(&source, "client clone");
    if let RecipeMutation::Clone { request } = &mut request {
        request.model_settings = Some(settings.clone());
        request.harness = Some(HarnessId::new(MEMENTO_TURNS_V1).unwrap());
    }
    let pending = client.prepare_recipe_save(request).unwrap();
    let state = Temp::new();
    let store = RecipeSaveStore::new(&state.0);
    let receipt = store.save(&pending, client.endpoint()).unwrap();
    // A request that hasn't been sent remains pending after lookup 404.
    assert_eq!(
        client
            .lookup_recipe_save(&pending)
            .await
            .unwrap_err()
            .api_code(),
        Some("not_found")
    );
    assert_eq!(store.list().unwrap(), vec![receipt.clone()]);
    let saved = client.save_recipe(&pending).await.unwrap();
    // Simulate losing the successful reply before acknowledging it locally.
    drop(client);
    let client = Client::connect(&server.endpoint(), token.clone(), Some(service.clone()))
        .await
        .unwrap();
    let restored = store.read(&pending.key).unwrap().unwrap();
    let recovered = client.lookup_recipe_save(&restored.pending).await.unwrap();
    assert!(recovered.replayed);
    assert_eq!(recovered.definition, saved.definition);
    assert_eq!(
        client
            .save_recipe(&restored.pending)
            .await
            .unwrap()
            .definition,
        saved.definition
    );
    assert_eq!(
        client
            .recipe_model_settings(&recovered.definition)
            .await
            .unwrap()
            .settings,
        settings
    );
    let mut settings_mismatch = recovered.clone();
    let mut harness_mismatch = recovered.clone();
    harness_mismatch.definition.inherited["harness"] = serde_json::json!("future_v2");
    assert!(store.acknowledge(&restored, &harness_mismatch).is_err());
    assert_eq!(
        client
            .recipe_harness(&recovered.definition)
            .await
            .unwrap()
            .harness,
        original_harness.harness
    );
    settings_mismatch.definition.inherited["recipe"]["model"]["temperature"] =
        serde_json::json!(1.0);
    assert!(store.acknowledge(&restored, &settings_mismatch).is_err());
    let mut mismatched = recovered.clone();
    mismatched.definition.prompt = "Different prompt".into();
    assert!(store.acknowledge(&restored, &mismatched).is_err());
    assert_eq!(store.list().unwrap(), vec![restored.clone()]);
    store.acknowledge(&restored, &recovered).unwrap();
    assert!(store.list().unwrap().is_empty());
    let id = &saved.definition.execution_recipe_id;
    assert_eq!(
        client
            .preview(id, RunMode::Experiment)
            .await
            .unwrap_err()
            .api_code(),
        Some("execution_unavailable")
    );
    assert_eq!(
        client.recipe_definition(id).await.unwrap(),
        saved.definition
    );
    let edit = client
        .prepare_recipe_save(revise(&saved.definition, "offline revision"))
        .unwrap();
    let revision = client.save_recipe(&edit).await.unwrap();
    assert_eq!(revision.definition.saved.unwrap().revision, 2);
    // Wire contract: typed 201/200, strict bodies, authentication and service binding.
    let raw = reqwest::Client::new();
    let post = || {
        raw.post(format!("{}/api/v1/saved-recipes", server.endpoint()))
            .bearer_auth(token.expose())
            .header(SERVICE_HEADER, service.as_str())
            .header("Idempotency-Key", "wire")
    };
    let RecipeMutation::Clone { request } = clone_request(&source, "wire clone") else {
        panic!()
    };
    assert_eq!(post().json(&request).send().await.unwrap().status(), 201);
    assert_eq!(post().json(&request).send().await.unwrap().status(), 200);
    let mut invalid = serde_json::to_value(&request).unwrap();
    invalid["limits"] = serde_json::json!({});
    assert_eq!(post().json(&invalid).send().await.unwrap().status(), 400);
    assert_eq!(
        raw.get(format!(
            "{}/api/v1/recipes/{id}/definition",
            server.endpoint()
        ))
        .send()
        .await
        .unwrap()
        .status(),
        401
    );
    let lost = raw
        .post(format!("{}/api/v1/saved-recipes", server.endpoint()))
        .bearer_auth(token.expose())
        .header(SERVICE_HEADER, service.as_str())
        .header("Idempotency-Key", "lost-wire")
        .json(&request)
        .timeout(std::time::Duration::from_millis(30))
        .send()
        .await;
    assert!(lost.unwrap_err().is_timeout());
    let committed = catalog.lookup(&key("lost-wire")).await.unwrap();
    assert!(committed.replayed);
    assert_eq!(committed.definition.prompt, request.prompt);
    // Other services' receipts do not consume this service's recovery page.
    for n in 0..200 {
        let mut foreign = pending.clone();
        foreign.service_id = ServiceId::new("other-service").unwrap();
        foreign.key = key(&format!("foreign-{n:03}"));
        store.save(&foreign, client.endpoint()).unwrap();
    }
    let mut own = pending.clone();
    own.key = key("zz-own");
    let own = store.save(&own, client.endpoint()).unwrap();
    assert_eq!(store.list_service(Some(&service)).unwrap(), vec![own]);
    catalog.check().await.unwrap();
    server.stop().await.unwrap();
}

#[tokio::test]
async fn model_settings_are_atomic_offline_recoverable_and_preserve_old_definitions() {
    let root = Temp::new();
    let (ledger, catalog, source) = setup(&root).await;
    let old = catalog
        .save(key("old-prompt-only"), clone_request(&source, "old"))
        .await
        .unwrap();
    let view = catalog
        .model_settings(&source.execution_recipe_id)
        .await
        .unwrap();
    let mut settings = view.settings.clone();
    settings.temperature = 0.0;
    settings.sampling_seed = 0;
    settings.reasoning = RecipeReasoning::ModelDefault;
    settings.context_tokens = 131072;
    settings.tokens_per_call = 4096;
    settings.handoff_tokens = 2048;
    let with_settings = |value: RecipeModelSettings| {
        let mut request = clone_request(&source, "settings");
        if let RecipeMutation::Clone { request } = &mut request {
            request.model_settings = Some(value);
        }
        request
    };
    let request = with_settings(settings.clone());
    let saved = catalog
        .save(key("settings"), request.clone())
        .await
        .unwrap();
    let id = &saved.definition.execution_recipe_id;
    assert_eq!(catalog.model_settings(id).await.unwrap().settings, settings);
    assert!(
        saved.definition.inherited["model_capabilities"]["provenance"]
            .as_str()
            .unwrap()
            .contains("0.32.3")
    );
    assert_eq!(
        saved.definition.inherited["recipe"]["parameters"]["limits"]["generated_tokens"],
        source.inherited["recipe"]["parameters"]["limits"]["generated_tokens"]
    );
    assert_eq!(saved.definition.seeds, source.seeds);
    assert_eq!(
        catalog
            .definition(&source.execution_recipe_id)
            .await
            .unwrap(),
        source
    );
    for invalid in [
        {
            let mut s = settings.clone();
            s.context_tokens = 24576;
            s.tokens_per_call = 8192;
            s.handoff_tokens = 8192;
            s
        },
        {
            let mut s = settings.clone();
            s.top_k = 0;
            s
        },
        {
            let mut s = settings.clone();
            s.repeat_penalty = 2.1;
            s
        },
        {
            let mut s = settings.clone();
            s.temperature = -0.1;
            s
        },
    ] {
        let error = catalog
            .save(key("refused"), with_settings(invalid))
            .await
            .unwrap_err();
        assert_eq!(error.code(), "invalid_parameters");
        assert_eq!(
            catalog.lookup(&key("refused")).await.unwrap_err().code(),
            "not_found"
        );
    }
    // A long prompt can pass static limits but fail the actual controller guard.
    let mut too_long = request.clone();
    if let RecipeMutation::Clone { request } = &mut too_long {
        request.prompt = "x".repeat(65536);
        request.model_settings.as_mut().unwrap().context_tokens = 65536;
    }
    let error = catalog.save(key("long"), too_long).await.unwrap_err();
    assert!(error.0.message.contains("too small to begin work"));
    drop(catalog);
    drop(ledger);
    let catalog = Catalog::new(Ledger::open(&root.0).await.unwrap(), vec![]).unwrap();
    catalog.check().await.unwrap();
    assert_eq!(
        catalog
            .lookup(&key("old-prompt-only"))
            .await
            .unwrap()
            .definition,
        old.definition
    );
    assert_eq!(
        catalog.lookup(&key("settings")).await.unwrap().definition,
        saved.definition
    );
    let replay = catalog.save(key("settings"), request).await.unwrap();
    assert!(replay.replayed);
    assert_eq!(catalog.model_settings(id).await.unwrap().settings, settings);
}

#[tokio::test]
async fn inherited_settings_do_not_bypass_context_validation_on_clone_or_revision() {
    let root = Temp::new();
    let (_, catalog, source) = setup(&root).await;
    let saved = catalog
        .save(key("original"), clone_request(&source, "original"))
        .await
        .unwrap();
    let family = &saved.definition.saved.as_ref().unwrap().recipe_id;
    let mut clone = clone_request(&source, "too long");
    let mut revision = revise(&saved.definition, "too long");
    for mutation in [&mut clone, &mut revision] {
        match mutation {
            RecipeMutation::Clone { request } => {
                assert!(request.model_settings.is_none());
                request.prompt = "x".repeat(65536);
            }
            RecipeMutation::Revise { request, .. } => {
                assert!(request.model_settings.is_none());
                request.prompt = "x".repeat(65536);
            }
        }
    }
    for (id, mutation) in [("oversized-clone", clone), ("oversized-revision", revision)] {
        let error = catalog.save(key(id), mutation).await.unwrap_err();
        assert_eq!(error.code(), "invalid_parameters");
        assert!(error.0.message.contains("too small to begin work"));
        assert_eq!(
            catalog.lookup(&key(id)).await.unwrap_err().code(),
            "not_found"
        );
        assert_eq!(
            catalog.head(family).await.unwrap(),
            saved.definition.saved.clone().unwrap()
        );
    }
    assert_eq!(
        catalog
            .page(PageQuery::default(), None, None)
            .await
            .unwrap()
            .items
            .len(),
        1
    );
    // Rejected operations consumed neither keys nor revisions.
    let next = catalog
        .save(
            key("oversized-revision"),
            revise(&saved.definition, "valid"),
        )
        .await
        .unwrap();
    assert_eq!(next.definition.saved.unwrap().revision, 2);
    catalog
        .save(
            key("oversized-clone"),
            clone_request(&source, "valid clone"),
        )
        .await
        .unwrap();
    catalog.check().await.unwrap();
}
