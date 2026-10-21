use playtime_core::RunId;
use playtime_protocol::{api::*, events::EventEnvelope};
use playtime_server::{application::Application, fixture::Fixture};
use playtime_ui::{backend::prepare_preview, model::*, transcript::*};

fn records() -> Vec<EventEnvelope> {
    include_str!("../../playtime-protocol/tests/fixtures/controller-observations/completed.jsonl")
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}
#[tokio::test]
async fn selection_changes_invalidate_preview_and_duplicate_launch_intents() {
    let fixture = Fixture::new(ServiceId::new("ui-fixture").unwrap());
    let preview = fixture
        .preview(
            RecipeId::new("fixture-example").unwrap(),
            PreviewRequest {
                mode: playtime_core::RunMode::Simulation,
            },
        )
        .await
        .unwrap();
    let mut model = Model::default();
    model.connection = ConnectionState::Connected;
    model.modes = vec![preview.mode];
    model.choose_recipe(preview.recipe_id.clone(), preview.mode);
    let original = model.tag;
    model.choose_recipe(RecipeId::new("fixture-empty").unwrap(), preview.mode);
    model.apply(Reply {
        tag: original,
        run: None,
        data: Data::Preview(Box::new(prepare_preview(preview.clone()))),
    });
    assert!(model.preview.is_none());
    assert!(!model.can_launch());
    model.choose_recipe(preview.recipe_id.clone(), preview.mode);
    model.apply(Reply {
        tag: model.tag,
        run: None,
        data: Data::Preview(Box::new(prepare_preview(preview))),
    });
    model.commands.clear();
    model.launch();
    model.launch();
    assert_eq!(model.commands.len(), 1);
    assert!(matches!(model.commands[0].action, Action::Launch(_)));
}
#[test]
fn late_run_responses_and_connection_loss_do_not_invent_outcomes() {
    let contract: serde_json::Value = serde_json::from_str(include_str!(
        "../../playtime-server/tests/fixtures/http-contract.json"
    ))
    .unwrap();
    let view: RunView = serde_json::from_value(contract["completed_run"].clone()).unwrap();
    let mut model = Model::default();
    model.select_run(view.run_id.clone());
    let old = model.tag;
    model.select_run(RunId::new("another-run").unwrap());
    model.apply(Reply {
        tag: old,
        run: Some(view.run_id.clone()),
        data: Data::Run(Box::new(playtime_ui::backend::prepare_run(view))),
    });
    assert!(model.run.is_none());
    model.apply(Reply {
        tag: model.tag,
        run: model.selected.clone(),
        data: Data::Connection(ConnectionState::Error("offline".into())),
    });
    assert!(model.run.is_none());
    assert!(model.stale);
    assert!(
        !model
            .commands
            .iter()
            .any(|c| matches!(c.action, Action::Cancel(_)))
    );
}
#[test]
fn late_history_merges_by_identity_and_final_text_replaces_chunks() {
    let records = records();
    let mut transcript = Transcript::default();
    for event in records.iter().rev() {
        transcript.apply(event.clone(), None).unwrap();
    }
    let rows = transcript.window(View::Full, 0, 64);
    assert_eq!(transcript.metrics.records, records.len());
    assert!(
        rows.rows
            .windows(2)
            .all(|rows| rows[0].first_sequence < rows[1].first_sequence)
    );
    for event in records {
        assert!(!transcript.apply(event, None).unwrap());
    }
    let response = rows
        .rows
        .iter()
        .find(|r| r.title.contains("Model r1"))
        .unwrap();
    let parts = response
        .sections
        .iter()
        .filter(|s| s.label == "Model response")
        .collect::<Vec<_>>();
    assert_eq!(parts.len(), 1, "final response must replace the chunks");
    assert!(!parts[0].text.contains("preview"));
}
#[test]
fn unknown_sources_remain_available_and_conflicting_duplicates_fail() {
    let mut transcript = Transcript::default();
    let mut event = records().remove(0);
    event.kind = "future_event".into();
    event.data = serde_json::json!({"text":"é💥","opaque":{"path":"/do/not/read"}});
    transcript.apply(event.clone(), None).unwrap();
    let source = transcript.raw(event.sequence.get(), 0).unwrap();
    assert!(source.text.contains("/do/not/read"));
    for view in [View::Readable, View::Full] {
        assert!(
            transcript.window(view, 0, 1).rows[0]
                .status
                .contains("Unknown")
        );
    }
    event.data = serde_json::json!({"text":"changed"});
    assert!(transcript.apply(event, None).is_err());
}
#[test]
fn text_pages_respect_utf8_and_diff_limits_are_explicit() {
    let text = "é💥".repeat(20_000);
    let first = source_page(&text, 0);
    let second = source_page(&text, first.text.len());
    assert!(first.text.len() <= TEXT_BYTES && second.text.len() <= TEXT_BYTES);
    assert_eq!(first.text + &second.text, text);
    assert_eq!(
        playtime_ui::media::memory_diff("saved", "saved").0,
        "Unchanged"
    );
    assert!(
        playtime_ui::media::memory_diff(&"a\n".repeat(4001), "saved")
            .1
            .contains("Diff limit")
    );
}

#[test]
fn cancellation_reply_loss_stays_unresolved_and_inspection_generations_are_independent() {
    let mut model = Model::default();
    model.connection = ConnectionState::Connected;
    model.select_run(RunId::new("running").unwrap());
    model.commands.clear();
    model.cancel();
    model.cancel();
    assert_eq!(
        model
            .commands
            .iter()
            .filter(|c| matches!(c.action, Action::Cancel(_)))
            .count(),
        1
    );
    model.apply(Reply {
        tag: model.tag,
        run: model.selected.clone(),
        data: Data::CancellationUnresolved("Reply lost; enforcement is unknown".into()),
    });
    assert!(matches!(model.cancellation, Mutation::Unresolved(_)));
    assert!(model.run.is_none());
    model.inspect(1, 10);
    let old = model.tag;
    model.inspect(2, 20);
    model.apply(Reply {
        tag: old,
        run: model.selected.clone(),
        data: Data::Source(10, source_page("old source", 0)),
    });
    assert!(model.source.is_none());
    assert_eq!(model.source_sequence, 20);
    model.apply(Reply {
        tag: model.tag,
        run: model.selected.clone(),
        data: Data::Source(20, source_page("selected source", 0)),
    });
    assert_eq!(model.source.as_ref().unwrap().text, "selected source");
}
