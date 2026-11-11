mod common;
use playtime_client::{Client, recipes::RecipeSaveStore};
use playtime_core::RunMode;
use playtime_protocol::api::*;
use playtime_server::{
    auth::Token, catalog::Catalog, daemon::catalog::load, fixture::Fixture, http::Server,
    lifecycle::Ledger, registrations::Registry,
};
use playtime_test_support::{
    TestRoot,
    recipe_models::{boolean_model, model_with_context, retain},
};
use std::{path::Path, sync::Arc};

fn clone(
    source: &RecipeDefinition,
    model: ModelRegistrationId,
    settings: Option<RecipeModelSettings>,
) -> RecipeMutation {
    RecipeMutation::Clone {
        request: CloneRecipeRequest {
            compaction: None,
            harness: None,
            source: RecipeSource {
                execution_recipe_id: source.execution_recipe_id.clone(),
                definition_digest: source.definition_digest.clone(),
            },
            name: "Explicit boolean model".into(),
            prompt: source.prompt.clone(),
            model_settings: settings,
            model_registration: Some(model),
        },
    }
}
fn revise(source: &RecipeDefinition) -> RecipeMutation {
    RecipeMutation::Revise {
        recipe_id: source.saved.as_ref().unwrap().recipe_id.clone(),
        request: ReviseRecipeRequest {
            compaction: None,
            harness: None,
            expected_head: source.execution_recipe_id.clone(),
            name: "Offline revision".into(),
            prompt: source.prompt.clone(),
            model_settings: None,
            model_registration: None,
        },
    }
}

#[tokio::test]
async fn model_selection_defaults_context_but_explicit_and_saved_allocations_stay_frozen() {
    let root = TestRoot::new().unwrap();
    let ledger = Ledger::open(&root.0).await.unwrap();
    let catalog = Catalog::new(
        ledger.clone(),
        load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../recipes")).unwrap(),
    )
    .unwrap();
    let source = catalog
        .definition(&RecipeId::new("curiosity").unwrap())
        .await
        .unwrap();
    let original = catalog
        .model_settings(&source.execution_recipe_id)
        .await
        .unwrap()
        .settings;
    assert_eq!(original.context_tokens, 65_536);
    let mut saved_override = None;
    for (window, expected) in [(98_304, 98_304), (131_072, 131_072), (262_144, 131_072)] {
        let model = model_with_context(window);
        retain(&root.0, &model);
        let key = |label| IdempotencyKey::new(format!("context-{window}-{label}")).unwrap();
        let mut request = clone(&source, model.summary.registration_id.clone(), None);
        let RecipeMutation::Clone { request: body } = &mut request else {
            panic!()
        };
        // This task cannot start at the inherited 64K allocation.
        body.prompt = "x".repeat(24_000);
        let mut too_small = request.clone();
        let RecipeMutation::Clone { request: body } = &mut too_small else {
            panic!()
        };
        body.model_settings = Some(original.clone());
        assert_eq!(
            catalog
                .save(key("explicit-too-small"), too_small)
                .await
                .unwrap_err()
                .code(),
            "invalid_parameters"
        );
        let saved = catalog.save(key("default"), request.clone()).await.unwrap();
        let settings = catalog
            .model_settings(&saved.definition.execution_recipe_id)
            .await
            .unwrap()
            .settings;
        let mut expected_settings = original.clone();
        expected_settings.context_tokens = expected;
        assert_eq!(settings, expected_settings);
        assert_eq!(
            catalog
                .save(key("default"), request)
                .await
                .unwrap()
                .definition,
            saved.definition
        );

        let mut override_settings = original.clone();
        override_settings.context_tokens = 81_920;
        let overridden = catalog
            .save(
                key("explicit"),
                clone(
                    &source,
                    model.summary.registration_id.clone(),
                    Some(override_settings.clone()),
                ),
            )
            .await
            .unwrap();
        assert_eq!(
            catalog
                .model_settings(&overridden.definition.execution_recipe_id)
                .await
                .unwrap()
                .settings,
            override_settings
        );
        // Re-selecting the same identity without settings is an edit, not a new default.
        let mut edit = revise(&overridden.definition);
        let RecipeMutation::Revise { request, .. } = &mut edit else {
            panic!()
        };
        request.model_registration = Some(model.summary.registration_id);
        let edited = catalog.save(key("same-model"), edit).await.unwrap();
        assert_eq!(
            catalog
                .model_settings(&edited.definition.execution_recipe_id)
                .await
                .unwrap()
                .settings,
            override_settings
        );
        saved_override = Some(edited.definition);
    }
    assert_eq!(
        catalog
            .definition(&source.execution_recipe_id)
            .await
            .unwrap(),
        source
    );
    let saved = saved_override.unwrap();
    drop(catalog);
    drop(ledger);
    let catalog = Catalog::new(Ledger::open(&root.0).await.unwrap(), vec![]).unwrap();
    assert_eq!(
        catalog
            .definition(&saved.execution_recipe_id)
            .await
            .unwrap(),
        saved
    );
    let revised = catalog
        .save(
            IdempotencyKey::new("context-offline-edit").unwrap(),
            revise(&saved),
        )
        .await
        .unwrap();
    assert_eq!(
        catalog
            .model_settings(&revised.definition.execution_recipe_id)
            .await
            .unwrap()
            .settings
            .context_tokens,
        81_920
    );
    catalog.check().await.unwrap();
}
#[tokio::test]
async fn choose_correct_save_and_recover_an_immutable_model_without_execution_or_fallback() {
    let root = TestRoot::new().unwrap();
    let ledger = Ledger::open(&root.0).await.unwrap();
    let service = ServiceId::new(ledger.service_id().as_str()).unwrap();
    let catalog = Catalog::new(
        ledger.clone(),
        load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../recipes")).unwrap(),
    )
    .unwrap();
    let registry = Registry::new(ledger, None);
    let model = boolean_model();
    retain(&root.0, &model);
    registry.check().await.unwrap();
    let fixture = Arc::new(
        Fixture::with_catalog(service, catalog.clone()).with_model_registry(registry.clone()),
    );
    let token = Token::generate().unwrap();
    let server = Server::new(fixture.clone(), token.clone())
        .unwrap()
        .listen("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let client = Client::connect(&server.endpoint(), token, None)
        .await
        .unwrap();
    let original = client
        .recipe_definition(&RecipeId::new("curiosity").unwrap())
        .await
        .unwrap();
    let settings = client
        .recipe_model_settings(&original)
        .await
        .unwrap()
        .settings;
    assert_eq!(settings.reasoning, RecipeReasoning::High);
    let pending = client
        .prepare_recipe_save(clone(
            &original,
            model.summary.registration_id.clone(),
            None,
        ))
        .unwrap();
    let error = client.save_recipe(&pending).await.unwrap_err();
    assert_eq!(error.api_code(), Some("invalid_parameters"));
    assert!(
        client
            .saved_recipes(&PageQuery::default())
            .await
            .unwrap()
            .items
            .is_empty()
    );
    // High is never translated; change only the explicitly incompatible values.
    let mut corrected = settings.clone();
    corrected.reasoning = RecipeReasoning::On;
    corrected.context_tokens = 49152;
    let pending = client
        .prepare_recipe_save(clone(
            &original,
            model.summary.registration_id.clone(),
            Some(corrected.clone()),
        ))
        .unwrap();
    let store = RecipeSaveStore::new(&root.join("state"));
    let receipt = store.save(&pending, client.endpoint()).unwrap();
    let saved = client.save_recipe(&pending).await.unwrap();
    assert_eq!(saved.definition.schema_version, 2);
    assert_eq!(
        saved.definition.model_registration,
        Some(model.summary.clone())
    );
    assert_eq!(
        saved.definition.source.as_ref().unwrap().definition_digest,
        original.definition_digest
    );
    assert_eq!(
        saved.definition.inherited["model_lock"],
        original.inherited["model_lock"]
    );
    assert_eq!(saved.definition.seeds, original.seeds);
    assert_eq!(
        client
            .recipe_model_settings(&saved.definition)
            .await
            .unwrap()
            .settings,
        corrected
    );
    let source_recipe = &original.inherited["recipe"];
    let saved_recipe = &saved.definition.inherited["recipe"];
    for pointer in [
        "/protocol",
        "/inventory",
        "/workspace_entries",
        "/parameters/workshop",
        "/parameters/limits/generated_tokens",
        "/parameters/limits/sessions",
        "/parameters/limits/work_turns_per_session",
        "/parameters/limits/wall_seconds",
    ] {
        assert_eq!(
            source_recipe.pointer(pointer),
            saved_recipe.pointer(pointer),
            "{pointer}"
        );
    }
    assert_eq!(
        client
            .recipe_definition(&original.execution_recipe_id)
            .await
            .unwrap(),
        original
    );
    // Discard the reply: use the original receipt and key, never a replacement save.
    let recovered = client.lookup_recipe_save(&receipt.pending).await.unwrap();
    assert!(recovered.replayed);
    assert_eq!(recovered.definition, saved.definition);
    store.acknowledge(&receipt, &recovered).unwrap();
    assert!(store.list().unwrap().is_empty());
    assert_eq!(
        client.save_recipe(&pending).await.unwrap().definition,
        saved.definition
    );
    let frozen = catalog
        .frozen(saved.definition.execution_recipe_id.as_str())
        .await
        .unwrap()
        .0;
    assert!(
        frozen
            .executable()
            .unwrap_err()
            .to_string()
            .contains("unsupported")
    );
    assert_eq!(
        client
            .preview(&saved.definition.execution_recipe_id, RunMode::Experiment)
            .await
            .unwrap_err()
            .api_code(),
        Some("execution_unavailable")
    );
    let next = client
        .prepare_recipe_save(revise(&saved.definition))
        .unwrap();
    let b = client.save_recipe(&next).await.unwrap();
    assert_eq!(
        b.definition.model_registration,
        saved.definition.model_registration
    );
    let stale = client
        .prepare_recipe_save(revise(&saved.definition))
        .unwrap();
    assert_eq!(
        client.save_recipe(&stale).await.unwrap_err().api_code(),
        Some("head_conflict")
    );
    assert_eq!(registry.list().await.unwrap().items.len(), 1);
    assert!(!fixture.calls().iter().any(|c| c.operation == "admit"));
    catalog.check().await.unwrap();
    server.stop().await.unwrap();
}

#[tokio::test]
async fn missing_unknown_incompatible_and_inherited_context_failures_preserve_heads() {
    let root = TestRoot::new().unwrap();
    let ledger = Ledger::open(&root.0).await.unwrap();
    let catalog = Catalog::new(
        ledger.clone(),
        load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../recipes")).unwrap(),
    )
    .unwrap();
    let source = catalog
        .definition(&RecipeId::new("curiosity").unwrap())
        .await
        .unwrap();
    let mut settings = catalog
        .model_settings(&source.execution_recipe_id)
        .await
        .unwrap()
        .settings;
    settings.reasoning = RecipeReasoning::Off;
    settings.context_tokens = 49152;
    let key = |s: &str| IdempotencyKey::new(s).unwrap();
    assert_eq!(
        catalog
            .save(
                key("missing"),
                clone(&source, ModelRegistrationId::new("absent").unwrap(), None)
            )
            .await
            .unwrap_err()
            .code(),
        "invalid_parameters"
    );
    for variant in ["unknown", "no-tools", "unknown-context", "bad-context"] {
        let mut model = boolean_model();
        model.summary.registration_id = ModelRegistrationId::new(variant).unwrap();
        model.summary.identity.name = variant.into();
        match variant {
            "unknown" => model.summary.identity.reviewed_capabilities = None,
            "no-tools" => {
                model.summary.identity.advertised_capabilities = vec!["completion".into()];
                model.definition["capabilities"] = serde_json::json!(["completion"]);
                model.summary.identity.definition_sha256 = playtime_ollama::json::hash(
                    &playtime_ollama::json::canonical(&model.definition, MODEL_DEFINITION_BYTES)
                        .unwrap(),
                );
            }
            "unknown-context" => {
                model
                    .summary
                    .identity
                    .reviewed_capabilities
                    .as_mut()
                    .unwrap()
                    .context_ceiling = None
            }
            "bad-context" => {
                model
                    .summary
                    .identity
                    .reviewed_capabilities
                    .as_mut()
                    .unwrap()
                    .context_ceiling = Some(1024)
            }
            _ => unreachable!(),
        }
        retain(&root.0, &model);
        assert_eq!(
            catalog
                .save(
                    key(variant),
                    clone(
                        &source,
                        model.summary.registration_id,
                        Some(settings.clone())
                    )
                )
                .await
                .unwrap_err()
                .code(),
            "invalid_parameters"
        );
        assert_eq!(
            catalog.lookup(&key(variant)).await.unwrap_err().code(),
            "not_found"
        );
    }
    let model = boolean_model();
    retain(&root.0, &model);
    let saved = catalog
        .save(
            key("valid"),
            clone(&source, model.summary.registration_id, Some(settings)),
        )
        .await
        .unwrap();
    let mut large = revise(&saved.definition);
    let RecipeMutation::Revise { request, .. } = &mut large else {
        panic!()
    };
    request.prompt = "x".repeat(65536);
    assert_eq!(
        catalog
            .save(key("context"), large)
            .await
            .unwrap_err()
            .code(),
        "invalid_parameters"
    );
    assert_eq!(
        catalog
            .head(&saved.definition.saved.as_ref().unwrap().recipe_id)
            .await
            .unwrap()
            .execution_recipe_id,
        saved.definition.execution_recipe_id
    );
    drop(catalog);
    drop(ledger);
    let ledger = Ledger::open(&root.0).await.unwrap();
    let catalog = Catalog::new(ledger, vec![]).unwrap();
    catalog.check().await.unwrap();
    assert_eq!(
        catalog
            .definition(&saved.definition.execution_recipe_id)
            .await
            .unwrap(),
        saved.definition
    );
    assert_eq!(
        catalog
            .save(key("offline-edit"), revise(&saved.definition))
            .await
            .unwrap()
            .definition
            .model_registration,
        saved.definition.model_registration
    );
}

#[tokio::test]
async fn real_registration_path_selects_the_reviewed_lock_but_never_approves_an_alias() {
    use playtime_server::models::ModelBrowser;
    use playtime_test_support::model_metadata::FakeOllama;
    let root = TestRoot::new().unwrap();
    let fake = FakeOllama::start().await;
    let ledger = Ledger::open(&root.0).await.unwrap();
    let catalog = Catalog::new(
        ledger.clone(),
        load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../recipes")).unwrap(),
    )
    .unwrap();
    let registry = Registry::new(
        ledger,
        Some(Arc::new(ModelBrowser::new(&fake.endpoint).unwrap())),
    );
    let mut registered = Vec::new();
    for name in [playtime_ollama::Profile::MODEL, "alias:latest"] {
        let review = registry
            .review(ModelInspectRequest {
                name: name.into(),
                expected_digest: None,
            })
            .await
            .unwrap();
        registered.push(
            registry
                .register(
                    IdempotencyKey::new(format!("register-{}", registered.len())).unwrap(),
                    review.request.unwrap(),
                )
                .await
                .unwrap(),
        );
    }
    fake.stop();
    let source = catalog
        .definition(&RecipeId::new("curiosity").unwrap())
        .await
        .unwrap();
    let selected = catalog
        .save(
            IdempotencyKey::new("same-lock").unwrap(),
            clone(
                &source,
                registered[0].registration.registration_id.clone(),
                None,
            ),
        )
        .await
        .unwrap();
    assert_eq!(
        selected.definition.model_registration,
        Some(registered[0].registration.clone())
    );
    let frozen = catalog
        .frozen(selected.definition.execution_recipe_id.as_str())
        .await
        .unwrap()
        .0;
    assert_eq!(frozen.recipe.model.thinking, "high");
    frozen.executable().unwrap();
    let mut malformed = frozen.clone();
    malformed.model_registration = None;
    assert!(malformed.validate().is_err());
    malformed = frozen;
    malformed.schema_version = 1;
    assert!(malformed.validate().is_err());
    // An alias with the same digest has no reviewed capability profile and cannot
    // borrow the original request name's support or silently fall back to its lock.
    assert!(
        registered[1]
            .registration
            .identity
            .reviewed_capabilities
            .is_none()
    );
    assert_eq!(
        catalog
            .save(
                IdempotencyKey::new("alias").unwrap(),
                clone(
                    &source,
                    registered[1].registration.registration_id.clone(),
                    None
                )
            )
            .await
            .unwrap_err()
            .code(),
        "invalid_parameters"
    );
    assert_eq!(registry.list().await.unwrap().items.len(), 2);
    catalog.check().await.unwrap();
}

#[tokio::test]
async fn review_keeps_a_reviewed_lock_when_ollama_reorders_its_parameters() {
    use playtime_server::models::ModelBrowser;
    use playtime_test_support::model_metadata::FakeOllama;
    let lock: serde_json::Value =
        serde_json::from_slice(playtime_runner::docker::QWEN_MODEL_LOCK).unwrap();
    let lines: Vec<&str> = lock["definition"]["parameters"]
        .as_str()
        .unwrap()
        .split('\n')
        .collect();
    // On 2026-10-26 a fresh Ollama listed repeat_penalty first (#155).
    let reordered = [3, 0, 1, 2, 4, 5].map(|i| lines[i]).join("\n");
    let root = TestRoot::new().unwrap();
    let fake = FakeOllama::start().await;
    {
        let mut state = fake.metadata.0.lock().unwrap();
        state.tags = serde_json::json!({"models":[{"name":lock["name"], "digest":lock["digest"]}]});
        state.version = lock["ollama_version"].as_str().unwrap().into();
        state.show = lock["definition"].clone();
        state.show["parameters"] = reordered.clone().into();
    }
    let registry = Registry::new(
        Ledger::open(&root.0).await.unwrap(),
        Some(Arc::new(ModelBrowser::new(&fake.endpoint).unwrap())),
    );
    let inspect = || ModelInspectRequest {
        name: lock["name"].as_str().unwrap().into(),
        expected_digest: None,
    };
    let review = registry.review(inspect()).await.unwrap();
    assert_eq!(
        review.inspection.definition_sha256.as_deref(),
        lock["definition_sha256"].as_str()
    );
    assert!(review.inspection.reviewed_capabilities.is_some());
    let accepted = registry
        .register(
            IdempotencyKey::new("reordered").unwrap(),
            review.request.unwrap(),
        )
        .await
        .unwrap();
    assert!(
        accepted
            .registration
            .identity
            .reviewed_capabilities
            .is_some()
    );
    let record = registry
        .get(accepted.registration.registration_id)
        .await
        .unwrap();
    assert_eq!(record.definition, lock["definition"]);
    // A changed value is a different identity, with no reviewed support.
    fake.metadata.0.lock().unwrap().show["parameters"] = reordered.replace("0.6", "0.7").into();
    let changed = registry.review(inspect()).await.unwrap().inspection;
    assert_ne!(
        changed.definition_sha256.as_deref(),
        lock["definition_sha256"].as_str()
    );
    assert!(changed.reviewed_capabilities.is_none());
}
