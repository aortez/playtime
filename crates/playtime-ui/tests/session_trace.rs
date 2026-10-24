use egui_kittest::{Harness, kittest::Queryable};
use playtime_core::{RunId, SessionId};
use playtime_protocol::events::EventEnvelope;
use playtime_ui::{model::*, transcript::*, ui::Workbench};
use serde_json::json;

fn memory(ordinal: u32, sequence: u64, status: &str, text: &str) -> EventEnvelope {
    let mut event: EventEnvelope = serde_json::from_str(
        include_str!(
            "../../playtime-protocol/tests/fixtures/controller-observations/completed.jsonl"
        )
        .lines()
        .nth(3)
        .unwrap(),
    )
    .unwrap();
    event.sequence = sequence.try_into().unwrap();
    event.data["session_id"] = json!(format!("s{ordinal}"));
    event.data["data"]["value"]["ordinal"] = json!(ordinal);
    event.data["data"]["value"]["memory"] = json!({"status":status,"content":text});
    event
}
fn started(ordinal: u32, sequence: u64) -> EventEnvelope {
    let mut event: EventEnvelope = serde_json::from_str(
        include_str!("../../playtime-protocol/tests/fixtures/two-session/events.jsonl")
            .lines()
            .nth(2)
            .unwrap(),
    )
    .unwrap();
    event.run_id = RunId::new("simulation").unwrap();
    event.sequence = sequence.try_into().unwrap();
    event.data["session_id"] = json!(format!("s{ordinal}"));
    event.data["ordinal"] = json!(ordinal);
    event
}
fn reply(model: &mut Model, data: Data) {
    model.apply(Reply {
        tag: model.tag,
        run: model.selected.clone(),
        data,
    });
}
#[test]
fn sessions_are_recorded_paged_and_survive_eviction_and_reversed_history() {
    let mut transcript = Transcript::default();
    for n in (1..=140).rev() {
        transcript
            .apply(memory(n, n as u64 * 10, "loaded", "exact delivery"), None)
            .unwrap();
    }
    let first = transcript.sessions(0, false);
    assert_eq!(
        (first.total, first.items.len(), first.complete),
        (140, 64, false)
    );
    assert_eq!(first.items[0].ordinal, 1);
    let page = transcript.sessions(64, true);
    assert_eq!(page.items[0].ordinal, 65);
    assert_eq!(transcript.sessions(128, true).items.len(), 12);
    let target = page.items[0].clone();
    let at = transcript.position(View::Readable, target.row).unwrap();
    assert_eq!(at, 64);
    // Evict payloads, keeping session navigation and exact source identities.
    for n in 0..20 {
        let mut event = memory(200 + n, 10000 + n as u64, "loaded", &"x".repeat(500_000));
        event.kind = "future_event".into();
        transcript.apply(event, None).unwrap();
    }
    assert!(!transcript.contains(target.sequence));
    assert_eq!(
        transcript.sessions(64, true).items[0].sequence,
        target.sequence
    );
    assert!(transcript.window(View::Readable, at, 1).rows[0].needs_loading);
    transcript
        .apply(memory(65, 650, "loaded", "exact delivery"), None)
        .unwrap();
    let row = &transcript.window(View::Readable, at, 1).rows[0];
    assert!(!row.needs_loading);
    assert_eq!(row.sections[0].text, "exact delivery");
    assert!(
        transcript
            .raw(target.sequence, 0)
            .unwrap()
            .text
            .contains("exact delivery")
    );
}
#[test]
fn canonical_start_and_observed_delivery_have_one_readable_heading_in_either_order() {
    for reverse in [false, true] {
        let mut events = vec![memory(1, 6, "missing", ""), started(1, 9)];
        if reverse {
            events.reverse();
        }
        let mut transcript = Transcript::default();
        for e in events {
            transcript.apply(e, None).unwrap();
        }
        let window = transcript.window(View::Readable, 0, 64);
        assert_eq!(window.total, 1);
        assert_eq!(window.rows[0].title, "Session 1");
        assert_eq!(window.rows[0].sections[0].text, "No memento supplied.");
        assert_eq!(window.sessions.total, 1);
        assert_eq!(window.sessions.items[0].sequence, 6);
        assert_eq!(transcript.window(View::Full, 0, 64).total, 2);
        assert!(transcript.raw(9, 0).is_some());
        transcript
            .apply(memory(2, 20, "loaded", "next session"), None)
            .unwrap();
        // A jump requested before coalescing still names canonical sequence 9.
        // Resolve it to this session's delivery, not the following session.
        let (id, at) = transcript.location(View::Readable, 9).unwrap();
        assert_eq!(at, 0);
        assert_eq!(transcript.window(View::Readable, at, 1).rows[0].id, id);
        assert_eq!(
            transcript.window(View::Readable, at, 1).rows[0].first_sequence,
            6
        );
        assert_eq!(
            transcript.location(View::Full, 9).unwrap().0,
            transcript.row_for(9).unwrap()
        );
    }
}
#[test]
fn delivered_missing_empty_unavailable_and_old_workspace_memory_are_distinct() {
    let mut transcript = Transcript::default();
    for (n, status, content) in [
        (1, "missing", ""),
        (2, "loaded", ""),
        (3, "oversized", ""),
        (4, "loaded", "older memory reused"),
    ] {
        transcript
            .apply(memory(n, n as u64 * 10, status, content), None)
            .unwrap();
    }
    let rows = transcript.window(View::Readable, 0, 64).rows;
    assert_eq!(rows[0].sections[0].text, "No memento supplied.");
    assert_eq!(rows[1].sections[0].text, "Available empty memento");
    assert!(rows[2].sections[0].text.contains("unavailable"));
    assert!(rows[2].status.contains("oversized"));
    assert_eq!(rows[3].sections[0].text, "older memory reused");
    // Old archives can name a workspace memento without an explicit delivery.
    let mut start = started(5, 50);
    let content = json!({"path":"sessions/s4/memento.txt","sha256":"a".repeat(64),"size_bytes":20});
    start.data["memento"] = json!({"status":"available","content":content});
    transcript.apply(start.clone(), None).unwrap();
    let row = &transcript.window(View::Readable, 4, 1).rows[0];
    assert!(row.trace.is_none());
    assert!(row.sections[0].text.contains("Exact delivery unavailable"));
    start.sequence = 60.try_into().unwrap();
    start.data["session_id"] = json!("s6");
    start.data["ordinal"] = json!(6);
    start.data["delivery"] =
        json!({"memento":{"status":"loaded","content":content},"evidence":null});
    transcript.apply(start, None).unwrap();
    assert!(matches!(
        transcript.window(View::Readable, 5, 1).rows[0].trace,
        Some(TraceContent::Incoming(_))
    ));
    // No publication event means no invented outgoing checkpoint.
    assert!(
        transcript
            .window(View::Readable, 0, 64)
            .rows
            .iter()
            .all(|r| r.checkpoint.is_none())
    );
}
#[test]
fn trace_reads_are_bounded_and_do_not_replace_a_newer_run_or_inspector() {
    let mut model = Model::default();
    model.select_run(RunId::new("simulation").unwrap());
    model.commands.clear();
    let window = Window {
        total: 64,
        rows: (1..=64)
            .map(|sequence| Row {
                first_sequence: sequence,
                trace: Some(TraceContent::Checkpoint(
                    SessionId::new(format!("s{sequence}")).unwrap(),
                )),
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    };
    reply(&mut model, Data::Window(window, "history".into(), 0));
    assert_eq!(
        model
            .commands
            .iter()
            .filter(|c| matches!(c.action, Action::Trace(..)))
            .count(),
        2
    );
    let old = model.tag;
    model.inspect_checkpoint(SessionId::new("s2").unwrap(), Inspector::Memory);
    let inspection = model.tag.inspection;
    reply(
        &mut model,
        Data::Trace(
            1,
            TraceView {
                notice: "published".into(),
                ..Default::default()
            },
        ),
    );
    assert_eq!(model.tag.inspection, inspection);
    assert_eq!(model.inspector, Inspector::Memory);
    assert!(model.checkpoint.is_none());
    assert_eq!(model.trace.len(), 1);
    model.select_run(RunId::new("different-run").unwrap());
    model.apply(Reply {
        tag: old,
        run: Some(RunId::new("simulation").unwrap()),
        data: Data::Trace(2, TraceView::default()),
    });
    assert!(model.trace.is_empty());
    model.apply(Reply {
        tag: old,
        run: Some(RunId::new("simulation").unwrap()),
        data: Data::Window(
            Window {
                total: 100,
                location: Some((40, 3)),
                ..Default::default()
            },
            "stale navigation".into(),
            10,
        ),
    });
    assert!(model.location.is_none());
    assert_eq!(model.window.total, 0);
    // Even queue/connection errors that have no run tag are selection checked.
    model.apply(Reply {
        tag: old,
        run: None,
        data: Data::Trace(2, TraceView::default()),
    });
    assert!(model.trace.is_empty());
}
#[test]
fn session_chooser_locates_a_cold_target_and_reconnect_uses_stable_sequence() {
    let mut transcript = Transcript::default();
    for n in 1..=60 {
        transcript
            .apply(memory(n, n as u64 * 10, "loaded", "session memory"), None)
            .unwrap();
    }
    let mut app = Workbench::default();
    app.model.launcher = false;
    app.model.connection = ConnectionState::Connected;
    app.model.selected = Some(RunId::new("simulation").unwrap());
    app.model.following = false;
    app.model.window = transcript.window(View::Readable, 0, 4);
    let mut h = Harness::builder()
        .with_size([900.0, 600.0])
        .build_ui_state(|ui, app: &mut Workbench| app.draw(ui), app);
    h.run();
    // The chooser's bounded popup includes the cold target.
    h.get_by_role(egui::accesskit::Role::ComboBox).click();
    h.run();
    h.get_by_label("Session 8 · s8").click();
    h.run();
    assert!(
        h.state()
            .model
            .commands
            .iter()
            .any(|c| matches!(c.action, Action::Locate(80, View::Readable)))
    );
    assert!(!h.state().model.following);
    // Reconnect while the navigation reply is still pending must retain the
    // chosen destination rather than reverting to the old visible session.
    h.state_mut().model.tag.connection += 1;
    h.state_mut().model.commands.clear();
    h.run();
    assert!(
        h.state()
            .model
            .commands
            .iter()
            .any(|c| matches!(c.action, Action::Locate(80, _)))
    );
    let target = transcript.sessions(0, true).items[7].clone();
    let mut window = transcript.window(View::Readable, 5, 20);
    window.location = Some((80, target.row));
    reply(
        &mut h.state_mut().model,
        Data::Window(window, "complete".into(), 0),
    );
    h.run_steps(30);
    let before = h.get_by_label("Inspect source 80").rect().top();
    transcript
        .apply(memory(61, 610, "loaded", "new live session"), None)
        .unwrap();
    reply(
        &mut h.state_mut().model,
        Data::Window(
            transcript.window(View::Readable, 5, 20),
            "new session".into(),
            1,
        ),
    );
    h.run_steps(20);
    assert!((h.get_by_label("Inspect source 80").rect().top() - before).abs() < 2.0);
    assert!(!h.state().model.following);
    // Reconnection can rebuild group IDs in another order. Locate by sequence.
    h.state_mut().model.tag.connection += 1;
    h.state_mut().model.commands.clear();
    h.run();
    assert!(
        h.state()
            .model
            .commands
            .iter()
            .any(|c| matches!(c.action, Action::Locate(80, _)))
    );
    h.get_by_label("Jump to live (1)").click();
    h.run();
    assert!(h.state().model.following);
    assert_eq!(
        h.state().model.selected.as_ref().unwrap().as_str(),
        "simulation"
    );
}

#[test]
fn checkpoint_controls_select_the_exact_session_and_ignore_stale_inspection() {
    let mut app = Workbench::default();
    app.model.launcher = false;
    app.model.following = false;
    app.model.selected = Some(RunId::new("simulation").unwrap());
    app.model.window = Window {
        total: 1,
        rows: vec![Row {
            title: "Checkpoint saved".into(),
            first_sequence: 30,
            checkpoint: Some("s2".into()),
            trace: Some(TraceContent::Checkpoint(SessionId::new("s2").unwrap())),
            marker: true,
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut h = Harness::builder()
        .with_size([1440.0, 900.0])
        .build_ui_state(|ui, app: &mut Workbench| app.draw(ui), app);
    h.run();
    let stale = h.state().model.tag;
    h.get_by_label("Compare memory").click();
    h.run();
    assert_eq!(h.state().model.inspector, Inspector::Memory);
    assert!(
        h.state()
            .model
            .commands
            .iter()
            .any(|c| matches!(&c.action, Action::Checkpoint(_, s) if s.as_str()=="s2"))
    );
    let contract: serde_json::Value = serde_json::from_str(include_str!(
        "../../playtime-server/tests/fixtures/http-contract.json"
    ))
    .unwrap();
    let c: playtime_protocol::api::Checkpoint =
        serde_json::from_value(contract["checkpoints"]["items"][0].clone()).unwrap();
    let memory = MemoryView {
        incoming: "old".into(),
        outgoing: "old".into(),
        evidence: String::new(),
        diff: String::new(),
        notice: String::new(),
    };
    let run = h.state().model.selected.clone();
    h.state_mut().model.apply(Reply {
        tag: stale,
        run,
        data: Data::Checkpoint(Box::new(c.clone()), memory.clone()),
    });
    assert!(h.state().model.memory.is_none());
    // A list arriving while this explicit selection is loading must not queue
    // the first checkpoint and supersede the requested session.
    h.state_mut().model.commands.clear();
    reply(
        &mut h.state_mut().model,
        Data::Checkpoints(
            playtime_protocol::api::Page {
                items: vec![c],
                next_cursor: None,
                frontier: None,
            },
            Default::default(),
        ),
    );
    assert!(
        !h.state()
            .model
            .commands
            .iter()
            .any(|c| matches!(c.action, Action::Checkpoint(..)))
    );
    h.get_by_label("Browse files").click();
    h.run();
    assert_eq!(h.state().model.inspector, Inspector::Files);
    assert!(!h.state().model.following);
}

#[test]
fn partial_history_never_assigns_an_unseen_session_to_the_previous_one() {
    let mut transcript = Transcript::default();
    transcript.apply(memory(1, 1, "missing", ""), None).unwrap();
    let mut call: EventEnvelope = serde_json::from_str(
        include_str!(
            "../../playtime-protocol/tests/fixtures/controller-observations/completed.jsonl"
        )
        .lines()
        .nth(6)
        .unwrap(),
    )
    .unwrap();
    call.data["session_id"] = json!("s2");
    let seq = call.sequence.get();
    transcript.apply(call, None).unwrap();
    assert!(
        transcript.window(View::Readable, 1, 1).rows[0]
            .session
            .is_none()
    );
    transcript
        .apply(memory(2, seq - 1, "loaded", "reused actual delivery"), None)
        .unwrap();
    assert_eq!(
        transcript.window(View::Readable, 2, 1).rows[0]
            .session
            .as_ref()
            .unwrap()
            .ordinal,
        2
    );
    // A referenced observation has no readable ordinal or delivered text until
    // resolved; never invent a session entry from its ID/configured limit.
    let mut referenced = memory(3, 20, "loaded", "");
    referenced.data["data"] = json!({"storage":"artifact","content":{"path":"observations/memory.json","sha256":"a".repeat(64),"size_bytes":300000}});
    transcript.apply(referenced, None).unwrap();
    assert_eq!(transcript.sessions(0, false).total, 2);
    let window = transcript.window(View::Readable, 3, 1);
    assert!(window.rows[0].status.contains("Retained"));
    assert!(window.rows[0].session.is_none());
    assert!(transcript.raw(20, 0).unwrap().retained.is_some());
}

#[test]
fn a_pending_session_jump_uses_the_latest_history_window() {
    for target_still_visible in [true, false] {
        let mut transcript = Transcript::default();
        for n in 25..=60 {
            transcript
                .apply(memory(n, n as u64 * 10, "loaded", "session memory"), None)
                .unwrap();
        }
        let mut app = Workbench::default();
        app.model.launcher = false;
        app.model.connection = ConnectionState::Connected;
        app.model.selected = Some(RunId::new("simulation").unwrap());
        app.model.following = false;
        app.model.window = transcript.window(View::Readable, 0, 4);
        let mut h = Harness::builder()
            .with_size([900.0, 600.0])
            .build_ui_state(|ui, app: &mut Workbench| app.draw(ui), app);
        h.run();
        h.get_by_role(egui::accesskit::Role::ComboBox).click();
        h.run();
        h.get_by_label("Session 30 · s30").click();
        h.run();
        let target = transcript.sessions(0, false).items[5].clone();
        let mut located = transcript.window(View::Readable, 3, 20);
        located.location = Some((300, target.row));
        reply(
            &mut h.state_mut().model,
            Data::Window(located, "located".into(), 0),
        );
        // Both replies are applied before the UI renders. Earlier sessions move
        // Session 30 from row 5 to row 29; the old absolute position is invalid.
        for n in 1..25 {
            transcript
                .apply(memory(n, n as u64 * 10, "missing", ""), None)
                .unwrap();
        }
        let start = if target_still_visible { 27 } else { 3 };
        reply(
            &mut h.state_mut().model,
            Data::Window(
                transcript.window(View::Readable, start, 20),
                "backfilled".into(),
                0,
            ),
        );
        h.state_mut().model.commands.clear();
        h.run_steps(20);
        if !target_still_visible {
            assert!(
                h.state()
                    .model
                    .commands
                    .iter()
                    .any(|c| matches!(c.action, Action::Locate(300, View::Readable))),
                "a displaced target needs a fresh location, not completion at the stale index"
            );
            let mut located = transcript.window(View::Readable, 27, 20);
            located.location = Some((300, target.row));
            reply(
                &mut h.state_mut().model,
                Data::Window(located, "relocated".into(), 0),
            );
            h.run_steps(20);
        }
        h.get_by_label("Inspect source 300");
        assert!(
            h.query_by_label("Inspect source 290").is_none(),
            "the target session must be the top activity"
        );
        assert!(!h.state().model.following);
    }
}

#[test]
fn a_checkpoint_waits_for_its_own_session_boundary_during_backfill() {
    let mut transcript = Transcript::default();
    transcript.apply(memory(1, 1, "missing", ""), None).unwrap();
    let mut checkpoint: EventEnvelope = serde_json::from_str(
        include_str!("../../playtime-protocol/tests/fixtures/two-session/events.jsonl")
            .lines()
            .nth(12)
            .unwrap(),
    )
    .unwrap();
    checkpoint.run_id = RunId::new("simulation").unwrap();
    assert_eq!(checkpoint.data["checkpoint"]["session_id"], "s2");
    let sequence = checkpoint.sequence.get();
    transcript.apply(checkpoint, None).unwrap();
    let id = transcript.row_for(sequence).unwrap();
    for view in [View::Readable, View::Full] {
        let at = transcript.position(view, id).unwrap();
        let row = &transcript.window(view, at, 1).rows[0];
        assert!(
            row.session.is_none(),
            "an s2 checkpoint must not inherit s1's ordinal"
        );
        assert!(matches!(&row.trace, Some(TraceContent::Checkpoint(s)) if s.as_str() == "s2"));
    }
    transcript
        .apply(memory(2, sequence - 1, "loaded", "delivered to s2"), None)
        .unwrap();
    for view in [View::Readable, View::Full] {
        let at = transcript.position(view, id).unwrap();
        let row = &transcript.window(view, at, 1).rows[0];
        assert_eq!(row.session.as_ref().unwrap().session, "s2");
        assert_eq!(row.session.as_ref().unwrap().ordinal, 2);
    }
}
