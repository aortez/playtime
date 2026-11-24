mod common;
#[path = "support/preview_scene.rs"]
mod preview_scene;
use playtime_ui::{backend::prepare_preview, model::*};
use serde_json::json;

#[tokio::test]
async fn selected_preview_discloses_registration_and_boolean_controls_without_translation() {
    let app = common::demo(true).await;
    let original = preview_scene::live(app.model.preview.unwrap().value);
    for (value, label) in [
        (json!(false), "Off"),
        (json!(true), "On"),
        (json!("high"), "high"),
    ] {
        let mut preview = original.clone();
        preview.model_identity["registration"] = json!({"registration_id":"model-reviewed", "identity":{"capability_provenance":"explicit reviewed evidence"}});
        preview.model_identity["definition_sha256"] = json!("a".repeat(64));
        preview.model_identity["settings"]["thinking"] = value;
        let prepared = prepare_preview(preview.clone());
        let text = prepared
            .sections
            .iter()
            .find(|s| s.title == "Model")
            .unwrap()
            .lines
            .join("\n");
        assert!(text.contains("Registration: model-reviewed"));
        assert!(text.contains("Capability evidence: explicit reviewed evidence"));
        assert!(text.contains(&format!("Thinking: {label}")));
        assert_eq!(prepared.value, preview);
    }
}

#[tokio::test]
async fn readable_frozen_values_preserve_source_and_exact_launch_revision() {
    let mut app = common::demo(true).await;
    let original = preview_scene::live(app.model.preview.take().unwrap().value);
    let prepared = prepare_preview(original.clone());
    let section = |title| {
        prepared
            .sections
            .iter()
            .find(|s| s.title == title)
            .unwrap()
            .lines
            .join("\n")
    };
    assert!(
        section("Run limits")
            .contains("Tool parse retries: 0 consecutive · Per-session error limit: 1")
    );
    assert!(section("Run limits").contains("Command return allowance: 2,000 ms"));
    assert!(section("Model").contains("Thinking: model default (setting omitted)"));
    assert!(
        section("Model")
            .contains("Temperature: 0.7 · Top-p: 0.9 · Top-k: 40 · Repeat penalty: 1.1")
    );
    assert!(section("Workshop").contains("Workspace entries: 4,096"));
    assert!(section("Seed files").contains("README.txt · 12 bytes"));
    assert!(section("Seed files").contains("notes/goal.txt · 13 bytes"));
    assert!(!section("Seed files").contains("Start here"));
    assert!(prepared.inventory.contains("/workspace persists"));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&prepared.source.text).unwrap(),
        serde_json::to_value(&original).unwrap()
    );
    app.model.preview = Some(prepared);
    app.model.commands.clear();
    app.model.launch();
    let Action::Launch(request) = &app.model.commands[0].action else {
        panic!("launch expected")
    };
    assert_eq!(request.preview_revision, original.preview_revision);
}

#[tokio::test]
async fn missing_malformed_and_bounded_metadata_never_invent_values() {
    let app = common::demo(true).await;
    let mut original = app.model.preview.unwrap().value;
    original.model_identity = json!({"adapter":"unknown", "settings":{"thinking":{"nested":"not display text"},"temperature":null}});
    original.workshop_identity = json!({});
    original.effective_configuration = json!({});
    original.seeds.files = 2;
    let prepared = prepare_preview(original.clone());
    let lines = prepared
        .sections
        .iter()
        .flat_map(|s| &s.lines)
        .cloned()
        .collect::<Vec<_>>()
        .join("\n");
    for expected in [
        "Thinking: unavailable",
        "Temperature: unavailable",
        "Image: unavailable",
        "Workspace entries: unavailable",
        "Seed names and individual sizes unavailable",
    ] {
        assert!(lines.contains(expected), "{lines}");
    }
    assert!(!lines.contains("model default"));
    assert!(!lines.contains("not display text"));
    original.effective_configuration = json!({"recipe":{"seeds":(0..100).map(|i| (format!("seed-{i:03}"), json!("secret contents"))).collect::<serde_json::Map<_,_>>()}});
    let prepared = prepare_preview(original.clone());
    let lines = &prepared.sections.last().unwrap().lines;
    assert_eq!(lines.len(), 67); // aggregate, 64 names, overflow and mismatch notices
    assert!(!lines.join("\n").contains("secret contents"));
    original.effective_configuration = json!({});
    original.seeds.files = 0;
    assert!(
        prepare_preview(original)
            .sections
            .last()
            .unwrap()
            .lines
            .contains(&"No seed files.".into())
    );
}

#[tokio::test]
async fn a_strata_preview_names_its_lock_settings_and_held_host_without_ollama_labels() {
    let app = common::demo(true).await;
    let mut preview = preview_scene::live(app.model.preview.unwrap().value);
    preview.target = Some(playtime_protocol::api::InferenceTarget {
        id: "framework-strata".into(),
        name: "Framework · Strata".into(),
        provider: playtime_protocol::api::InferenceProvider::Strata,
        endpoint: String::new(),
        profile: Some("framework-gyro-m-fast".into()),
    });
    preview.model_identity = json!({
        "adapter":"strata", "lock":"example-strata-lock-v1", "model":"example-model",
        "engine":"0.1.58", "context_tokens":163840, "launcher_profile":"framework-gyro-m-fast",
        "host":{"backend":"hip", "quantization":"TQ2_0", "server_release_manifest_sha256":"f".repeat(64)},
        "settings":{"thinking":"medium", "summary_thinking":"none", "temperature":0.6, "top_p":0.95,
            "top_k":20, "min_p":0.0, "repetition_penalty":1.0, "presence_penalty":0.0,
            "frequency_penalty":0.0, "penalty_last_n":64}
    });
    let prepared = prepare_preview(preview);
    let section = |title| {
        prepared
            .sections
            .iter()
            .find(|s| s.title == title)
            .unwrap()
            .lines
            .join("\n")
    };
    let target = section("Inference target");
    assert!(target.contains("Framework · Strata (framework-strata)"));
    assert!(target.contains("Launcher profile: framework-gyro-m-fast"));
    assert!(target.contains("Provider: Strata"));
    let model = section("Model");
    for expected in [
        "Model: example-model · Lock: example-strata-lock-v1",
        "Strata engine: 0.1.58 · Server context: 163,840 tokens",
        "Host: hip backend · TQ2_0 quantization · Launcher profile: framework-gyro-m-fast",
        "Thinking: medium · Summary thinking: none",
        "Temperature: 0.6 · Top-p: 0.95 · Top-k: 20 · Min-p: 0.0",
        "Repetition penalty: 1.0 · Presence penalty: 0.0 · Frequency penalty: 0.0 · Penalty window: 64 tokens",
    ] {
        assert!(model.contains(expected), "{model}");
    }
    let all = format!("{target}\n{model}");
    for absent in [
        "Ollama",
        "Repeat penalty",
        "Resolved addresses",
        "unavailable",
    ] {
        assert!(!all.contains(absent), "{all}");
    }
    assert_eq!(prepared.model_name, None);
}

#[tokio::test]
async fn archive_waiting_is_normal_and_scoped_to_the_selected_observation() {
    let mut model = common::demo(false).await.model;
    let run = model.selected.clone();
    let tag = model.tag;
    model.apply(Reply {
        tag,
        run: run.clone(),
        data: Data::ArchiveWaiting(true),
    });
    assert!(model.archive_waiting);
    assert_eq!(model.connection, ConnectionState::Connected);
    model.apply(Reply {
        tag,
        run: run.clone(),
        data: Data::ArchiveWaiting(false),
    });
    assert!(!model.archive_waiting);
    model.select_run(playtime_core::RunId::new("another-run").unwrap());
    model.apply(Reply {
        tag,
        run,
        data: Data::ArchiveWaiting(true),
    });
    assert!(!model.archive_waiting);
    model.archive_waiting = true;
    model.connect(Default::default());
    assert!(!model.archive_waiting);
}

#[tokio::test]
async fn long_preview_text_leaves_room_to_read_every_source_page() {
    let mut preview = preview_scene::live(common::demo(true).await.model.preview.unwrap().value);
    preview.prompt = "🦀".repeat(16384);
    preview.protocol = "Protocol text. ".repeat(8192);
    preview.effective_configuration["scenario"]["inventory"] =
        json!("Inventory text. ".repeat(8192));
    preview.effective_configuration["recipe"]["seeds"] = json!(
        (0..64)
            .map(|i| (format!("{i:03}{}", "n".repeat(400)), json!("seed contents")))
            .collect::<serde_json::Map<_, _>>()
    );
    preview.seeds.files = 64;
    preview.model_identity["addresses"] = json!(vec!["a".repeat(400); 16]);
    let prepared = prepare_preview(preview);
    let shown = prepared.prompt.len()
        + prepared.protocol.len()
        + prepared.inventory.len()
        + prepared
            .sections
            .iter()
            .flat_map(|s| &s.lines)
            .map(String::len)
            .sum::<usize>();
    assert!(shown + prepared.source.text.len() <= playtime_ui::transcript::TEXT_BYTES);
    assert!(prepared.prompt.ends_with(" …"));
    assert!(prepared.protocol.ends_with(" …"));
    assert!(prepared.inventory.ends_with(" …"));
    assert!(prepared.source.text.len() < prepared.source.total);
}

#[tokio::test]
async fn saved_definitions_state_where_their_seed_files_came_from() {
    let app = common::demo(true).await;
    let mut original = app.model.preview.unwrap().value;
    let seed_lines = |preview: &playtime_protocol::api::Preview| {
        prepare_preview(preview.clone())
            .sections
            .into_iter()
            .find(|s| s.title == "Seed files")
            .unwrap()
            .lines
    };
    original.seeds.files = 1;
    original.seeds.bytes = 1;
    original.effective_configuration = json!({"recipe":{"seeds":{"notes.txt":"x"}}});
    // A catalog recipe's preview has no saved definition, so it states no origin.
    assert_eq!(
        seed_lines(&original),
        ["1 files · 1 byte total", "notes.txt · 1 byte"]
    );
    for (definition, expected) in [
        (json!({}), "Seed files from the catalog recipe"),
        (json!({"seed_origin":"uploaded"}), "Uploaded seed files"),
        (
            json!({"seed_origin":"later"}),
            "Unrecognized seed-file origin; inspect the full frozen preview.",
        ),
    ] {
        original.effective_configuration["definition"] = definition;
        let lines = seed_lines(&original);
        assert_eq!(lines[1], expected, "{lines:?}");
        assert_eq!(lines[2], "notes.txt · 1 byte");
    }
    original.seeds.files = 0;
    original.seeds.bytes = 0;
    original.effective_configuration =
        json!({"recipe":{"seeds":{}},"definition":{"seed_origin":"empty"}});
    assert_eq!(seed_lines(&original)[1], "Started without seed files");
    // A saved recipe that kept a catalog recipe's empty set says nothing more.
    original.effective_configuration = json!({"seeds":{},"definition":{}});
    assert_eq!(seed_lines(&original).len(), 1);
}
