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
