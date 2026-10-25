#[path = "common/registered.rs"]
mod registered;
use playtime_protocol::api::{HarnessId, HarnessView, MEMENTO_TURNS_V1, RecipeReasoning};
use playtime_runner::docker::*;
use serde_json::json;

fn definition() -> FrozenRecipe {
    let scenario: playtime_engine::simulation::Scenario =
        serde_json::from_slice(LIVE_SCENARIO).unwrap();
    let mut definition = FrozenRecipe::current(Recipe {
        id: "selected-smoke".into(),
        name: "Selected smoke".into(),
        prompt: scenario.prompt,
        protocol: scenario.protocol,
        inventory: scenario.inventory,
        parameters: scenario.parameters,
        workspace_entries: 512,
        model: Sampling {
            thinking: "high".into(),
            temperature: 0.8,
            top_p: 0.95,
            top_k: 40,
            repeat_penalty: 1.0,
        },
        seeds: Default::default(),
    })
    .unwrap();
    definition
        .select_registered_model(registered::qwen())
        .unwrap();
    let mut settings = definition.model_settings().unwrap();
    settings.reasoning = RecipeReasoning::Off;
    definition.apply_model_settings(&settings).unwrap();
    definition
}
fn profile(definition: FrozenRecipe) -> Profile {
    Profile::freeze_definition(
        definition,
        playtime_docker::transport::Selection {
            executable: "/synthetic/docker".into(),
            socket: "/synthetic/socket".into(),
            executable_sha256: "a".repeat(64),
        },
        playtime_docker::DaemonIdentity {
            id: "synthetic".into(),
            name: "synthetic".into(),
            cli_version: "28".into(),
            server_version: "28".into(),
            api_version: "1.51".into(),
            kernel: "synthetic".into(),
        },
        LiveHost {
            endpoint: "http://127.0.0.1:11435".into(),
            addresses: vec!["127.0.0.1".parse().unwrap()],
            live_lock: "/synthetic/live-model.lock".into(),
        },
    )
    .unwrap()
}
#[test]
fn selected_execution_projects_the_exact_lock_without_rewriting_base_provenance() {
    assert_eq!(
        QWEN_MODEL_LOCK,
        include_bytes!("../../../models/qwen3-1.7b.lock.json")
    );
    let definition = definition();
    definition.executable().unwrap();
    let frozen = profile(definition.clone());
    frozen.validate().unwrap();
    let model = frozen.model.as_ref().unwrap();
    model.execution_model().unwrap(); // independently verifies the canonical hash
    assert_eq!(model.lock["name"], "qwen3:1.7b");
    assert_eq!(model.settings["thinking"], false);
    assert_eq!(
        model.registration,
        definition
            .model_registration
            .as_ref()
            .map(|m| m.summary.clone())
    );
    assert_eq!(frozen.definition.unwrap(), definition);
    assert_eq!(
        definition.model_lock,
        serde_json::from_slice::<serde_json::Value>(MODEL_LOCK).unwrap()
    );
    assert_eq!(model.execution_model().unwrap().reasoning.len(), 3);
}
#[test]
fn coherent_stored_claims_cannot_approve_a_model_or_change_the_execution_projection() {
    for case in 0..6 {
        let mut definition = definition();
        let selected = definition.model_registration.as_mut().unwrap();
        match case {
            0 => selected.summary.identity.name = "alias:latest".into(),
            1 => selected.summary.identity.digest = "b".repeat(64),
            2 => selected.definition["template"] = json!("changed"),
            3 => selected.summary.identity.backend_version = "0.32.4".into(),
            4 => {
                selected
                    .summary
                    .identity
                    .reviewed_capabilities
                    .as_mut()
                    .unwrap()
                    .default_reasoning = Some(RecipeReasoning::Off);
                definition.model_capabilities =
                    selected.summary.identity.reviewed_capabilities.clone();
            }
            _ => definition.recipe.model.thinking = "high".into(),
        }
        assert!(definition.executable().is_err(), "case {case}");
    }
    let frozen = profile(definition());
    for pointer in [
        "/harness",
        "/definition/harness",
        "/model/lock/name",
        "/model/settings/thinking",
        "/model/registration/registration_id",
    ] {
        let mut value = serde_json::to_value(&frozen).unwrap();
        *value.pointer_mut(pointer).unwrap() = json!("changed");
        assert!(
            serde_json::from_value::<Profile>(value)
                .unwrap()
                .validate()
                .is_err(),
            "{pointer}"
        );
    }
}

#[test]
fn selection_preserves_definition_bytes_and_unknown_harness_is_readable_only() {
    let mut recipe = definition().recipe;
    recipe.model.thinking = "high".into();
    let legacy = FrozenRecipe::current(recipe).unwrap();
    for mut definition in [legacy, definition()] {
        let bytes = serde_json::to_vec(&definition).unwrap();
        let digest = definition.digest().unwrap();
        definition
            .select_harness(&HarnessId::new(MEMENTO_TURNS_V1).unwrap())
            .unwrap();
        assert_eq!(serde_json::to_vec(&definition).unwrap(), bytes);
        assert_eq!(definition.digest().unwrap(), digest);
        assert!(
            definition
                .select_harness(&HarnessId::new("future_v2").unwrap())
                .is_err()
        );
        assert_eq!(serde_json::to_vec(&definition).unwrap(), bytes);
        definition.harness = "future_v2".into();
        definition.validate().unwrap();
        assert!(!definition.harness_view().unwrap().harness.supported);
        assert!(
            definition
                .harness_view()
                .unwrap()
                .harness
                .settings
                .is_none()
        );
        assert!(definition.executable().is_err());
        definition
            .check_model_settings(&definition.model_settings().unwrap())
            .unwrap();
    }
}

#[test]
fn archive_identity_is_explicit_and_independent_of_current_support() {
    let mut profile = profile(definition());
    let mut value = serde_json::to_value(&profile).unwrap();
    let view = HarnessView::from_configuration(&value).unwrap().unwrap();
    assert_eq!(
        view,
        profile
            .definition
            .as_ref()
            .unwrap()
            .harness_view()
            .unwrap()
            .harness
    );
    value.as_object_mut().unwrap().remove("harness");
    // Older authored profiles retain identity in their saved definition.
    assert_eq!(HarnessView::from_configuration(&value).unwrap(), Some(view));
    value.as_object_mut().unwrap().remove("definition");
    assert!(HarnessView::from_configuration(&value).unwrap().is_none());
    profile.harness = None;
    assert!(profile.validate().is_err()); // historical readback is not admission
    let mut value = serde_json::to_value(profile).unwrap();
    value["harness"] = json!("future_v2");
    assert!(HarnessView::from_configuration(&value).is_err());
    value["definition"]["harness"] = json!("future_v2");
    let view = HarnessView::from_configuration(&value).unwrap().unwrap();
    assert_eq!(view.id, "future_v2");
    assert!(!view.supported);
    assert!(view.settings.is_none());
}
