#[path = "common/registered.rs"]
mod registered;
use playtime_protocol::api::RecipeReasoning;
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
