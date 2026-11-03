mod common;
#[path = "support/continuous_scene.rs"]
mod scene;
use egui_kittest::{Harness, kittest::Queryable};
use playtime_protocol::api::*;
use playtime_ui::{
    progress::{Compactions, ContextUsage, lead},
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
    // Either way the call's duration follows.
    assert_eq!(row(&rows, "Call 1 · Work").1, "Response recorded · 1.0 s");
    let summary = row(&rows, "Call 2 · Compaction summary");
    assert_eq!(summary.1, "Stopped at its token limit · 1.0 s");
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
    assert_eq!(call.1, "Tool calls rejected · the run stops · 1.0 s");
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
    // Either rejection stops the run, so both lead the jump list.
    let mut t = Transcript::default();
    for e in scene::rejected_events() {
        t.apply(e, None).unwrap();
    }
    assert_eq!(
        t.jumps(0, true)
            .items
            .into_iter()
            .map(|jump| (jump.sequence, jump.label))
            .collect::<Vec<_>>(),
        [
            (4, "0:04 · Compaction rejected".into()),
            (
                3,
                "0:03 · Tool calls rejected: invalid completion · call 1".into()
            ),
        ]
    );
}

#[test]
fn the_results_scene_reads_in_either_order() {
    let rows = readable(scene::results_events());
    assert_eq!(
        row(&rows, "Call 1 · Work").1,
        "Response recorded · 2.0 s · 900 prompt tokens · 120 generated · 60 tokens/s"
    );
    assert_eq!(
        row(&rows, "Call 4 · Compaction summary").1,
        "Response recorded · 2.0 s · 11,800 prompt tokens · 36 generated · 18 tokens/s"
    );
    let captured = row(
        &rows,
        "Final stopped workspace captured · open Files to inspect",
    );
    assert!(captured.2 && captured.3.is_empty());
}

/// The contract fixture's completed run, as the header tests use it.
fn completed_run() -> RunView {
    let contract: serde_json::Value = serde_json::from_str(include_str!(
        "../../playtime-server/tests/fixtures/http-contract.json"
    ))
    .unwrap();
    serde_json::from_value(contract["completed_run"].clone()).unwrap()
}

#[test]
fn calls_show_their_cost_and_the_header_sums_context_and_compactions() {
    let rows = readable(scene::events());
    assert_eq!(
        row(&rows, "Call 1 · Work").1,
        "Response recorded · 1.0 s · 1,200 prompt tokens · 48 generated · 48 tokens/s"
    );
    assert_eq!(
        row(&rows, "Call 2 · Compaction summary").1,
        "Response recorded · 1.0 s · 11,800 prompt tokens · 36 generated · 36 tokens/s"
    );
    // A tool has no cost of its own.
    assert_eq!(row(&rows, "Tool · exec").1, "Exit status: 0");
    let mut t = Transcript::default();
    for e in scene::events() {
        t.apply(e, None).unwrap();
    }
    // Until earlier history has loaded the counts would be partial, so the
    // header shows none of them.
    let loading = t.progress(false);
    assert!(loading.context.is_none() && loading.compactions.is_none());
    assert_eq!((loading.model_errors, loading.failed_tools), (0, 0));
    let p = t.progress(true);
    assert_eq!(
        p.context,
        Some(ContextUsage {
            tokens: 1150,
            basis: "input_bytes".into(),
            capacity: Some(32768),
        })
    );
    assert_eq!(
        p.compactions,
        Some(Compactions {
            trigger_percent: Some(75),
            planned: 1,
            applied: 1,
            deferred: 0,
            rejected: 0,
        })
    );
    assert_eq!((p.model_errors, p.failed_tools), (0, 0));
    let lines = lead(&completed_run(), &p);
    assert_eq!(
        lines[2..],
        [
            "Context: 1,150 of 32,768 tokens from input bytes · compaction trigger 75% · compactions: 1 planned · 1 applied"
        ]
    );
}

#[test]
fn the_header_counts_errors_failed_tools_deferrals_and_rejections() {
    let rows = readable(scene::troubled_events());
    assert_eq!(
        row(&rows, "Call 1 · Work").1,
        "Model error · partial inference may be present · 1.0 s"
    );
    assert_eq!(
        row(&rows, "Call 2 · Work").1,
        "Response recorded · 1.0 s · 20,480 prompt tokens · 10 generated · 10 tokens/s"
    );
    assert_eq!(row(&rows, "Tool · exec").1, "Failed · exit status: 1");
    let mut t = Transcript::default();
    for e in scene::troubled_events() {
        t.apply(e, None).unwrap();
    }
    let p = t.progress(true);
    assert_eq!(
        p.context,
        Some(ContextUsage {
            tokens: 20480,
            basis: "calibrated_prefix_plus_new_bytes".into(),
            capacity: Some(32768),
        })
    );
    assert_eq!(
        p.compactions,
        Some(Compactions {
            trigger_percent: Some(50),
            planned: 1,
            applied: 0,
            deferred: 1,
            rejected: 1,
        })
    );
    assert_eq!((p.model_errors, p.failed_tools), (1, 1));
    assert_eq!(
        lead(&completed_run(), &p)[2..],
        [
            "Context: 20,480 of 32,768 tokens from calibrated prefix plus new bytes · compaction trigger 50% · compactions: 1 planned · 0 applied · 1 deferred · 1 rejected",
            "1 model error · 1 failed tool result",
        ]
    );
}

#[test]
fn the_jump_list_leads_with_the_latest_compaction_or_error() {
    let build = |reverse: bool| {
        let mut t = Transcript::default();
        let mut events = scene::troubled_events();
        if reverse {
            events.reverse();
        }
        for e in events {
            t.apply(e, None).unwrap();
        }
        t
    };
    let t = build(false);
    let labels = |t: &Transcript| {
        t.jumps(0, true)
            .items
            .into_iter()
            .map(|jump| (jump.sequence, jump.label))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        labels(&t),
        [
            (11, "0:11 · Compaction rejected".into()),
            (10, "0:10 · Compaction 1 planned".into()),
            (9, "0:09 · Compaction 1 deferred".into()),
            (8, "0:08 · exec exited with status 1 · call 2".into()),
            (4, "0:04 · Model error: parse · call 1".into()),
        ]
    );
    assert_eq!(labels(&build(true)), labels(&t));
    // Each entry and each call lands on its own row.
    let window = t.window(View::Readable, 0, 64);
    let title = |sequence| {
        let (id, _) = t.location(View::Readable, sequence).unwrap();
        window
            .rows
            .iter()
            .find(|r| r.id == id)
            .unwrap()
            .title
            .clone()
    };
    assert_eq!(title(4), "Call 1 · Work");
    assert_eq!(title(8), "Tool · exec");
    assert_eq!(title(9), "Compaction 1 · deferred");
    assert_eq!(title(11), "Compaction rejected");
    assert_eq!(t.calls(), 2);
    assert_eq!(title(t.call_sequence(2).unwrap()), "Call 2 · Work");
    assert_eq!(
        (t.call_sequence(1), t.call_sequence(2), t.call_sequence(3)),
        (Some(3), Some(5), None)
    );
    assert_eq!(t.find_call(2, false), Ok(5));
    assert_eq!(
        t.find_call(3, true),
        Err("Call 3 isn't recorded; the latest recorded call is 2".into())
    );
    // Earlier history loads forward from the run's start, so a call past the
    // latest indexed may still be recorded until it has loaded.
    let mut first = Transcript::default();
    for e in scene::troubled_events().into_iter().take(4) {
        first.apply(e, None).unwrap();
    }
    assert_eq!(first.calls(), 1);
    assert_eq!(
        first.find_call(2, false),
        Err("Call 2 isn't indexed yet; earlier history is still loading".into())
    );
    assert_eq!(
        Transcript::default().find_call(1, true),
        Err("This run has no recorded model calls".into())
    );
}

/// The workbench on the troubled continuous scene, with history loaded.
async fn troubled() -> Harness<'static, Workbench> {
    let mut app = common::demo(false).await;
    let run = &mut app.model.run.as_mut().unwrap().value;
    run.harness = Some(ArchivedHarness {
        configuration: run.input_manifest.clone().unwrap(),
        harness: HarnessView::continuous(Default::default()).unwrap(),
    });
    let mut t = Transcript::default();
    for e in scene::troubled_events() {
        t.apply(e, None).unwrap();
    }
    app.model.window = t.window(View::Readable, 0, 64);
    app.model.window.jumps = t.jumps(0, true);
    app.model.window.calls.complete = true;
    app.model.connection = playtime_ui::model::ConnectionState::Connected;
    let mut h = Harness::builder()
        .with_size([1440.0, 900.0])
        .build_ui_state(|ui, app: &mut Workbench| app.draw(ui), app);
    h.run();
    h
}
fn located(h: &Harness<'_, Workbench>, sequence: u64) -> bool {
    h.state().model.commands.iter().any(|c| {
        matches!(c.action, playtime_ui::model::Action::Locate(s, View::Readable) if s == sequence)
    })
}
fn reply(h: &mut Harness<'_, Workbench>, call: u64, found: Result<u64, String>) {
    arrive(h, call, found);
    h.run();
}
/// Applies a found call's reply without running a frame.
fn arrive(h: &mut Harness<'_, Workbench>, call: u64, found: Result<u64, String>) {
    let model = &mut h.state_mut().model;
    model.apply(playtime_ui::model::Reply {
        tag: model.tag,
        run: model.selected.clone(),
        data: playtime_ui::model::Data::CallFound(call, found),
    });
}
/// Clicks `label`, calling `pressed` between the press and the release, so
/// that what it does lands in the frame where the click registers.
fn pick(
    h: &mut Harness<'_, Workbench>,
    label: &str,
    pressed: impl FnOnce(&mut Harness<'_, Workbench>),
) {
    let pos = h.get_by_label(label).rect().center();
    let button = |pressed| egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: Default::default(),
    };
    h.event(egui::Event::PointerMoved(pos));
    h.event(button(true));
    h.step();
    pressed(h);
    h.event(button(false));
    h.step();
    h.run();
}
fn type_call(h: &mut Harness<'_, Workbench>, call: &str) {
    h.state_mut().model.call_text = call.into();
    h.run();
    h.get_by_label("Go to call").click();
    h.run();
}

#[tokio::test]
async fn the_jump_list_and_a_found_call_go_to_their_records() {
    use playtime_ui::model::Action;
    let mut h = troubled().await;
    h.get_by_label("Jump to a compaction or error").click();
    h.run();
    h.get_by_label("0:08 · exec exited with status 1 · call 2")
        .click();
    h.run();
    assert!(located(&h, 8));
    assert!(!h.state().model.following);
    // The call field takes a number; the find goes to the backend.
    h.state_mut().model.commands.clear();
    type_call(&mut h, "2");
    assert!(
        h.state()
            .model
            .commands
            .iter()
            .any(|c| matches!(c.action, Action::FindCall(2)))
    );
    reply(&mut h, 2, Ok(5));
    assert!(located(&h, 5));
    // An unrecorded call says why, and the timeline stays put.
    h.state_mut().model.commands.clear();
    type_call(&mut h, "9");
    reply(
        &mut h,
        9,
        Err("Call 9 isn't recorded; the latest recorded call is 2".into()),
    );
    assert_eq!(
        h.state().model.notice,
        "Call 9 isn't recorded; the latest recorded call is 2"
    );
    assert!(
        !h.state()
            .model
            .commands
            .iter()
            .any(|c| matches!(c.action, Action::Locate(..)))
    );
    type_call(&mut h, "call two");
    assert_eq!(h.state().model.notice, "Enter a call number, such as 12");
    // A pick replaces a find still waiting for its reply, whether the reply
    // arrives later or in the pick's own frame.
    for same_frame in [false, true] {
        h.state_mut().model.commands.clear();
        type_call(&mut h, "1");
        h.get_by_label("Jump to a compaction or error").click();
        h.run();
        pick(&mut h, "0:09 · Compaction 1 deferred", |h| {
            if same_frame {
                arrive(h, 1, Ok(3));
            }
        });
        reply(&mut h, 1, Ok(3));
        assert!(located(&h, 9));
        assert!(!located(&h, 3), "same frame: {same_frame}");
    }
    // Jumping to live drops a find still waiting for its reply.
    type_call(&mut h, "1");
    h.state_mut().model.jump_live();
    reply(&mut h, 1, Ok(3));
    assert!(!located(&h, 3));
    assert!(h.state().model.following);
    // Without the run's stream, a find would go unanswered.
    h.state_mut().model.commands.clear();
    h.state_mut().model.connection =
        playtime_ui::model::ConnectionState::Error("stream ended".into());
    type_call(&mut h, "2");
    assert_eq!(h.state().model.notice, "Reconnect to go to a call");
    assert!(
        !h.state()
            .model
            .commands
            .iter()
            .any(|c| matches!(c.action, Action::FindCall(..)))
    );
}

#[tokio::test]
async fn an_opened_text_file_shows_its_name_and_pages_only_when_it_must() {
    use playtime_ui::model::{Action, ContentView, Inspector};
    let mut app = common::demo(false).await;
    let artifact = |name: &str, size: usize| Artifact {
        artifact_id: ArtifactId::new("notes").unwrap(),
        name: name.into(),
        kind: "file".into(),
        content: playtime_protocol::archive::ContentRef {
            path: playtime_core::ArchivePath::new("artifacts/notes").unwrap(),
            sha256: playtime_core::Sha256Digest::new("a".repeat(64)).unwrap(),
            size_bytes: size as u64,
        },
        session_id: None,
        published_sequence: 1,
        frozen_input: false,
    };
    let text = "line of notes\n".repeat(5000);
    app.model.content = Some(ContentView {
        artifact: artifact("notes.md", text.len()),
        source: Some(playtime_ui::transcript::inspector_page(&text, 0)),
        image: None,
        notice: String::new(),
        animated: false,
    });
    app.model.inspector = Inspector::Files;
    let mut h = Harness::builder()
        .with_size([1440.0, 900.0])
        .build_ui_state(|ui, app: &mut Workbench| app.draw(ui), app);
    h.run();
    assert!(h.query_by_label("notes.md").is_some());
    assert!(h.query_by_label("68.4 KiB").is_some());
    assert!(
        h.query_by_label("Showing bytes 1–24,576 of 70,000")
            .is_some()
    );
    // The inspector draws first; its page leaves the timeline its text.
    assert!(h.query_by_label_contains("Text viewport limit").is_none());
    h.get_by_label("Next file page").click();
    h.run();
    assert!(
        h.state()
            .model
            .commands
            .iter()
            .any(|c| matches!(&c.action, Action::Content(_, id, 24576) if id.as_str() == "notes"))
    );
    // The page read scrolls to the file once it arrives.
    assert!(h.state().model.reveal_content);
    let short = "one line\n";
    h.state_mut().model.content = Some(ContentView {
        artifact: artifact("short.txt", short.len()),
        source: Some(playtime_ui::transcript::inspector_page(short, 0)),
        image: None,
        notice: String::new(),
        animated: false,
    });
    h.run();
    assert!(!h.state().model.reveal_content);
    assert!(h.query_by_label("short.txt").is_some());
    assert!(h.query_by_label("Next file page").is_none());
    assert!(h.query_by_label_contains("Showing bytes").is_none());
    h.get_by_label("Close file").click();
    h.run();
    assert!(h.state().model.content.is_none());
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
    let limit_note = "Without finish_run, the run lasts until its time or token limit.";
    assert!(h.query_by_label(limit_note).is_none());
    h.get_by_label("The model can end the run with finish_run")
        .click();
    h.run();
    assert!(h.query_by_label(limit_note).is_some());
    let draft = h.state_mut().model.authoring.draft.as_mut().unwrap();
    draft.compaction.trigger = "-".into();
    assert!(!draft.valid());
    assert!(draft.request().is_err());
    assert_eq!(draft.compaction.trigger, "-");
    draft.compaction.trigger = "70".into();
    let RecipeMutation::Clone { request } = draft.request().unwrap() else {
        panic!()
    };
    let compaction = request.compaction.unwrap();
    assert_eq!(compaction.trigger_percent, 70);
    assert!(!compaction.allow_finish_run);
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
    // The listing is the run's: a later inspection doesn't make its reply
    // stale, but another selection does.
    let tag = model.tag;
    model.tag.inspection += 1;
    model.apply(Reply {
        tag,
        run: model.selected.clone(),
        data: Data::FinalFiles(vec![], true),
    });
    assert_eq!(
        model
            .final_files
            .as_ref()
            .map(|(files, complete)| (files.len(), *complete)),
        Some((0, true))
    );
    model.final_files = None;
    let tag = model.tag;
    model.tag.selection += 1;
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

#[test]
fn readback_says_when_the_model_cannot_end_the_run() {
    let note = "No finish_run · the run lasts until its time or token limit";
    let mut view = HarnessView {
        id: CONTINUOUS_COMPACTION_V1.into(),
        supported: true,
        version: Some(1),
        settings: None,
        compaction: Some(CompactionSettings::default()),
    };
    assert!(!playtime_ui::harness::lines(&view).iter().any(|l| l == note));
    view.compaction.as_mut().unwrap().allow_finish_run = false;
    assert_eq!(playtime_ui::harness::lines(&view).last().unwrap(), note);
}

#[tokio::test]
async fn read_in_full_pages_a_long_response_in_the_inspector() {
    use playtime_ui::{
        model::{Action, Data, Reply, Tag},
        transcript::{PAGE_BYTES, TextRef, thousands},
    };
    let mut app = common::demo(false).await;
    app.model.content = None;
    let mut t = Transcript::default();
    for e in scene::long_response_events() {
        t.apply(e, None).unwrap();
    }
    app.model.window = t.window(View::Readable, 0, 64);
    let mut h = Harness::builder()
        .with_size([1440.0, 900.0])
        .build_ui_state(|ui, app: &mut Workbench| app.draw(ui), app);
    h.run();
    let total = scene::long_text().len();
    h.get_by_label("Excerpt of 26.0 KiB");
    h.get_by_label("Read Model response 2").click();
    h.run();
    let asked = |h: &Harness<'_, Workbench>| {
        h.state()
            .model
            .commands
            .iter()
            .rev()
            .find_map(|c| match c.action {
                Action::Text(text, offset) => Some((text, offset)),
                _ => None,
            })
            .unwrap()
    };
    let (text, offset) = asked(&h);
    assert_eq!((text.bytes, offset), (total, 0));
    h.get_by_label("Call 1 · Work · Model response");
    h.get_by_label("Loading text…");
    // The window task answers from the index, and only the latest page
    // request's answer applies.
    let answer = |h: &mut Harness<'_, Workbench>, tag: Tag, text: TextRef, offset| {
        let model = &mut h.state_mut().model;
        model.apply(Reply {
            tag,
            run: model.selected.clone(),
            data: Data::Text(text, Ok(t.text(text, offset).unwrap())),
        });
        h.run();
    };
    let first = h.state().model.tag;
    answer(&mut h, first, text, 0);
    let end = t.text(text, 0).unwrap().text.len();
    h.get_by_label(&format!(
        "Showing bytes 1–{} of {}",
        thousands(end as u64),
        thousands(total as u64)
    ));
    h.get_by_label("Next text page").click();
    h.run();
    assert_eq!(asked(&h), (text, end));
    // An answer for an earlier request leaves the page shown.
    answer(&mut h, first, text, end);
    let page = |h: &Harness<'_, Workbench>| {
        h.state()
            .model
            .reading
            .as_ref()
            .and_then(|r| r.page.as_ref())
            .map(|p| p.start)
    };
    assert_eq!(page(&h), Some(0));
    let tag = h.state().model.tag;
    answer(&mut h, tag, text, end);
    h.get_by_label(&format!(
        "Showing bytes {}–{} of {}",
        thousands(end as u64 + 1),
        thousands(total as u64),
        thousands(total as u64)
    ));
    h.get_by_label("Previous text page").click();
    h.run();
    assert_eq!(asked(&h), (text, end.saturating_sub(PAGE_BYTES)));
    h.get_by_label("Close text").click();
    h.run();
    assert!(h.state().model.reading.is_none());
    assert!(h.query_by_label("Close text").is_none());
}

#[tokio::test]
async fn the_inspector_keeps_to_its_share_of_each_frame_text() {
    use playtime_ui::{
        model::Action,
        transcript::{INSPECTOR_TEXT_BYTES, inspector_page, thousands},
    };
    let mut app = common::demo(false).await;
    // A source page and an open file's page would take 48 KiB of the frame's
    // 64 KiB, before the timeline draws.
    let text = scene::long_text();
    app.model.window = Default::default();
    app.model.selected_row = Some(0);
    app.model.source = Some(inspector_page(&text, 0));
    let content = app.model.content.as_mut().unwrap();
    content.image = None;
    content.source = Some(inspector_page(&text, 0));
    let mut h = Harness::builder()
        .with_size([1440.0, 900.0])
        .build_ui_state(|ui, app: &mut Workbench| app.draw(ui), app);
    h.run();
    assert!(h.state().metrics.text_bytes <= INSPECTOR_TEXT_BYTES);
    h.get_by_label("Text viewport limit reached · inspect the next source page for continuation");
    // The file shows what the source page leaves of the share, and its range
    // and Next button follow that part, so paging skips no byte.
    let mut shown = INSPECTOR_TEXT_BYTES - h.state().model.source.as_ref().unwrap().text.len();
    while !text.is_char_boundary(shown) {
        shown -= 1;
    }
    h.get_by_label(&format!(
        "Showing bytes 1–{} of {}",
        thousands(shown as u64),
        thousands(text.len() as u64)
    ));
    // The pager sits below the source page, out of view.
    h.get_by_label("Next file page").click_accesskit();
    h.run();
    let asked = h
        .state()
        .model
        .commands
        .iter()
        .rev()
        .find_map(|c| match c.action {
            Action::Content(_, _, offset) => Some(offset),
            _ => None,
        });
    assert_eq!(asked, Some(shown));
}

#[test]
fn an_evicted_record_gives_the_same_text_once_applied_again() {
    let mut t = Transcript::default();
    let events = scene::long_response_events();
    for e in events.clone() {
        t.apply(e, None).unwrap();
    }
    let total = scene::long_text().len();
    let text = t
        .window(View::Readable, 0, 64)
        .rows
        .iter()
        .flat_map(|r| r.texts.iter().flatten())
        .copied()
        .find(|text| text.bytes == total)
        .unwrap();
    let page = t.text(text, 0).unwrap();
    // Later responses, each under the inline limit, push it out of the
    // payload cache.
    let filler = "x".repeat(200 << 10);
    for call in 2..=60 {
        t.apply(scene::response(call + 2, call, &filler), None)
            .unwrap();
    }
    assert!(t.text(text, 0).is_none());
    // A reload redelivers the record, which projects the same sections.
    t.apply(events[2].clone(), None).unwrap();
    let reloaded = t.text(text, 0).unwrap();
    assert_eq!((reloaded.text, reloaded.total), (page.text, page.total));
}

#[test]
fn proportional_text_draws_the_characters_models_write() {
    let mut h = Harness::builder()
        .build_ui_state(|ui, app: &mut Workbench| app.draw(ui), Workbench::default());
    h.run();
    assert!(h.ctx.fonts_mut(|fonts| {
        fonts.has_glyphs(&egui::FontId::proportional(14.0), "\u{2011}\u{202f}→")
    }));
}

#[test]
fn a_call_settled_without_a_response_is_shown_cut_off() {
    let rows = readable(scene::cut_off_events());
    // The failure recorded after the first call's settlement names itself.
    assert_eq!(
        row(&rows, "Call 1 · Work").1,
        "Model error · partial inference may be present · 3.0 s"
    );
    // The run's stop left the second call without a response.
    let cut = row(&rows, "Call 2 · Work");
    assert_eq!(
        cut.1,
        "Cut off before a response · 8,192 tokens charged as unknown usage · 2.0 s"
    );
    assert!(cut.3.iter().any(|(_, text)| text == "cut by the wall"));
}

/// The narrow no-break space that models write is shown as a no-break space,
/// which the fonts have at a space's width.
#[tokio::test]
async fn narrow_no_break_spaces_are_shown_as_no_break_spaces() {
    let mut app = common::demo(false).await;
    let mut t = Transcript::default();
    for event in scene::long_response_events() {
        t.apply(event, None).unwrap();
    }
    app.model.window = t.window(View::Readable, 0, 64);
    let h = Harness::builder()
        .with_size([1440.0, 900.0])
        .build_ui_state(|ui, app: &mut Workbench| app.draw(ui), app);
    h.get_by_label_contains("side\u{a0}by\u{a0}side");
    assert!(h.query_by_label_contains("\u{202f}").is_none());
}

#[test]
fn a_delayed_archive_write_is_named_and_listed_among_the_jumps() {
    let rows = readable(scene::delayed_events());
    let delayed = row(&rows, "Archive write delayed · 6.2 s");
    assert!(delayed.3.iter().any(|(label, text)| {
        label == "Delay"
            && text
                == "3 events (47,827 bytes) took 6.2 s to commit; no record was published meanwhile."
    }));
    let mut t = Transcript::default();
    for event in scene::delayed_events() {
        t.apply(event, None).unwrap();
    }
    assert!(
        t.jumps(0, true)
            .items
            .iter()
            .any(|jump| jump.label.contains("Archive write delayed 6.2 s")),
        "{:?}",
        t.jumps(0, true).items
    );
}
