//! The run header leads with progress against the frozen limits and says
//! every state in words (#92).
use egui_kittest::{
    Harness,
    kittest::{NodeT, Queryable},
};
use playtime_core::{RunMode, RunPhase, StopReason};
use playtime_protocol::{api::*, events::EventEnvelope, v1::UsageSummary};
use playtime_ui::{
    backend::prepare_run,
    model::*,
    progress::{Limits, Progress, details, lead},
    transcript::{Transcript, View},
    ui::Workbench,
};
use serde_json::json;

fn parse(jsonl: &str) -> Vec<EventEnvelope> {
    jsonl
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}
fn records() -> Vec<EventEnvelope> {
    parse(include_str!(
        "../../playtime-protocol/tests/fixtures/controller-observations/completed.jsonl"
    ))
}
/// Session 2 stops on a model connection error.
fn partial_failure() -> Vec<EventEnvelope> {
    parse(include_str!(
        "../../playtime-protocol/tests/fixtures/controller-observations/partial-failure.jsonl"
    ))
}
/// A copy of a record at another sequence.
fn moved(record: &EventEnvelope, sequence: u64) -> EventEnvelope {
    let mut record = record.clone();
    record.sequence = std::num::NonZeroU64::new(sequence).unwrap();
    record.data["controller_sequence"] = json!(sequence);
    record
}
/// The record with its value stored as an artifact, as a value over 256 KiB is.
fn stored(mut record: EventEnvelope) -> EventEnvelope {
    record.data["data"] = json!({"storage": "artifact", "content": {
        "path": format!("artifacts/observations/{}.json", record.sequence),
        "sha256": "0".repeat(64),
        "size_bytes": 300_000,
    }});
    record
}
/// The records up to `last`, all arriving live.
fn indexed(records: &[EventEnvelope], last: u64) -> Transcript {
    let mut transcript = Transcript::default();
    for event in records.iter().filter(|event| event.sequence.get() <= last) {
        transcript.apply(event.clone(), None).unwrap();
    }
    transcript
}
const FIXTURE: Limits = Limits {
    sessions: 3,
    turns: 2,
    wall_seconds: 300,
    tokens: 1000,
};
const CURIOSITY: Limits = Limits {
    sessions: 10,
    turns: 10,
    wall_seconds: 1800,
    tokens: 300_000,
};

fn view(state: OrchestrationState) -> RunView {
    let contract: serde_json::Value = serde_json::from_str(include_str!(
        "../../playtime-server/tests/fixtures/http-contract.json"
    ))
    .unwrap();
    let mut view: RunView = serde_json::from_value(contract["completed_run"].clone()).unwrap();
    view.orchestration.state = state;
    view
}
fn usage(known: u64, unknown: u64, unknown_requests: u64) -> UsageSummary {
    UsageSummary {
        known_generated_tokens: known,
        unknown_reserved_tokens: unknown,
        unknown_requests,
        budget_charged_tokens: known + unknown,
        usage_complete: unknown_requests == 0,
    }
}
fn coverage(view: &RunView, requests: u64, allowance: u64) -> AccountingCoverage {
    AccountingCoverage {
        schema_version: 1,
        availability: EvidenceAvailability::Available,
        unsettled_requests: requests,
        unsettled_allowance: allowance,
        frontier: view.last_confirmed_frontier.clone().unwrap(),
    }
}
/// The fixture's run in its second session, with an `exec` command running.
fn running() -> RunView {
    let mut view = view(OrchestrationState::Running);
    view.outcome = None;
    view.controller_phase = Some(RunPhase::Working);
    view.usage = Some(usage(21, 0, 0));
    view.accounting_coverage = Some(coverage(&view, 0, 0));
    view
}
fn ended() -> RunView {
    let mut view = view(OrchestrationState::Terminal);
    view.usage = Some(usage(26, 0, 0));
    view.accounting_coverage = Some(coverage(&view, 0, 0));
    view.cleanup = Cleanup::Complete;
    view
}

#[test]
fn progress_follows_the_run_against_its_frozen_limits() {
    let records = records();
    let at = |last, complete| indexed(&records, last).progress(complete);
    assert_eq!(
        at(1, false),
        Progress {
            limits: Some(FIXTURE),
            elapsed_ms: Some(0),
            activity: Some("starting the run".into()),
            ..Default::default()
        }
    );
    let steps = |last, complete| {
        let p = at(last, complete);
        (p.session, p.turn, p.activity)
    };
    let step = |session, turn, activity: &str| (session, turn, Some(activity.to_owned()));
    assert_eq!(steps(7, false), step(Some(1), Some(1), "thinking"));
    assert_eq!(
        steps(16, false),
        step(Some(1), Some(1), "running a command")
    );
    assert_eq!(steps(20, false), step(Some(1), Some(2), "thinking"));
    assert_eq!(steps(23, false), step(Some(1), Some(2), "handing off"));
    assert_eq!(steps(25, false), step(Some(1), Some(2), "working"));
    assert_eq!(
        steps(28, false),
        step(Some(1), Some(2), "saving a checkpoint")
    );
    // Session 2's number is recorded with its memento; once every earlier
    // record is indexed, a session still starting is the next one.
    assert_eq!(steps(29, false), step(None, Some(0), "starting a session"));
    assert_eq!(
        steps(29, true),
        step(Some(2), Some(0), "starting a session")
    );
    assert_eq!(
        steps(31, false),
        step(Some(2), Some(0), "starting a session")
    );
    assert_eq!(
        steps(41, false),
        step(Some(2), Some(1), "running a command")
    );
    assert_eq!(at(41, false).elapsed_ms, Some(350));
    assert_eq!(
        steps(54, false),
        step(Some(2), Some(2), "stopping: agent finished")
    );
    assert_eq!(steps(59, true), step(Some(2), Some(2), "finished"));
    assert_eq!(at(59, true).elapsed_ms, Some(400));
}

#[test]
fn handoff_requests_are_not_work_turns() {
    let records = records();
    // The controller asks for the memento after the session's last turn.
    let mut phase = moved(&records[4], 23);
    phase.data["data"]["value"]["phase"] = json!("handing_off");
    let mut request = moved(&records[18], 24);
    request.data["data"]["value"]["handoff"] = json!(true);
    let mut transcript = indexed(&records, 22);
    for event in [phase, request.clone()] {
        transcript.apply(event, None).unwrap();
    }
    let progress = transcript.progress(false);
    assert_eq!(progress.turn, Some(2));
    assert_eq!(progress.activity.as_deref(), Some("handing off"));

    // After a reconnect, the request says it's the handoff before the
    // handing-off phase loads, and so does the output streamed for it.
    let mut transcript = Transcript::default();
    for event in [request, moved(&records[7], 25)] {
        transcript.apply(event, None).unwrap();
    }
    assert_eq!(
        transcript.progress(false).activity.as_deref(),
        Some("handing off")
    );
}

#[test]
fn requests_stored_as_artifacts_count_like_the_controller() {
    let records = records();
    // A large work request counts as a turn.
    let mut transcript = indexed(&records, 18);
    transcript.apply(stored(records[18].clone()), None).unwrap();
    let progress = transcript.progress(false);
    assert_eq!(
        (progress.turn, progress.activity.as_deref()),
        (Some(2), Some("thinking"))
    );
    // A large request after the handing-off phase is the handoff.
    let mut phase = moved(&records[4], 23);
    phase.data["data"]["value"]["phase"] = json!("handing_off");
    let mut transcript = indexed(&records, 22);
    for event in [phase, stored(moved(&records[18], 24))] {
        transcript.apply(event, None).unwrap();
    }
    let progress = transcript.progress(false);
    assert_eq!(
        (progress.turn, progress.activity.as_deref()),
        (Some(2), Some("handing off"))
    );
}

#[test]
fn a_model_error_says_whether_the_run_retries() {
    let failed = partial_failure();
    assert_eq!(
        indexed(&failed, 38).progress(false).activity.as_deref(),
        Some("stopping after a model error")
    );
    let mut error = failed[37].clone();
    error.data["data"]["value"]["recovery_allowed"] = json!(true);
    let mut transcript = indexed(&failed, 37);
    transcript.apply(error, None).unwrap();
    assert_eq!(
        transcript.progress(false).activity.as_deref(),
        Some("retrying after a model error")
    );
}

#[test]
fn while_history_loads_only_current_values_show() {
    let records = records();
    let mut transcript = Transcript::default();
    // The live stream starts in session 2, after that session began.
    for event in records
        .iter()
        .filter(|e| (31..=41).contains(&e.sequence.get()))
    {
        transcript.apply(event.clone(), None).unwrap();
    }
    assert_eq!(
        transcript.progress(false),
        Progress {
            limits: None,
            session: Some(2),
            completed: None,
            turn: None,
            elapsed_ms: Some(350),
            activity: Some("running a command".into()),
        }
    );
    // Earlier history arrives afterwards, in order.
    for event in records.iter().filter(|e| e.sequence.get() <= 30) {
        transcript.apply(event.clone(), Some(0)).unwrap();
    }
    let loading = transcript.progress(false);
    assert_eq!((loading.limits, loading.turn), (Some(FIXTURE), None));
    let loaded = transcript.progress(true);
    assert_eq!((loaded.session, loaded.turn), (Some(2), Some(1)));
    assert_eq!(loaded.activity.as_deref(), Some("running a command"));

    // Without live records, nothing loaded so far is current: an ended run's
    // first sessions load before its last.
    let mut transcript = Transcript::default();
    for event in records.iter().filter(|e| e.sequence.get() <= 20) {
        transcript.apply(event.clone(), Some(0)).unwrap();
    }
    assert_eq!(
        transcript.progress(false),
        Progress {
            limits: Some(FIXTURE),
            ..Default::default()
        }
    );
}

#[test]
fn a_live_run_leads_with_progress_and_an_ended_run_with_its_outcome() {
    let records = records();
    let progress = indexed(&records, 41).progress(true);
    assert_eq!(
        lead(&running(), &progress),
        [
            "Session 2 of 3 · turn 1 of 2 · running a command",
            "21 of 1,000 tokens charged · 0:00 of 5:00 wall time",
        ]
    );
    assert_eq!(
        details(&running()),
        [
            "Running · simulation · evidence available",
            "Settled: 21 tokens reported · no unknown usage · nothing pending",
        ]
    );
    let progress = indexed(&records, 59).progress(true);
    assert_eq!(
        lead(&ended(), &progress),
        [
            "Ended: agent finished · 2 of 3 sessions completed · 26 of 1,000 tokens charged",
            "Cleanup complete · 0:00 of 5:00 wall time",
        ]
    );

    // A long live run, as #92 describes it.
    let mut view = running();
    view.mode = RunMode::Experiment;
    view.usage = Some(usage(50_000, 57_344, 7));
    view.accounting_coverage = Some(coverage(&view, 1, 8_192));
    let progress = Progress {
        limits: Some(CURIOSITY),
        session: Some(4),
        completed: None,
        turn: Some(3),
        elapsed_ms: Some(750_400),
        activity: Some("running a command".into()),
    };
    assert_eq!(
        lead(&view, &progress),
        [
            "Session 4 of 10 · turn 3 of 10 · running a command",
            "107,344 of 300,000 tokens charged · 12:30 of 30:00 wall time",
        ]
    );
    assert_eq!(
        details(&view)[1],
        "Settled: 50,000 tokens reported · 7 requests with unknown usage, charged 57,344 · 8,192 tokens held for 1 unsettled request"
    );
}

#[test]
fn an_outcome_recorded_before_the_run_ends_still_shows() {
    // A cancelled run whose worker the daemon lost waits in reconciling.
    let mut view = ended();
    view.orchestration.state = OrchestrationState::Reconciling;
    view.orchestration.reason = Some("worker_unavailable".into());
    view.outcome.as_mut().unwrap().reason = StopReason::Interrupted;
    assert_eq!(
        details(&view)[..3],
        [
            "Reconciling · worker unavailable · simulation · evidence available",
            "Outcome: interrupted",
            "Cleanup complete",
        ]
    );
    assert!(
        !details(&ended())
            .iter()
            .any(|line| line.contains("Outcome"))
    );
}

#[test]
fn purpose_accounting_keeps_unknown_usage_and_missing_purposes_explicit() {
    use playtime_core::RequestPurpose;
    use playtime_protocol::archive_queries::{PurposeAccounting, RequestAccounting};
    let purpose = |admitted, settled, not_dispatched, usage| PurposeAccounting {
        admitted_calls: admitted,
        settled_calls: settled,
        not_dispatched_calls: not_dispatched,
        usage,
    };
    let mut view = ended();
    view.request_accounting = Some(RequestAccounting {
        model_calls: Some(4),
        by_purpose: [
            (RequestPurpose::Work, purpose(3, 2, 1, usage(1200, 0, 0))),
            (
                RequestPurpose::Compaction,
                purpose(1, 1, 0, usage(0, 512, 1)),
            ),
        ]
        .into(),
        unclassified: purpose(0, 1, 0, usage(40, 0, 0)),
    });
    let lines = details(&view);
    for expected in [
        "Work: 3 calls admitted · 2 settled · 1 not dispatched · 1,200 tokens reported · 1,200 tokens charged · no unknown usage",
        "Compaction: 1 call admitted · 1 settled · 0 tokens reported · 512 tokens charged · 1 request with unknown usage",
        "Unclassified: 0 calls admitted · 1 settled · 40 tokens reported · 40 tokens charged · no unknown usage",
    ] {
        assert!(
            lines.iter().any(|l| l == expected),
            "{expected}: {lines:#?}"
        );
    }
    view.request_accounting.as_mut().unwrap().unclassified = purpose(0, 0, 0, usage(0, 0, 0));
    assert!(!details(&view).iter().any(|l| l.starts_with("Unclassified")));
}

#[test]
fn an_ended_run_counts_only_the_sessions_it_completed() {
    // The model error interrupts session 2's checkpoint.
    let transcript = indexed(&partial_failure(), 47);
    let mut view = ended();
    view.outcome.as_mut().unwrap().reason = StopReason::ModelConnectionError;
    view.usage = Some(usage(13, 100, 1));
    assert_eq!(
        lead(&view, &transcript.progress(true))[0],
        "Ended: model connection error · 1 of 3 sessions completed · 113 of 1,000 tokens charged"
    );
    // Until earlier history loads, the count isn't known.
    assert_eq!(transcript.progress(false).completed, None);
    assert_eq!(
        lead(&view, &transcript.progress(false))[0],
        "Ended: model connection error · 113 of 1,000 tokens charged"
    );
    assert_eq!(indexed(&records(), 59).progress(true).completed, Some(2));
    // An archive without controller records has no checkpoints to count.
    let archive = parse(include_str!(
        "../../playtime-protocol/tests/fixtures/two-session/events.jsonl"
    ));
    assert_eq!(indexed(&archive, 16).progress(true).completed, None);
}

#[test]
fn a_failed_cleanup_names_its_first_error() {
    let progress = indexed(&records(), 59).progress(true);
    let mut view = ended();
    view.cleanup = Cleanup::Failed {
        errors: vec!["container left behind".into()],
    };
    assert_eq!(
        lead(&view, &progress)[1],
        "Cleanup failed: container left behind · 0:00 of 5:00 wall time"
    );
    view.cleanup = Cleanup::Failed {
        errors: vec![
            "container left behind".into(),
            "network left behind".into(),
            "volume left behind".into(),
        ],
    };
    assert_eq!(
        lead(&view, &progress)[1],
        "Cleanup failed: container left behind and 2 more errors · 0:00 of 5:00 wall time"
    );
    // A run still reconciling reports cleanup under the lead.
    view.orchestration.state = OrchestrationState::Reconciling;
    assert!(
        details(&view)
            .contains(&"Cleanup failed: container left behind and 2 more errors".to_owned())
    );
}

#[test]
fn unavailable_values_are_never_shown_as_zero() {
    let mut view = running();
    view.usage = None;
    view.accounting_coverage = None;
    view.controller_phase = None;
    assert_eq!(
        lead(&view, &Progress::default()),
        ["Running", "Tokens charged: unavailable"]
    );
    assert_eq!(details(&view)[1], "Token accounting unavailable");
    view.usage = Some(usage(0, 100, 1));
    assert_eq!(
        details(&view)[1],
        "Known: 0 tokens reported · 1 request with unknown usage, charged 100 · pending allowance not reported"
    );
    // Before the first records, the API's phase stands in for the activity.
    view.controller_phase = Some(RunPhase::Checkpointing);
    assert_eq!(lead(&view, &Progress::default())[0], "Saving a checkpoint");
}

/// Debug names of more than one word, such as `StopAccepted`.
fn debug_names() -> Vec<String> {
    let mut names = vec![];
    for state in states() {
        names.push(format!("{state:?}"));
    }
    for mode in [
        RunMode::Experiment,
        RunMode::ScriptedSmoke,
        RunMode::LiveSmoke,
        RunMode::Simulation,
    ] {
        names.push(format!("{mode:?}"));
    }
    for phase in phases() {
        names.push(format!("{phase:?}"));
    }
    for reason in reasons() {
        names.push(format!("{reason:?}"));
    }
    for enforcement in enforcements() {
        names.push(format!("{enforcement:?}"));
    }
    names.extend(["OwnershipUnresolved".into(), "Terminal".into()]);
    names.retain(|name| name == "Terminal" || name.chars().skip(1).any(char::is_uppercase));
    names
}
fn states() -> [OrchestrationState; 6] {
    [
        OrchestrationState::Preparing,
        OrchestrationState::Starting,
        OrchestrationState::Running,
        OrchestrationState::Cancelling,
        OrchestrationState::Reconciling,
        OrchestrationState::Terminal,
    ]
}
fn phases() -> [RunPhase; 7] {
    [
        RunPhase::Admitted,
        RunPhase::StartingSession,
        RunPhase::Working,
        RunPhase::HandingOff,
        RunPhase::Checkpointing,
        RunPhase::CleaningUp,
        RunPhase::Terminal,
    ]
}
fn enforcements() -> [Enforcement; 4] {
    [
        Enforcement::Requested,
        Enforcement::StopAccepted,
        Enforcement::Stopped,
        Enforcement::Uncertain,
    ]
}
fn reasons() -> Vec<StopReason> {
    [
        "agent_finished",
        "session_limit",
        "token_limit",
        "wall_limit",
        "log_limit",
        "model_timeout",
        "model_connection_error",
        "model_response_byte_limit",
        "incomplete_model_response",
        "missing_token_usage",
        "tool_parse_retry_limit",
        "tool_parse_error_limit",
        "too_many_tools_in_one_response",
        "token_limit_exceeded_by_server",
        "interrupted",
        "infrastructure_failure",
    ]
    .iter()
    .map(|reason| serde_json::from_value(json!(reason)).unwrap())
    .collect()
}
fn assert_words(lines: &[String]) {
    let names = debug_names();
    for line in lines {
        assert!(
            !line.contains("Some(") && !line.contains("None") && !line.contains('_'),
            "{line}"
        );
        for name in &names {
            assert!(!line.contains(name.as_str()), "{name} in {line}");
        }
    }
}

#[test]
fn every_state_reads_as_words() {
    let records = records();
    let progress = indexed(&records, 59).progress(true);
    let mut views = vec![];
    for state in states() {
        let mut view = running();
        view.orchestration.state = state;
        for reason in [None, Some("worker_unavailable"), Some("operator_released")] {
            view.orchestration.reason = reason.map(Into::into);
            views.push(view.clone());
        }
    }
    for phase in phases() {
        let mut view = running();
        view.controller_phase = Some(phase);
        views.push(view);
    }
    for reason in reasons() {
        let mut view = ended();
        view.outcome.as_mut().unwrap().reason = reason;
        views.push(view);
    }
    for cleanup in [
        Cleanup::Complete,
        Cleanup::Failed {
            errors: vec!["container left behind".into()],
        },
        Cleanup::Failed {
            errors: vec!["container left behind".into(), "network left behind".into()],
        },
        Cleanup::Failed { errors: vec![] },
        Cleanup::Unknown,
    ] {
        for state in [
            OrchestrationState::Reconciling,
            OrchestrationState::Terminal,
        ] {
            let mut view = ended();
            view.orchestration.state = state;
            view.cleanup = cleanup.clone();
            views.push(view);
        }
    }
    for evidence in [
        EvidenceAvailability::Unavailable {
            reason: "awaiting authenticated worker confirmation".into(),
        },
        EvidenceAvailability::OwnershipUnresolved {
            reason: "another daemon holds the archive".into(),
        },
    ] {
        let mut view = running();
        view.evidence = evidence.clone();
        view.accounting_coverage.as_mut().unwrap().availability = evidence;
        views.push(view);
    }
    for enforcement in enforcements() {
        let mut view = running();
        view.cancellation = Some(Cancellation {
            requested_at: view.updated_at.clone(),
            enforcement,
            stop_accepted_at: None,
            grace_deadline: None,
            uncertainty: Some("worker termination unresolved".into()),
        });
        views.push(view);
    }
    for possible in [false, true] {
        let mut view = ended();
        view.contention = Some(Contention {
            timed_requests: 117,
            reloads: 1,
            queue_waits: 2,
            possible,
        });
        views.push(view);
    }
    // The fixture's run is a simulation.
    for mode in [
        RunMode::Experiment,
        RunMode::ScriptedSmoke,
        RunMode::LiveSmoke,
    ] {
        let mut view = running();
        view.mode = mode;
        views.push(view);
    }
    for view in views {
        assert_words(&lead(&view, &progress));
        assert_words(&lead(&view, &Progress::default()));
        assert_words(&prepare_run(view).summary);
    }
}

#[test]
fn the_workbench_shows_the_header_and_no_debug_text() {
    let records = records();
    let transcript = indexed(&records, 41);
    let mut app = Workbench::default();
    app.model.connection = ConnectionState::Connected;
    app.model.launcher = false;
    app.model.following = false;
    let view = running();
    app.model.selected = Some(view.run_id.clone());
    app.model.runs = vec![view.clone()];
    app.model.run = Some(prepare_run(view));
    app.model.window = transcript.window(View::Readable, 0, 64);
    app.model.window.progress = transcript.progress(true);
    let mut h = Harness::builder()
        .with_size([1440.0, 900.0])
        .build_ui_state(|ui, app: &mut Workbench| app.draw(ui), app);
    h.run();
    h.get_by_label("Session 2 of 3 · turn 1 of 2 · running a command");
    h.get_by_label("21 of 1,000 tokens charged · 0:00 of 5:00 wall time");
    h.get_by_label("Running · simulation · evidence available");
    h.get_by_label("Run fixture-run-00000001 · Running");
    let texts = h
        .query_all_by(|_| true)
        .filter_map(|node| {
            let node = node.accesskit_node();
            if node.role() == egui::accesskit::Role::Label {
                node.value()
            } else {
                node.label()
            }
        })
        .collect::<Vec<_>>();
    assert!(texts.len() > 20);
    let names = debug_names();
    for text in texts {
        assert!(!text.contains("Some("), "{text}");
        for name in &names {
            assert!(!text.contains(name.as_str()), "{name} in {text}");
        }
    }
}
