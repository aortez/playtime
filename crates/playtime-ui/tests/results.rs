//! The Results tab totals a run's time, calls, errors, tools, compactions,
//! sessions and final workspace from the records the index keeps, and the run
//! rows say how each run ended.
#[path = "support/pictures.rs"]
mod pictures;
#[path = "support/continuous_scene.rs"]
#[allow(dead_code)]
mod scene;
use egui_kittest::{Harness, kittest::Queryable};
use pictures::{artifact, artifact_id, picture};
use playtime_client::{
    config::Options,
    report::{Line, Results},
};
use playtime_core::{RunId, StopReason};
use playtime_protocol::{api::*, events::EventEnvelope, v1::UsageSummary};
use playtime_ui::{
    backend::prepare_run,
    model::*,
    transcript::{Transcript, View},
    ui::Workbench,
};
use serde_json::json;
use std::sync::Arc;

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
        [("2 sessions started · 2 completed", None)]
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
            text: "2 sessions started · 2 completed · 1 handoff missing".into(),
            jump: Some(60),
        }
    );
}

/// The Model calls lines' text.
fn call_lines(results: &Results) -> Vec<String> {
    results
        .sections
        .iter()
        .find(|s| s.title == "Model calls")
        .map(|s| s.lines.iter().map(|line| line.text.clone()).collect())
        .unwrap_or_default()
}

#[test]
fn a_memento_handoff_or_retained_request_counts_on_its_own_line() {
    // The fixture's second request asks for the handoff.
    let mut events = legacy();
    assert_eq!(events[18].data["observation_kind"], "model_request");
    events[18].data["data"]["value"]["handoff"] = json!(true);
    assert_eq!(
        call_lines(&indexed(events).results(true)),
        [
            "3 work calls · median 50 ms · longest 300 ms · 21 tokens",
            "1 handoff call · median 0 ms · longest 0 ms · 5 tokens",
        ]
    );
    // With its payload retained elsewhere, the request's handoff flag isn't
    // in the index, so the call is neither guessed as work nor as handoff.
    let mut events = legacy();
    events[18].data["data"] = json!({
        "storage": "artifact",
        "content": {"path": "observations/r2.json", "sha256": "e".repeat(64), "size_bytes": 300_000},
    });
    assert_eq!(
        call_lines(&indexed(events).results(true)),
        [
            "3 work calls · median 50 ms · longest 300 ms · 21 tokens",
            "1 call with a retained request · median 0 ms · longest 0 ms · 5 tokens",
        ]
    );
}

/// A v2 observation of `kind` for work call `call`, at `seq` seconds.
fn v2(seq: u64, kind: &str, call: u64, value: serde_json::Value) -> EventEnvelope {
    let request = json!({"effect_id":call,"request_id":format!("r{call}"),"call":call,"scope":{"kind":"run"},"purpose":"work"});
    serde_json::from_value(json!({"schema_version":1,"run_id":"continuous-ui","sequence":seq,"time":"2026-10-25T00:00:00Z","kind":"execution_v2","data":{"schema_version":2,"fact":{"kind":"observation","data":{"controller_sequence":seq,"elapsed_ms":seq*1000,"scope":{"kind":"run"},"observation_kind":kind,"identity":{"effect_id":call,"request_id":format!("r{call}"),"tool_request_sequence":null},"request":request,"data":{"storage":"inline","value":value}}}}})).unwrap()
}
/// A v2 fact that isn't an observation.
fn v2_fact(seq: u64, fact: serde_json::Value) -> EventEnvelope {
    serde_json::from_value(json!({"schema_version":1,"run_id":"continuous-ui","sequence":seq,"time":"2026-10-25T00:00:00Z","kind":"execution_v2","data":{"schema_version":2,"fact":fact}})).unwrap()
}

#[test]
fn calls_never_dispatched_and_omitted_settlements_still_count() {
    // A request the controller never dispatched ends without a settlement
    // and counts as such, not as a call in progress.
    let mut events = scene::results_events();
    events.push(v2(
        27,
        "model_request",
        5,
        json!({"purpose":"work","request_id":"r5","call":5,"context_input_bound":1500,"context_basis":"input_bytes","context_reserve":4096}),
    ));
    events.push(v2(
        28,
        "request_not_dispatched",
        5,
        json!({"call":5,"released_tokens":4096}),
    ));
    let forward = indexed(events.clone()).results(true);
    events.reverse();
    assert_eq!(indexed(events).results(true), forward);
    let lines = &forward
        .sections
        .iter()
        .find(|s| s.title == "Model calls")
        .unwrap()
        .lines;
    assert_eq!(lines.len(), 4);
    assert_eq!(
        lines[3],
        Line {
            text: "1 call never dispatched".into(),
            jump: Some(27),
        }
    );

    // A settlement whose observation the controller log limit omitted still
    // has its time in the omission and its usage in the settlement fact.
    let request =
        json!({"effect_id":1,"request_id":"r1","call":1,"scope":{"kind":"run"},"purpose":"work"});
    let events = vec![
        v2(
            1,
            "model_request",
            1,
            json!({"purpose":"work","request_id":"r1","call":1,"context_input_bound":1150,"context_basis":"input_bytes","context_reserve":4096}),
        ),
        v2_fact(
            2,
            json!({"kind":"request_settled","data":{"request":request,"usage":{"status":"known","generated_tokens":50}}}),
        ),
        v2_fact(
            3,
            json!({"kind":"observation_omitted","data":{"controller_sequence":3,"observation_kind":"request_settled","elapsed_ms":3000,"scope":{"kind":"run"},"identity":{"effect_id":1,"request_id":"r1","tool_request_sequence":null},"request":request,"size_bytes":120,"reason":"controller_log_limit"}}),
        ),
    ];
    let expected = ["1 work call · median 2.0 s · longest 2.0 s · 50 tokens · 25 tokens/s"];
    assert_eq!(call_lines(&indexed(events.clone()).results(true)), expected);
    let mut reversed = events.clone();
    reversed.reverse();
    assert_eq!(call_lines(&indexed(reversed).results(true)), expected);

    // The archive records the settlement fact after the observation that
    // settles the call, and before an omission of it; whichever side of the
    // end it lands on, the fact adds the usage the call still lacks, never
    // a second call.
    let mut late = vec![events[0].clone(), events[2].clone(), events[1].clone()];
    late[1].sequence = std::num::NonZeroU64::new(2).unwrap();
    late[2].sequence = std::num::NonZeroU64::new(3).unwrap();
    assert_eq!(call_lines(&indexed(late.clone()).results(true)), expected);
    late.reverse();
    assert_eq!(call_lines(&indexed(late).results(true)), expected);
    let settled = vec![
        events[0].clone(),
        v2(
            2,
            "request_settled",
            1,
            json!({"call":1,"usage":{"status":"known","generated_tokens":50}}),
        ),
        {
            let mut fact = events[1].clone();
            fact.sequence = std::num::NonZeroU64::new(3).unwrap();
            fact
        },
    ];
    let expected = ["1 work call · median 1.0 s · longest 1.0 s · 50 tokens · 50 tokens/s"];
    assert_eq!(
        call_lines(&indexed(settled.clone()).results(true)),
        expected
    );
    let mut reversed = settled;
    reversed.reverse();
    assert_eq!(call_lines(&indexed(reversed).results(true)), expected);
}

#[test]
fn model_errors_count_their_kinds_up_to_a_bound() {
    // Sixteen kinds are named; the rest count as other kinds.
    let events: Vec<EventEnvelope> = (1..=17)
        .map(|seq| {
            v2(
                seq,
                "model_error",
                1,
                json!({"error":{"kind":format!("e{seq:02}"),"message":"scripted"},"recovery_allowed":true,"token_budget_charge":4096,"consecutive_errors":1,"tool_parse_errors":0,
                    "usage":{"budget_charged_tokens":4096,"known_generated_tokens":0,"unknown_requests":1,"unknown_reserved_tokens":4096,"usage_complete":false}}),
            )
        })
        .collect();
    let results = indexed(events).results(true);
    let errors = &results
        .sections
        .iter()
        .find(|s| s.title == "Model errors")
        .unwrap()
        .lines;
    assert_eq!(
        errors[..],
        [Line {
            text: format!(
                "17 model errors: {} · 1 of other kinds",
                (1..=16)
                    .map(|i| format!("1 e{i:02}"))
                    .collect::<Vec<_>>()
                    .join(" · ")
            ),
            jump: Some(1),
        }]
    );
    // A memento error's kind is an open vocabulary, kept to an excerpt, and
    // the controller's consecutive count is its parse streak.
    let mut error: EventEnvelope = serde_json::from_str(
        include_str!(
            "../../playtime-protocol/tests/fixtures/controller-observations/partial-failure.jsonl"
        )
        .lines()
        .find(|line| line.contains(r#""observation_kind":"model_error""#))
        .unwrap(),
    )
    .unwrap();
    error.data["data"]["value"]["error"]["kind"] = json!("x".repeat(200));
    error.data["data"]["value"]["consecutive_errors"] = json!(3);
    let results = indexed(vec![error]).results(true);
    let errors = &results
        .sections
        .iter()
        .find(|s| s.title == "Model errors")
        .unwrap()
        .lines;
    assert_eq!(
        errors[0].text,
        format!(
            "1 model error: 1 {} … · longest parse streak 3",
            "x".repeat(40)
        )
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
        [
            Line {
                text: "4,096 calls without a recorded settlement".into(),
                jump: Some(2),
            },
            Line {
                text: "1 call dropped at the bound of 4,096 open calls".into(),
                jump: None,
            },
        ]
    );
    // The dropped call's settlement arrives on its own: a settlement without
    // its request, never a call counted twice.
    let settlement = serde_json::from_value::<EventEnvelope>(json!({"schema_version":1,"run_id":"continuous-ui","sequence":5_001,"time":"2026-10-25T00:00:00Z","kind":"execution_v2","data":{"schema_version":2,"fact":{"kind":"observation","data":{"controller_sequence":5001,"elapsed_ms":5001000,"scope":{"kind":"run"},"observation_kind":"request_settled","identity":{"effect_id":1,"request_id":"r1","tool_request_sequence":null},"request":{"effect_id":1,"request_id":"r1","call":1,"scope":{"kind":"run"},"purpose":"work"},"data":{"storage":"inline","value":{"call":1,"usage":{"status":"known","generated_tokens":1},"totals":{"budget_charged_tokens":1,"known_generated_tokens":1,"unknown_requests":0,"unknown_reserved_tokens":0,"usage_complete":true}}}}}}})).unwrap();
    ended.apply(settlement, None).unwrap();
    assert_eq!(
        calls(&ended.results(true)),
        [
            Line {
                text: "4,095 calls without a recorded settlement".into(),
                jump: Some(3),
            },
            Line {
                text: "1 settled call without a recorded request".into(),
                jump: Some(5_001),
            },
            Line {
                text: "2 calls dropped at the bound of 4,096 open calls".into(),
                jump: None,
            },
        ]
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
    // Tall enough for the whole tab, thumbnails included, to be clickable
    // without scrolling.
    let mut h = Harness::builder()
        .with_size([1440.0, 1400.0])
        .build_ui_state(|ui, app: &mut Workbench| app.draw(ui), app);
    h.run();
    // The row says how the run ended.
    h.get_by_label("wall limit · 4,292 tokens · cleanup complete");
    // An ended continuous run's final capture listing is asked for once.
    h.run();
    let asked: Vec<_> = h
        .state()
        .model
        .commands
        .iter()
        .filter(|c| matches!(&c.action, Action::FinalFiles(id) if *id == run))
        .collect();
    assert_eq!(asked.len(), 1);
    let tag = asked[0].tag;
    // Its reply applies after a later inspection too, and an empty listing
    // is no final workspace, not an empty one.
    let model = &mut h.state_mut().model;
    model.inspect(0, 3);
    model.inspector = Inspector::Results;
    model.apply(Reply {
        tag,
        run: Some(run.clone()),
        data: Data::FinalFiles(vec![], true),
    });
    assert_eq!(model.final_listing, None);
    h.run();
    h.get_by_label("No final workspace publication was found.");
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
    model.apply(Reply {
        tag,
        run: Some(run.clone()),
        data: Data::FinalFiles(
            vec![
                artifact("zoom.gif", 204_800),
                artifact("make.py", 2_048),
                artifact("plot.png", 1_048_576),
            ],
            true,
        ),
    });
    assert_eq!(model.final_listing, Some(listing));
    // The listed images' thumbnails are asked for once, in listing order.
    let sought: Vec<_> = model
        .commands
        .iter()
        .filter_map(|c| match &c.action {
            Action::Thumbnails(id, ids) if *id == run => Some(ids.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        sought,
        vec![vec![artifact_id("plot.png"), artifact_id("zoom.gif")]]
    );
    // They arrive one by one; a reply for another run or for an image that
    // wasn't asked for is ignored, and an image without one says why.
    let pixels = playtime_ui::media::thumbnail(&picture(640, 480, 0)).unwrap();
    assert_eq!(pixels.image.size, [224, 168]);
    for (run, image, thumbnail) in [
        (run.clone(), "plot.png", Ok(Arc::new(pixels))),
        (
            run.clone(),
            "zoom.gif",
            Err("GIF exceeds encoded preview limit".to_owned()),
        ),
        (
            RunId::new("other").unwrap(),
            "make.py",
            Err("other run".to_owned()),
        ),
        (run.clone(), "make.py", Err("not listed".to_owned())),
    ] {
        model.apply(Reply {
            tag,
            run: Some(run),
            data: Data::Thumbnail(artifact_id(image), thumbnail),
        });
    }
    assert_eq!(model.thumbnails.len(), 2);
    assert_eq!(
        model
            .thumbnails
            .get(&artifact_id("zoom.gif"))
            .and_then(|thumbnail| thumbnail.as_ref().err())
            .map(String::as_str),
        Some("GIF exceeds encoded preview limit")
    );
    h.run();
    h.get_by_label("3 files · 1.2 MiB · 2 images · 1 script");
    // Each thumbnail is a button to open its image in place, named after it,
    // with or without a preview; the name is written under it.
    h.get_by_label("Open image zoom.gif");
    h.get_by_label("plot.png");
    h.get_by_label("Open image plot.png").click();
    h.run();
    assert!(h.state().model.commands.iter().any(|c| matches!(
        &c.action,
        Action::Content(id, artifact, 0) if *id == run && *artifact == artifact_id("plot.png")
    )));
    h.get_by_label("Every file in Files").click();
    h.run();
    assert_eq!(h.state().model.inspector, Inspector::Files);
    // Back on the Results tab, which asks for the thumbnails left pending.
    h.state_mut().model.inspector = Inspector::Results;
    // A later listing of the same content under new artifacts, as a later
    // checkpoint publishes it, keeps the thumbnails it has and drops the
    // rest; only the images without one are asked for, in one request.
    let asks = |model: &Model| -> Vec<Vec<ArtifactId>> {
        model
            .commands
            .iter()
            .filter_map(|c| match &c.action {
                Action::Thumbnails(id, ids) if *id == run => Some(ids.clone()),
                _ => None,
            })
            .collect()
    };
    let later = |name: &str, bytes: u64, publication: u8| {
        let mut later = artifact(name, bytes);
        later.artifact_id = artifact_id(&format!("{name}@{publication}"));
        later
    };
    let model = &mut h.state_mut().model;
    let plot = model.thumbnails[&artifact_id("plot.png")].clone().unwrap();
    model.apply(Reply {
        tag,
        run: Some(run.clone()),
        data: Data::FinalFiles(
            vec![
                later("plot.png", 1_048_576, 2),
                later("make.py", 2_048, 2),
                later("new.png", 4_096, 2),
            ],
            true,
        ),
    });
    assert_eq!(
        model.thumbnails.keys().cloned().collect::<Vec<_>>(),
        [artifact_id("plot.png@2")]
    );
    assert!(matches!(
        &model.thumbnails[&artifact_id("plot.png@2")],
        Ok(kept) if Arc::ptr_eq(kept, &plot)
    ));
    assert_eq!(asks(model).last(), Some(&vec![artifact_id("new.png@2")]));
    // A reconnect drops the reply still in flight, so the tab asks for that
    // thumbnail again once the service is back, and only for it.
    model.connect(Options::default());
    h.run();
    assert_eq!(asks(&h.state().model).len(), 2);
    h.state_mut().model.connection = ConnectionState::Connected;
    h.run();
    assert_eq!(
        asks(&h.state().model)[1..],
        [
            vec![artifact_id("new.png@2")],
            vec![artifact_id("new.png@2")]
        ]
    );
    // A full queue defers the ask to a later frame rather than dropping it.
    let model = &mut h.state_mut().model;
    while model.commands.len() < 64 {
        model.command(Action::Targets);
    }
    model.apply(Reply {
        tag: model.tag,
        run: Some(run.clone()),
        data: Data::FinalFiles(
            vec![later("plot.png", 1_048_576, 3), later("sky.png", 4_096, 3)],
            true,
        ),
    });
    assert_eq!(asks(model).len(), 3);
    model.commands.clear();
    h.run();
    assert_eq!(asks(&h.state().model), [vec![artifact_id("sky.png@3")]]);
    // Another run's selection drops them.
    h.state_mut()
        .model
        .select_run(RunId::new("another").unwrap());
    assert!(h.state().model.thumbnails.is_empty());
}

#[test]
fn thumbnail_rows_fit_the_pane() {
    // A row holds as many cells as the pane is wide, counting the button's
    // padding around each thumbnail: at 380 pixels three cells would run
    // past the pane's edge, so they go two to a row.
    let mut app = workbench(ended_continuous());
    app.model.preferences.right_width = 380.0;
    let run = app.model.selected.clone().unwrap();
    let names = ["a.png", "b.png", "c.png", "d.png", "e.png"];
    app.model.apply(Reply {
        tag: app.model.tag,
        run: Some(run),
        data: Data::FinalFiles(
            names.iter().map(|name| artifact(name, 4_096)).collect(),
            true,
        ),
    });
    let pixels = Arc::new(playtime_ui::media::thumbnail(&picture(640, 480, 0)).unwrap());
    for name in names {
        app.model
            .thumbnails
            .insert(artifact_id(name), Ok(pixels.clone()));
    }
    let mut h = Harness::builder()
        .with_size([1440.0, 1400.0])
        .build_ui_state(|ui, app: &mut Workbench| app.draw(ui), app);
    h.run();
    let cells: Vec<_> = names
        .iter()
        .map(|name| h.get_by_label(&format!("Open image {name}")).rect())
        .collect();
    for cell in &cells {
        assert!(cell.min.x >= 1060.0 && cell.max.x <= 1440.0, "{cell:?}");
    }
    assert_eq!(cells[0].min.y, cells[1].min.y);
    assert!(cells[2].min.y > cells[0].max.y);
    assert!((cells[2].min.x - cells[0].min.x).abs() < 0.5);
}

#[test]
fn a_running_run_shows_its_latest_workspace_as_such() {
    // Before the run's end, the workspace captured is the latest, not final.
    let mut events = scene::results_events();
    events.truncate(25);
    let results = indexed(events).results(true);
    assert_eq!(
        sections(&results).last(),
        Some(&("Latest workspace", vec![("12.6 MiB captured", Some(25))]))
    );
    // A memento run still going lists its latest checkpoint as such.
    let contract: serde_json::Value = serde_json::from_str(include_str!(
        "../../playtime-server/tests/fixtures/http-contract.json"
    ))
    .unwrap();
    let checkpoints: Page<Checkpoint> =
        serde_json::from_value(contract["checkpoints"].clone()).unwrap();
    let mut view: RunView = serde_json::from_value(contract["completed_run"].clone()).unwrap();
    view.orchestration.state = OrchestrationState::Running;
    view.outcome = None;
    let mut app = workbench(view);
    app.model.window.results = Results {
        complete: true,
        sections: vec![],
    };
    let tag = app.model.tag;
    app.model.apply(Reply {
        tag,
        run: app.model.selected.clone(),
        data: Data::Checkpoints(checkpoints, Default::default()),
    });
    assert!(app.model.final_listing.is_some());
    let mut h = Harness::builder()
        .with_size([1440.0, 900.0])
        .build_ui_state(|ui, app: &mut Workbench| app.draw(ui), app);
    h.run();
    h.get_by_label("Latest checkpoint");
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
