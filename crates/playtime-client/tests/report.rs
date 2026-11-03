//! A run's report, totalled from raw event pages in either archive format,
//! reads the same whatever order its records arrive in, types each total
//! for JSON and words the sections the workbench shows.
use playtime_client::report::{
    Line, Listing, ListingSource, Results, Section, Sessions, Totals, Update,
};
use playtime_protocol::events::EventEnvelope;
use serde_json::{Value, json};

fn v2(seq: u64, fact: Value) -> EventEnvelope {
    serde_json::from_value(json!({"schema_version":1,"run_id":"continuous","sequence":seq,"time":"2026-11-03T00:00:00Z","kind":"execution_v2","data":{"schema_version":2,"fact":fact}})).unwrap()
}
/// A v2 observation at `seq` seconds of controller time, for the call and
/// purpose given.
fn observation(
    seq: u64,
    kind: &str,
    call: Option<(u64, &str)>,
    tool: Option<u64>,
    value: Value,
) -> EventEnvelope {
    let request = call.map(|(call, purpose)| json!({"effect_id":call,"request_id":format!("r{call}"),"call":call,"scope":{"kind":"run"},"purpose":purpose}));
    v2(
        seq,
        json!({"kind":"observation","data":{"controller_sequence":seq,"elapsed_ms":seq*1000,"scope":{"kind":"run"},"observation_kind":kind,"identity":{"effect_id":call.map(|(id,_)|id),"request_id":call.map(|(id,_)|format!("r{id}")),"tool_request_sequence":tool},"request":request,"data":{"storage":"inline","value":value}}}),
    )
}
/// Two work calls, one settled with unknown usage after a parse error, a
/// compaction call around a planned, applied and deferred compaction, a
/// request never dispatched, two tool calls that failed and exited, and the
/// final capture.
fn continuous() -> Vec<EventEnvelope> {
    vec![
        observation(
            1,
            "controller_started",
            None,
            None,
            json!({"parameters":{"limits":{"wall_seconds":600,"generated_tokens":16384,"context_tokens":32768}}}),
        ),
        observation(
            2,
            "compaction_configured",
            None,
            None,
            json!({"settings":{"trigger_percent":50}}),
        ),
        observation(
            3,
            "model_request",
            Some((1, "work")),
            None,
            json!({"purpose":"work","request_id":"r1","call":1}),
        ),
        observation(
            5,
            "request_settled",
            Some((1, "work")),
            None,
            json!({"call":1,"usage":{"status":"known","generated_tokens":100}}),
        ),
        observation(
            6,
            "tool_request",
            Some((1, "work")),
            Some(6),
            json!({"name":"exec","arguments":{"command":"make"}}),
        ),
        observation(
            7,
            "tool_result",
            Some((1, "work")),
            Some(6),
            json!({"name":"exec","result":{"exit_code":1,"stdout":"","stderr":"no rule"}}),
        ),
        observation(
            8,
            "tool_request",
            Some((1, "work")),
            Some(8),
            json!({"name":"write_file","arguments":{"path":"/work/a"}}),
        ),
        observation(
            9,
            "tool_result",
            Some((1, "work")),
            Some(8),
            json!({"name":"write_file","result":{"error":"OSError: no space\nsecond line"}}),
        ),
        observation(
            10,
            "model_request",
            Some((2, "work")),
            None,
            json!({"purpose":"work","request_id":"r2","call":2}),
        ),
        observation(
            11,
            "model_error",
            Some((2, "work")),
            None,
            json!({"error":{"kind":"parse","message":"bad json"},"recovery_allowed":true,"consecutive_errors":2}),
        ),
        observation(
            12,
            "request_settled",
            Some((2, "work")),
            None,
            json!({"call":2,"usage":{"status":"unknown","reserved_tokens":4096}}),
        ),
        observation(
            13,
            "model_request",
            Some((3, "compaction")),
            None,
            json!({"purpose":"compaction","request_id":"r3","call":3}),
        ),
        observation(
            14,
            "compaction_planned",
            None,
            None,
            json!({"ordinal":1,"source_requests":["r1","r2"],"retained_requests":[],"previous_summary":null,"previous_summary_request":null,"before_messages":[],"input_bound":13000,"projected_tokens":14000,"estimate_basis":"input_bytes","summary_input_bound":12000,"summary_estimate_basis":"input_bytes","evidence":{"schema_version":1,"requests":[{"request_id":"r1","purpose":"work","observation":null},{"request_id":"r2","purpose":"work","observation":null}],"plan":null}}),
        ),
        observation(
            15,
            "request_settled",
            Some((3, "compaction")),
            None,
            json!({"call":3,"usage":{"status":"known","generated_tokens":36}}),
        ),
        observation(
            16,
            "compaction_applied",
            Some((3, "compaction")),
            None,
            json!({"ordinal":1,"summary_request_id":"r3","summary":"Two calls so far.","messages":[],"input_bound":4000,"projected_tokens":5000,"estimate_basis":"input_bytes","evidence":{"schema_version":1,"requests":[{"request_id":"r3","purpose":"compaction","observation":null}],"plan":null}}),
        ),
        observation(
            17,
            "compaction_deferred",
            None,
            None,
            json!({"ordinal":2,"remaining_ms":20000,"expected_summary_ms":45000,"projected_tokens":24576,"estimate_basis":"server_usage_plus_new_bytes","summary_input_bound":20480,"summary_estimate_basis":"calibrated_bytes"}),
        ),
        observation(
            18,
            "model_request",
            Some((4, "work")),
            None,
            json!({"purpose":"work","request_id":"r4","call":4}),
        ),
        observation(
            19,
            "request_not_dispatched",
            Some((4, "work")),
            None,
            json!({"call":4,"released_tokens":4096}),
        ),
        v2(
            20,
            json!({"kind":"capture_published","data":{"capture":{"index":{"path":"artifacts/final/capture-index-v2.json","sha256":"b".repeat(64),"size_bytes":14400},"workspace":{"path":"artifacts/final/workspace.tar","sha256":"c".repeat(64),"size_bytes":13209600}}}}),
        ),
    ]
}
fn totals(events: &[EventEnvelope]) -> Totals {
    let mut totals = Totals::default();
    for event in events {
        let update = Update::decode(event).unwrap_or_else(|_| {
            let json = serde_json::to_string(event).unwrap();
            panic!(
                "record {}: {}",
                event.sequence,
                playtime_protocol::events::decode_event(&json)
                    .err()
                    .map_or_else(|| "decodes alone".into(), |e| e.to_string())
            )
        });
        totals.apply(event.sequence.get(), update);
    }
    totals
}
/// Each section's title and lines, as `(text, first sequence)`.
type Shown<'a> = Vec<(&'a str, Vec<(&'a str, Option<u64>)>)>;
fn sections(results: &Results) -> Shown<'_> {
    results
        .sections
        .iter()
        .map(|Section { title, lines }| {
            (
                *title,
                lines
                    .iter()
                    .map(|Line { text, jump }| (text.as_str(), *jump))
                    .collect(),
            )
        })
        .collect()
}

#[test]
fn a_continuous_run_reports_the_same_in_either_order() {
    let events = continuous();
    let forward = totals(&events).report(true, true);
    let mut reversed = events.clone();
    reversed.reverse();
    assert_eq!(totals(&reversed).report(true, true), forward);
    let plain = |text| (text, None);
    assert_eq!(
        sections(&forward.results()),
        [
            (
                "Time",
                vec![
                    plain("0:19 elapsed · 31% in model requests"),
                    plain("Work 0:04 · compaction 0:02 · tools and controller 0:13"),
                ]
            ),
            (
                "Model calls",
                vec![
                    plain("2 work calls · median 2.0 s · longest 2.0 s · 100 tokens · 50 tokens/s"),
                    plain(
                        "1 compaction call · median 2.0 s · longest 2.0 s · 36 tokens · 18 tokens/s"
                    ),
                    plain("1 call with unknown usage · 4,096 tokens charged · 0:02 in those calls"),
                    ("1 call never dispatched", Some(18)),
                ]
            ),
            (
                "Model errors",
                vec![("1 model error: 1 parse · longest parse streak 2", Some(11))]
            ),
            (
                "Tools",
                vec![
                    plain("2 tool calls: 1 exec · 1 write_file"),
                    plain("1 failed · 1 exited non-zero"),
                    ("1 × write_file: OSError: no space", Some(9)),
                ]
            ),
            (
                "Compactions",
                vec![
                    plain("1 applied of 1 planned · 1 deferred"),
                    ("Applied at 0:16", Some(16)),
                ]
            ),
            ("Final workspace", vec![("12.6 MiB captured", Some(20))]),
        ]
    );
    // The typed report carries the same totals, with each count's first
    // record, under stable names.
    let typed = serde_json::to_value(&forward).unwrap();
    assert_eq!(
        typed["time"],
        json!({"elapsed_ms":19000,"model_ms":6000,"work_ms":4000,"handoff_ms":0,"compaction_ms":2000,"retained_ms":0,"other_ms":13000})
    );
    assert_eq!(
        typed["calls"]["work"],
        json!({"calls":2,"median_ms":2000,"median_of":null,"longest_ms":2000,"known_calls":1,"known_tokens":100,"known_ms":2000,"tokens_per_second":50.0,"unknown_calls":1,"reserved_tokens":4096,"unknown_ms":2000})
    );
    assert_eq!(typed["calls"]["handoff"], Value::Null);
    assert_eq!(
        typed["calls"]["never_dispatched"],
        json!({"count":1,"first_sequence":18})
    );
    assert_eq!(typed["calls"]["open"], Value::Null);
    assert_eq!(
        typed["errors"],
        json!({"total":1,"kinds":[{"kind":"parse","count":1,"first_sequence":11}],"other_kinds":0,"longest_parse_streak":2})
    );
    assert_eq!(
        typed["tools"]["failures"],
        json!([{"tool":"write_file","message":"OSError: no space","count":1,"first_sequence":9}])
    );
    assert_eq!(typed["tools"]["exited_non_zero"], 1);
    assert_eq!(
        typed["compactions"],
        json!({"planned":1,"applied":1,"deferred":1,"rejected":0,"applied_at_ms":[16000],"more_applied":0,"first_applied_sequence":16})
    );
    assert_eq!(typed["sessions"], Value::Null);
    assert_eq!(typed["workspace"], json!({"bytes":13209600,"sequence":20}));
    assert_eq!(
        serde_json::to_value(forward.results().sections).unwrap()[1]["lines"][3],
        json!({"text":"1 call never dispatched","first_sequence":18})
    );
    // While the run goes on, an open call is in progress and the workspace
    // the latest; without every earlier record there is no report.
    let mut open = events.clone();
    open.truncate(17);
    let live = totals(&open).report(true, false);
    assert_eq!(
        live.calls.open.map(|open| (open.count, open.first)),
        Some((1, 18))
    );
    let live = live.results();
    assert_eq!(live.sections[1].lines[3].text, "1 call in progress");
    assert!(!live.sections.iter().any(|s| s.title == "Final workspace"));
    assert_eq!(totals(&events).results(false, true), Results::default());
    assert!(!totals(&events).report(false, true).complete);
}

#[test]
fn a_memento_run_reports_its_sessions_and_checkpoint() {
    let events: Vec<EventEnvelope> = include_str!(
        "../../playtime-protocol/tests/fixtures/controller-observations/completed.jsonl"
    )
    .lines()
    .map(|line| serde_json::from_str(line).unwrap())
    .collect();
    let report = totals(&events).report(true, true);
    assert_eq!(
        report.sessions,
        Some(Sessions {
            started: 2,
            completed: 2,
            handoffs_missing: None
        })
    );
    let results = report.results();
    let found = sections(&results);
    let section = |title: &str| found.iter().find(|(t, _)| *t == title).unwrap().1.clone();
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
    assert!(report.errors.is_none() && report.compactions.is_none());
}

#[test]
fn a_listing_counts_files_by_kind_and_joins_the_workspace_section() {
    let mut listing = Listing::new(ListingSource::Checkpoint);
    listing.add("plot.PNG", Some(1000));
    listing.add("make.py", Some(100));
    listing.add("notes.txt", None);
    assert_eq!(
        listing.words(),
        "3 files · 1.1 KiB · 1 image · 1 script · 1 unavailable"
    );
    assert_eq!(
        serde_json::to_value(&listing).unwrap(),
        json!({"source":"checkpoint","files":3,"bytes":1100,"images":1,"scripts":1,"unavailable":1,"incomplete":false})
    );
    // A record captured the workspace: the listing joins its section.
    let mut results = totals(&continuous()).results(true, false);
    results.list_workspace(false, &listing);
    let last = results.sections.last().unwrap();
    assert_eq!(last.title, "Latest workspace");
    assert_eq!(
        last.lines
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>(),
        [
            "12.6 MiB captured",
            "3 files · 1.1 KiB · 1 image · 1 script · 1 unavailable"
        ]
    );
    // No record did: the listing gets its own heading, naming the latest
    // checkpoint while the run goes on.
    let mut results = Results::default();
    results.list_workspace(false, &listing);
    assert_eq!(results.sections[0].title, "Latest checkpoint");
    let mut results = Results::default();
    results.list_workspace(true, &listing);
    assert_eq!(results.sections[0].title, "Final workspace");
    // A final capture without files says so; a bounded listing says it is
    // incomplete.
    let mut empty = Listing::new(ListingSource::FinalCapture);
    let mut results = Results::default();
    results.list_workspace(true, &empty);
    assert_eq!(
        results.sections[0].lines[0].text,
        "No final workspace publication was found."
    );
    empty.incomplete = true;
    let mut results = Results::default();
    results.list_workspace(true, &empty);
    assert_eq!(
        results.sections[0].lines[0].text,
        "Listing limit reached before any final workspace file."
    );
    listing.incomplete = true;
    let mut results = Results::default();
    results.list_workspace(true, &listing);
    assert_eq!(
        results.sections[0].lines[1].text,
        "Listing limit reached; this listing is incomplete."
    );
}

#[test]
fn a_settlement_fact_after_its_observation_adds_nothing_or_the_missing_usage() {
    // The continuous format records a settlement fact a few records after
    // the observation that settles the call: its usage is already counted.
    let request =
        json!({"effect_id":1,"request_id":"r1","call":1,"scope":{"kind":"run"},"purpose":"work"});
    let fact = |seq| {
        v2(
            seq,
            json!({"kind":"request_settled","data":{"request":request,"usage":{"status":"known","generated_tokens":50}}}),
        )
    };
    let events = vec![
        observation(
            1,
            "model_request",
            Some((1, "work")),
            None,
            json!({"purpose":"work","request_id":"r1","call":1}),
        ),
        observation(
            2,
            "request_settled",
            Some((1, "work")),
            None,
            json!({"call":1,"usage":{"status":"known","generated_tokens":50}}),
        ),
        fact(5),
    ];
    let calls = |events: &[EventEnvelope]| totals(events).report(true, true).calls;
    let settled = calls(&events);
    assert_eq!(
        settled
            .work
            .as_ref()
            .map(|w| (w.calls, w.known_calls, w.known_tokens)),
        Some((1, 1, 50))
    );
    assert!(settled.unrequested.is_none() && settled.open.is_none());
    let mut reversed = events.clone();
    reversed.reverse();
    assert_eq!(calls(&reversed), settled);
    // When the observation was omitted, the omission ends the call and the
    // later fact supplies its usage.
    let omitted = vec![
        events[0].clone(),
        v2(
            2,
            json!({"kind":"observation_omitted","data":{"controller_sequence":2,"observation_kind":"request_settled","elapsed_ms":2000,"scope":{"kind":"run"},"identity":{"effect_id":1,"request_id":"r1","tool_request_sequence":null},"request":request,"size_bytes":120,"reason":"controller_log_limit"}}),
        ),
        fact(5),
    ];
    assert_eq!(calls(&omitted), settled);
    let mut reversed = omitted;
    reversed.reverse();
    assert_eq!(calls(&reversed), settled);
}

#[test]
fn counted_parse_failures_remain_errors_and_settle_once_in_reports() {
    // The patched backend supplies usage even though it rejects the call.
    // Error metadata and the later settlement fact must not add another call
    // or another charge. Legacy failures still keep their unknown reservation.
    for purpose in ["work", "compaction"] {
        for tokens in [None, Some(0), Some(476), Some(52)] {
            let usage = match tokens {
                Some(tokens) => json!({"status":"known","generated_tokens":tokens}),
                None => json!({"status":"unknown","reserved_tokens":4096}),
            };
            let mut error = json!({"error":{"kind":"parse","message":"error parsing tool call: invalid JSON"},"consecutive_errors":1,"recovery_allowed":true,"token_budget_charge":tokens.unwrap_or(4096)});
            if tokens.is_some() {
                error["request_usage"] = usage.clone();
            }
            let events = vec![
                observation(
                    1,
                    "model_request",
                    Some((1, purpose)),
                    None,
                    json!({"purpose":purpose,"request_id":"r1","call":1}),
                ),
                observation(
                    2,
                    "request_settled",
                    Some((1, purpose)),
                    None,
                    json!({"call":1,"usage":usage}),
                ),
                observation(3, "model_error", Some((1, purpose)), None, error),
                v2(
                    4,
                    json!({"kind":"request_settled","data":{"request":{"effect_id":1,"request_id":"r1","call":1,"scope":{"kind":"run"},"purpose":purpose},"usage":usage}}),
                ),
            ];
            let report = totals(&events).report(true, true);
            let mut reversed = events.clone();
            reversed.reverse();
            assert_eq!(totals(&reversed).report(true, true), report);
            let typed = serde_json::to_value(&report).unwrap();
            let calls = &typed["calls"][purpose];
            assert_eq!(calls["calls"], 1);
            assert_eq!(calls["known_calls"], u64::from(tokens.is_some()));
            assert_eq!(calls["known_tokens"], tokens.unwrap_or(0));
            assert_eq!(calls["unknown_calls"], u64::from(tokens.is_none()));
            assert_eq!(
                calls["reserved_tokens"],
                if tokens.is_none() { 4096 } else { 0 }
            );
            assert_eq!(typed["calls"]["open"], Value::Null);
            assert_eq!(typed["calls"]["unrequested"], Value::Null);
            assert_eq!(typed["errors"]["total"], 1);
            assert_eq!(typed["errors"]["kinds"][0]["kind"], "parse");
            assert_eq!(typed["errors"]["kinds"][0]["count"], 1);
            assert!(report.results().sections.iter().any(|section| {
                section.title == "Model errors"
                    && section.lines[0].text.contains("1 model error: 1 parse")
            }));
        }
    }
}

#[test]
fn a_late_usage_record_past_the_remembered_calls_is_not_a_call() {
    // 1,025 calls settle, then a usage fact for the first arrives, past the
    // bound of settled calls remembered: it opens nothing that counts as a
    // call, and nothing is reported dropped.
    let mut events = vec![];
    for call in 1..=1025u64 {
        events.push(observation(
            2 * call - 1,
            "model_request",
            Some((call, "work")),
            None,
            json!({"purpose":"work","request_id":format!("r{call}"),"call":call}),
        ));
        events.push(observation(
            2 * call,
            "request_settled",
            Some((call, "work")),
            None,
            json!({"call":call,"usage":{"status":"known","generated_tokens":1}}),
        ));
    }
    events.push(v2(
        3000,
        json!({"kind":"request_settled","data":{"request":{"effect_id":1,"request_id":"r1","call":1,"scope":{"kind":"run"},"purpose":"work"},"usage":{"status":"known","generated_tokens":1}}}),
    ));
    let calls = totals(&events).report(true, true).calls;
    assert_eq!(
        calls
            .work
            .as_ref()
            .map(|w| (w.calls, w.known_calls, w.known_tokens)),
        Some((1025, 1025, 1025))
    );
    assert!(calls.unrequested.is_none() && calls.open.is_none());
    assert_eq!(calls.dropped, 0);
}

#[test]
fn the_runs_end_and_latest_session_are_remembered() {
    // A run's end is its controller's finish or the run_finished record;
    // without one the totals don't know the run ended.
    let events: Vec<EventEnvelope> = include_str!(
        "../../playtime-protocol/tests/fixtures/controller-observations/completed.jsonl"
    )
    .lines()
    .map(|line| serde_json::from_str(line).unwrap())
    .collect();
    assert!(totals(&events).finished());
    let unfinished: Vec<EventEnvelope> = events
        .iter()
        .filter(|event| event.data["observation_kind"] != "controller_finished")
        .cloned()
        .collect();
    assert!(unfinished.len() < events.len() && !totals(&unfinished).finished());
    assert!(!totals(&continuous()).finished());
    // A record naming a later session whose ordinal hasn't arrived counts
    // that session as started, as the run header does; a record of an
    // earlier session changes nothing.
    let mut later = events.clone();
    let mut next = events[6].clone();
    assert_eq!(next.kind, "controller_observation");
    next.sequence = std::num::NonZeroU64::new(60).unwrap();
    next.data["controller_sequence"] = json!(60);
    next.data["session_id"] = json!("s3");
    next.data["observation_kind"] = json!("future_note");
    next.data["identity"] =
        json!({"effect_id": null, "request_id": null, "tool_request_sequence": null});
    next.data["data"] = json!({"storage": "inline", "value": {}});
    later.push(next);
    let started = |events: &[EventEnvelope]| {
        totals(events)
            .report(true, true)
            .sessions
            .map(|s| (s.started, s.completed))
    };
    assert_eq!(started(&later), Some((3, 2)));
    later.reverse();
    assert_eq!(started(&later), Some((3, 2)));
    let mut earlier = events.clone();
    let mut first = events[6].clone();
    first.sequence = std::num::NonZeroU64::new(2).unwrap();
    first.data["controller_sequence"] = json!(2);
    first.data["session_id"] = json!("s0");
    first.data["observation_kind"] = json!("future_note");
    first.data["identity"] =
        json!({"effect_id": null, "request_id": null, "tool_request_sequence": null});
    first.data["data"] = json!({"storage": "inline", "value": {}});
    earlier.push(first);
    assert_eq!(started(&earlier), Some((2, 2)));
}

#[test]
fn a_malformed_record_is_an_invalid_response() {
    let bad: EventEnvelope = serde_json::from_value(json!({"schema_version":1,"run_id":"r","sequence":1,"time":"2026-11-03T00:00:00Z","kind":"run_finished","data":{"reason":"agent_finished"}})).unwrap();
    assert!(Update::decode(&bad).is_err());
    let unknown: EventEnvelope = serde_json::from_value(json!({"schema_version":1,"run_id":"r","sequence":1,"time":"2026-11-03T00:00:00Z","kind":"future_kind","data":{"anything":true}})).unwrap();
    assert_eq!(Update::decode(&unknown).unwrap().tracked_bytes(), 0);
}
