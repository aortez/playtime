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
        && r.2.as_deref() == Some("Call 1 · work")));
    assert!(forward.iter().any(|r| r.0 == "Model declared completion"));
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
