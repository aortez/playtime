//! The Results tab totals a run's time, calls, errors, tools, compactions,
//! sessions and final workspace from the records the index keeps, and the run
//! rows say how each run ended.
#[path = "support/continuous_scene.rs"]
#[allow(dead_code)]
mod scene;
use egui_kittest::{Harness, kittest::Queryable};
use playtime_core::{RunId, StopReason};
use playtime_protocol::{api::*, events::EventEnvelope, v1::UsageSummary};
use playtime_ui::{
    backend::prepare_run,
    model::*,
    summary::{Line, Results},
    transcript::{Transcript, View},
    ui::Workbench,
};
use serde_json::json;

fn indexed(events: Vec<EventEnvelope>) -> Transcript {
    let mut transcript = Transcript::default();
    for event in events {
        transcript.apply(event, None).unwrap();
    }
    transcript
}
/// Each section's title and lines, as `(text, jump)`.
type Shown<'a> = Vec<(&'a str, Vec<(&'a str, Option<u64>)>)>;
fn sections(results: &Results) -> Shown<'_> {
    results
        .sections
        .iter()
        .map(|section| {
            (
                section.title,
                section
                    .lines
                    .iter()
                    .map(|Line { text, jump }| (text.as_str(), *jump))
                    .collect(),
            )
        })
        .collect()
}
fn legacy() -> Vec<EventEnvelope> {
    include_str!("../../playtime-protocol/tests/fixtures/controller-observations/completed.jsonl")
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn a_continuous_run_totals_its_time_calls_errors_tools_and_capture() {
    let forward = indexed(scene::results_events()).results(true);
    let mut reversed = scene::results_events();
    reversed.reverse();
    assert_eq!(indexed(reversed).results(true), forward);
    assert!(forward.complete);
    let plain = |text| (text, None);
    assert_eq!(
        sections(&forward),
        [
            (
                "Time",
                vec![
                    plain("0:24 elapsed · 20% in model requests"),
                    plain("Work 0:04 · compaction 0:01 · tools and controller 0:19"),
                ]
            ),
            (
                "Model calls",
                vec![
                    plain("3 work calls · median 1.0 s · longest 2.0 s · 160 tokens · 80 tokens/s"),
                    plain(
                        "1 compaction call · median 1.0 s · longest 1.0 s · 36 tokens · 36 tokens/s"
                    ),
                    plain("1 call with unknown usage · 4,096 tokens charged · 0:02 in those calls"),
                ]
            ),
            ("Model errors", vec![("1 model error: 1 parse", Some(11))]),
            (
                "Tools",
                vec![
                    plain("3 tool calls: 2 exec · 1 write_file"),
                    plain("2 failed"),
                    (
                        "1 × exec: ValueError: Writable paths and working directories must be inside /work",
                        Some(17)
                    ),
                    (
                        "1 × write_file: OSError: [Errno 28] No space left on device",
                        Some(9)
                    ),
                ]
            ),
            (
                "Compactions",
                vec![
                    plain("1 applied of 1 planned · 1 deferred"),
                    ("Applied at 0:22", Some(22)),
                ]
            ),
            ("Final workspace", vec![("12.6 MiB captured", Some(26))]),
        ]
    );
}

#[test]
fn a_memento_run_totals_its_sessions_and_handoffs() {
    let results = indexed(legacy()).results(true);
    let found = sections(&results);
    let section = |title: &str| {
        found
            .iter()
            .find(|(t, _)| *t == title)
            .unwrap_or_else(|| panic!("no {title} in {found:#?}"))
            .1
            .clone()
    };
    // Four short work calls with known usage; under a second of them, so no rate.
    assert_eq!(
        section("Model calls"),
        [(
            "4 work calls · median 50 ms · longest 300 ms · 26 tokens",
            None
        )]
    );
    assert_eq!(
        section("Tools"),
        [(
            "6 tool calls: 2 end_session · 2 exec · 1 read_file · 1 write_file",
            None
        )]
    );
    assert_eq!(
        section("Sessions"),
        [("2 sessions started · 2 checkpoints saved", None)]
    );
    let workspace = section("Final workspace");
    assert!(workspace[0].0.ends_with(" captured") && workspace[0].1.is_some());
    assert!(!found.iter().any(|(title, _)| *title == "Model errors"));
    assert!(!found.iter().any(|(title, _)| *title == "Compactions"));

    // A session whose handoff the model skipped is counted and linked. The
    // controller records it as an observation the reader doesn't type.
    let mut events = legacy();
    let mut missing = events[6].clone();
    assert_eq!(missing.kind, "controller_observation");
    missing.sequence = std::num::NonZeroU64::new(60).unwrap();
    missing.data["controller_sequence"] = json!(60);
    missing.data["session_id"] = json!("s2");
    missing.data["observation_kind"] = json!("handoff_missing");
    missing.data["identity"] =
        json!({"effect_id": null, "request_id": null, "tool_request_sequence": null});
    missing.data["data"] = json!({"storage": "inline", "value": {}});
    events.push(missing);
    let results = indexed(events).results(true);
    let sessions = &results
        .sections
        .iter()
        .find(|s| s.title == "Sessions")
        .unwrap()
        .lines;
    assert_eq!(
        sessions[0],
        Line {
            text: "2 sessions started · 2 checkpoints saved · 1 handoff missing".into(),
            jump: Some(60),
        }
    );
}

#[test]
fn results_wait_for_history_and_bound_their_open_calls() {
    let transcript = indexed(scene::results_events());
    assert_eq!(transcript.results(false), Results::default());

    // Requests that never settle wait as open calls, up to a bound; a live
    // run's open call is in progress, an ended run's never settled.
    let request = |seq: u64| {
        serde_json::from_value::<EventEnvelope>(json!({"schema_version":1,"run_id":"continuous-ui","sequence":seq,"time":"2026-10-25T00:00:00Z","kind":"execution_v2","data":{"schema_version":2,"fact":{"kind":"observation","data":{"controller_sequence":seq,"elapsed_ms":seq*1000,"scope":{"kind":"run"},"observation_kind":"model_request","identity":{"effect_id":seq,"request_id":format!("r{seq}"),"tool_request_sequence":null},"request":{"effect_id":seq,"request_id":format!("r{seq}"),"call":seq,"scope":{"kind":"run"},"purpose":"work"},"data":{"storage":"inline","value":{"purpose":"work","request_id":format!("r{seq}"),"call":seq,"context_input_bound":1150,"context_basis":"input_bytes","context_reserve":4096}}}}}})).unwrap()
    };
    let mut live = Transcript::default();
    live.apply(request(1), None).unwrap();
    let calls = |results: &Results| {
        results
            .sections
            .iter()
            .find(|s| s.title == "Model calls")
            .map(|s| s.lines.clone())
            .unwrap_or_default()
    };
    assert_eq!(
        calls(&live.results(true)),
        [Line {
            text: "1 call in progress".into(),
            jump: Some(1)
        }]
    );
    let mut ended = Transcript::default();
    for seq in 1..=4_097 {
        ended.apply(request(seq), None).unwrap();
    }
    let mut end = scene::results_events().pop().unwrap();
    end.sequence = std::num::NonZeroU64::new(5_000).unwrap();
    ended.apply(end, None).expect("the run's end");
    assert_eq!(
        calls(&ended.results(true)),
        [Line {
            text: "4,097 calls without a recorded settlement".into(),
            jump: Some(2)
        }]
    );
}

/// The contract fixture's completed run, ended at the wall limit as a
/// continuous run.
fn ended_continuous() -> RunView {
    let contract: serde_json::Value = serde_json::from_str(include_str!(
        "../../playtime-server/tests/fixtures/http-contract.json"
    ))
    .unwrap();
    let mut view: RunView = serde_json::from_value(contract["completed_run"].clone()).unwrap();
    view.run_id = RunId::new("continuous-ui").unwrap();
    view.orchestration.state = OrchestrationState::Terminal;
    view.outcome = Some(ExperimentalOutcome {
        reason: StopReason::WallLimit,
        event_sequence: 26,
    });
    view.usage = Some(UsageSummary {
        known_generated_tokens: 196,
        unknown_reserved_tokens: 4096,
        unknown_requests: 1,
        budget_charged_tokens: 4292,
        usage_complete: false,
    });
    view.cleanup = Cleanup::Complete;
    view.harness = Some(ArchivedHarness {
        configuration: view.input_manifest.clone().unwrap(),
        harness: HarnessView::continuous(Default::default()).unwrap(),
    });
    view
}
fn workbench(view: RunView) -> Workbench {
    let mut app = Workbench::default();
    app.model.connection = ConnectionState::Connected;
    app.model.selected = Some(view.run_id.clone());
    app.model.runs = vec![view.clone()];
    app.model.run = Some(prepare_run(view));
    app.model.launcher = false;
    let transcript = indexed(scene::results_events());
    app.model.window = transcript.window(View::Readable, 0, 64);
    app.model.window.progress = transcript.progress(true);
    app.model.window.results = transcript.results(true);
    app.model.window.calls.complete = true;
    app.model.inspector = Inspector::Results;
    app
}

#[test]
fn the_results_tab_opens_first_links_each_count_and_asks_for_the_final_files() {
    // Selecting a run opens its results, whatever tab was showing.
    let mut model = Model::default();
    model.inspector = Inspector::Files;
    model.select_run(RunId::new("continuous-ui").unwrap());
    assert_eq!(model.inspector, Inspector::Results);

    let app = workbench(ended_continuous());
    let run = app.model.selected.clone().unwrap();
    let mut h = Harness::builder()
        .with_size([1440.0, 900.0])
        .build_ui_state(|ui, app: &mut Workbench| app.draw(ui), app);
    h.run();
    // The row says how the run ended.
    h.get_by_label("wall limit · 4,292 tokens · cleanup complete");
    // An ended continuous run's final capture listing is asked for once.
    h.run();
    assert_eq!(
        h.state()
            .model
            .commands
            .iter()
            .filter(|c| matches!(&c.action, Action::FinalFiles(id) if *id == run))
            .count(),
        1
    );
    h.get_by_label("1 model error: 1 parse").click();
    h.run();
    assert!(!h.state().model.following);
    assert!(
        h.state()
            .model
            .commands
            .iter()
            .any(|c| matches!(c.action, Action::Locate(11, View::Readable)))
    );

    // The listing counts the files and lists the images to open.
    let artifact = |name: &str, bytes: u64| {
        serde_json::from_value::<Artifact>(json!({
            "artifact_id": format!("{:0>64}", name.len()),
            "name": name,
            "kind": "file_version",
            "content": {"path": format!("artifacts/final/files/{name}"), "sha256": "d".repeat(64), "size_bytes": bytes},
            "session_id": null,
            "published_sequence": 25,
            "frozen_input": false,
        }))
        .unwrap()
    };
    let listing = FinalListing::from_artifacts(
        &[
            artifact("zoom.gif", 204_800),
            artifact("make.py", 2_048),
            artifact("plot.png", 1_048_576),
        ],
        true,
    );
    assert_eq!(listing.summary, "3 files · 1.2 MiB · 2 images · 1 script");
    assert_eq!(
        listing
            .images
            .iter()
            .map(|image| (image.name.as_str(), image.size.as_str()))
            .collect::<Vec<_>>(),
        [("plot.png", "1 MiB"), ("zoom.gif", "200 KiB")]
    );
    assert!(!listing.incomplete && listing.more_images == 0);
    let model = &mut h.state_mut().model;
    model.final_listing = Some(listing);
    h.run();
    h.get_by_label("3 files · 1.2 MiB · 2 images · 1 script");
    h.get_all_by_label("Open image").next().unwrap().click();
    h.run();
    assert!(h.state().model.commands.iter().any(|c| matches!(
        &c.action,
        Action::Content(id, artifact, 0) if *id == run && artifact.as_str() == format!("{:0>64}", 8)
    )));
    h.get_by_label("Every file in Files").click();
    h.run();
    assert_eq!(h.state().model.inspector, Inspector::Files);
}

#[test]
fn a_memento_runs_listing_comes_from_its_last_checkpoint() {
    let contract: serde_json::Value = serde_json::from_str(include_str!(
        "../../playtime-server/tests/fixtures/http-contract.json"
    ))
    .unwrap();
    let checkpoints: Page<Checkpoint> =
        serde_json::from_value(contract["checkpoints"].clone()).unwrap();
    let last = checkpoints
        .items
        .iter()
        .max_by_key(|c| c.checkpoint.published_sequence)
        .unwrap();
    let expected = FinalListing::from_checkpoint(last);
    let view: RunView = serde_json::from_value(contract["completed_run"].clone()).unwrap();
    let mut app = workbench(view);
    let tag = app.model.tag;
    app.model.apply(Reply {
        tag,
        run: app.model.selected.clone(),
        data: Data::Checkpoints(checkpoints.clone(), Default::default()),
    });
    assert_eq!(app.model.final_listing, Some(expected));
    // While more checkpoints remain, the last in hand isn't the final one.
    app.model.final_listing = None;
    let mut partial = checkpoints;
    partial.next_cursor = Some("more".into());
    let tag = app.model.tag;
    app.model.apply(Reply {
        tag,
        run: app.model.selected.clone(),
        data: Data::Checkpoints(partial, Default::default()),
    });
    assert_eq!(app.model.final_listing, None);
    let mut h = Harness::builder()
        .with_size([1440.0, 900.0])
        .build_ui_state(|ui, app: &mut Workbench| app.draw(ui), app);
    h.run();
    h.get_by_label("Load the remaining checkpoints in Files to see the final workspace.");
    assert!(
        !h.state()
            .model
            .commands
            .iter()
            .any(|c| matches!(c.action, Action::FinalFiles(_)))
    );
}
