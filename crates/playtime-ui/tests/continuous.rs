mod common;
#[path = "support/continuous_scene.rs"]
mod scene;
use egui_kittest::{Harness, kittest::Queryable};
use playtime_protocol::api::*;
use playtime_ui::{
    transcript::{Transcript, View},
    ui::Workbench,
};

#[test]
fn continuous_calls_and_compaction_remain_readable_across_backfill() {
    let build = |reverse: bool| {
        let mut t = Transcript::default();
        let mut events = scene::events();
        if reverse {
            events.reverse();
        }
        for e in events {
            t.apply(e, None).unwrap();
        }
        assert_eq!(t.sessions(0, true).total, 0);
        let p = t.progress(true);
        assert_eq!(p.limits.unwrap().tokens, 16384);
        assert!(p.session.is_none() && p.turn.is_none() && p.completed.is_none());
        t.window(View::Readable, 0, 64)
            .rows
            .into_iter()
            .map(|r| {
                (
                    r.title,
                    r.status,
                    r.parent,
                    r.sections
                        .into_iter()
                        .map(|s| (s.label, s.text))
                        .collect::<Vec<_>>(),
                )
            })
            .collect::<Vec<_>>()
    };
    let forward = build(false);
    assert_eq!(forward, build(true));
    assert!(forward.iter().any(|r| r.0 == "Call 2 · Compaction summary"));
    assert!(forward.iter().any(|r| {
        r.0 == "Compaction 1 · applied"
            && r.3
                .iter()
                .any(|(label, text)| label == "Applied summary" && text.contains("stage 12"))
    }));
    assert!(forward.iter().any(|r| r.0 == "Tool · exec"
        && r.1 == "Exit status: 0"
        && r.2.as_deref() == Some("Call 1 · Work")));
    assert!(forward.iter().any(|r| r.0 == "Model declared completion"));
}

type ReadableRow = (String, String, bool, Vec<(String, String)>);
/// Readable rows, checked to match when history arrives in reverse.
fn readable(events: Vec<playtime_protocol::events::EventEnvelope>) -> Vec<ReadableRow> {
    let build = |events: &mut dyn Iterator<Item = playtime_protocol::events::EventEnvelope>| {
        let mut t = Transcript::default();
        for e in events {
            t.apply(e, None).unwrap();
        }
        t.window(View::Readable, 0, 64)
            .rows
            .into_iter()
            .map(|r| {
                (
                    r.title,
                    r.status,
                    r.marker,
                    r.sections
                        .into_iter()
                        .map(|s| (s.label, s.text))
                        .collect::<Vec<_>>(),
                )
            })
            .collect::<Vec<_>>()
    };
    let forward = build(&mut events.clone().into_iter());
    assert_eq!(forward, build(&mut events.into_iter().rev()));
    forward
}
fn row<'a>(rows: &'a [ReadableRow], title: &str) -> &'a ReadableRow {
    rows.iter()
        .find(|r| r.0 == title)
        .unwrap_or_else(|| panic!("missing row {title}: {rows:#?}"))
}

#[test]
fn summary_at_its_token_limit_explains_why_the_run_stopped() {
    let rows = readable(scene::failed_summary_events());
    // A normal stop leaves the response state; the capped summary replaces it.
    assert_eq!(row(&rows, "Call 1 · Work").1, "Response recorded");
    let summary = row(&rows, "Call 2 · Compaction summary");
    assert_eq!(summary.1, "Stopped at its token limit");
    assert!(summary.3.contains(&(
        "Completion".into(),
        "The response reached its token limit before it finished (finish reason: length).".into()
    )));
    let plan = row(&rows, "Compaction 1 · planned");
    assert_eq!(
        plan.3,
        [
            (
                "Context estimate".into(),
                "Work request: 5,300 tokens projected, from server usage plus new bytes\nSummary request: 14,000 input tokens, from input bytes".into()
            ),
            (
                "Conversation selection".into(),
                "1 older exchange · 0 recent exchanges retained. Inspect source for exact before-conversation and request links.".into()
            ),
        ]
    );
    let stop = row(&rows, "Controller stopped: compaction failed");
    assert!(stop.2 && stop.3.is_empty());
    assert_eq!(
        row(&rows, "Controller diagnostics").3,
        [(
            "Diagnostics".into(),
            "summary requires explicit normal completion".into()
        )]
    );
    assert!(
        !rows
            .iter()
            .flat_map(|r| &r.3)
            .any(|s| s.0 == "Recorded data")
    );
}

#[test]
fn rejections_show_their_reason_and_detail() {
    let rows = readable(scene::rejected_events());
    let call = row(&rows, "Call 1 · Work");
    assert_eq!(call.1, "Tool calls rejected · the run stops");
    assert_eq!(
        call.3,
        [
            ("Rejected".into(), "Invalid completion".into()),
            (
                "Error".into(),
                "finish_run requires a normally completed response containing only that call"
                    .into()
            ),
        ]
    );
    let rejected = row(&rows, "Compaction rejected");
    assert_eq!(rejected.1, "Rejected · the run stops");
    assert_eq!(
        rejected.3,
        [
            ("Reason".into(), "Context limit".into()),
            (
                "Detail".into(),
                "summary request cannot fit with its allowance and safety margin".into()
            ),
        ]
    );
}

#[tokio::test]
async fn compaction_editor_preserves_invalid_input_and_emits_explicit_settings() {
    let mut app = common::harness_editor().await;
    app.model.authoring.draft = None;
    app.model.edit_recipe(true);
    let mut h = Harness::builder()
        .with_size([1440.0, 1200.0])
        .build_ui_state(|ui, app: &mut Workbench| app.draw(ui), app);
    h.run();
    h.get_by_label("Harness").click();
    h.run();
    h.get_by_label("Continuous conversation with compaction · v1")
        .click();
    h.run();
    assert!(h.query_by_label("Context trigger (%)").is_some());
    assert!(h.query_by_label("Memento handoff limit (tokens)").is_none());
    let draft = h.state_mut().model.authoring.draft.as_mut().unwrap();
    draft.compaction.trigger = "-".into();
    assert!(!draft.valid());
    assert!(draft.request().is_err());
    assert_eq!(draft.compaction.trigger, "-");
    draft.compaction.trigger = "70".into();
    let RecipeMutation::Clone { request } = draft.request().unwrap() else {
        panic!()
    };
    assert_eq!(request.compaction.unwrap().trigger_percent, 70);
    assert_eq!(request.harness.unwrap().as_str(), CONTINUOUS_COMPACTION_V1);
    draft.harness = Some(HarnessId::new(MEMENTO_TURNS_V1).unwrap());
    let RecipeMutation::Clone { request } = draft.request().unwrap() else {
        panic!()
    };
    assert!(request.compaction.is_none());
}

#[tokio::test]
async fn continuous_drafts_ignore_the_hidden_handoff_field() {
    let app = common::model_settings_editor().await;
    let mut h = Harness::builder()
        .with_size([1440.0, 1200.0])
        .build_ui_state(|ui, app: &mut Workbench| app.draw(ui), app);
    h.run();
    let field = "Memento handoff limit (tokens)";
    h.get_by_label(field).focus();
    h.run();
    h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::A);
    h.run();
    h.get_by_label(field).type_text("not a number");
    h.run();
    let draft = h.state_mut().model.authoring.draft.as_mut().unwrap();
    assert!(draft.request().is_err());
    // Once the field is hidden, its unsaved text can't block the save.
    draft.harness = Some(HarnessId::new(CONTINUOUS_COMPACTION_V1).unwrap());
    let RecipeMutation::Clone { request } = draft.request().unwrap() else {
        panic!()
    };
    assert_eq!(request.model_settings.unwrap().handoff_tokens, 8192);
}

#[tokio::test]
async fn final_files_replace_the_inactive_memory_panel_and_reject_stale_replies() {
    use playtime_ui::model::{Data, Inspector, Reply};
    let mut app = common::demo(false).await;
    app.model.run.as_mut().unwrap().value.harness = Some(ArchivedHarness {
        configuration: app
            .model
            .run
            .as_ref()
            .unwrap()
            .value
            .input_manifest
            .clone()
            .unwrap(),
        harness: HarnessView::continuous(Default::default()).unwrap(),
    });
    app.model.inspector = Inspector::Memory;
    let mut h = Harness::builder()
        .with_size([1440.0, 900.0])
        .build_ui_state(|ui, app: &mut Workbench| app.draw(ui), app);
    h.run();
    assert_eq!(h.state().model.inspector, Inspector::Files);
    assert!(h.query_by_label("Checkpoint").is_none());
    assert!(h.query_by_label("Load final workspace files").is_some());
    let model = &mut h.state_mut().model;
    let tag = model.tag;
    model.tag.inspection += 1;
    model.apply(Reply {
        tag,
        run: model.selected.clone(),
        data: Data::FinalFiles(vec![], true),
    });
    assert!(model.final_files.is_none());
    model.apply(Reply {
        tag: model.tag,
        run: model.selected.clone(),
        data: Data::FinalFiles(vec![], false),
    });
    assert_eq!(
        model.final_files.as_ref().map(|(_, complete)| *complete),
        Some(false)
    );
    h.run();
    assert!(
        h.query_by_label("Listing limit reached; this listing is incomplete.")
            .is_some()
    );
}
