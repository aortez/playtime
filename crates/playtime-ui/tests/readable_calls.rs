//! Readable call associations use archived identities, independently of delivery order.
use playtime_protocol::events::EventEnvelope;
use playtime_ui::transcript::{Transcript, View};
use serde_json::{Value, json};

fn records() -> Vec<EventEnvelope> {
    include_str!("../../playtime-protocol/tests/fixtures/controller-observations/completed.jsonl")
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}
fn observation(seq: u64, kind: &str, tool: Option<u64>, value: Value) -> EventEnvelope {
    serde_json::from_value(json!({
        "schema_version":1, "run_id":"readable-calls", "sequence":seq,
        "time":"2026-01-01T00:00:00Z", "kind":"controller_observation",
        "data":{"schema_version":1,"controller_sequence":seq,"elapsed_ms":seq,
            "session_id":"s1","observation_kind":kind,
            "identity":{"effect_id":1,"request_id":"r1","tool_request_sequence":tool},
            "data":{"storage":"inline","value":value}}
    }))
    .unwrap()
}
fn request() -> EventEnvelope {
    let mut event = records()
        .into_iter()
        .find(|e| e.data["observation_kind"] == "model_request")
        .unwrap();
    event.run_id = playtime_core::RunId::new("readable-calls").unwrap();
    event.sequence = 1.try_into().unwrap();
    event.data["controller_sequence"] = json!(1);
    event.data["data"]["value"]["handoff"] = json!(true);
    event
}
#[test]
fn readable_call_and_tool_blocks_are_identical_after_reverse_history_and_redelivery() {
    let records = records();
    let build = |reverse: bool| {
        let mut t = Transcript::default();
        let mut events = records.clone();
        if reverse {
            events.reverse();
        }
        for e in events {
            t.apply(e, None).unwrap();
        }
        for e in &records {
            assert!(!t.apply(e.clone(), None).unwrap());
        }
        let w = t.window(View::Readable, 0, 64);
        for row in &w.rows {
            assert!(t.raw(row.first_sequence, 0).is_some());
            assert!(t.sources(row.id).contains(&row.first_sequence));
        }
        w.rows
            .into_iter()
            .map(|r| {
                (
                    r.first_sequence,
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
    let call = forward.iter().position(|r| r.1 == "Call 1").unwrap();
    assert_eq!(forward[call + 1].1, "Write · /work/persistent.txt");
    assert_eq!(forward[call + 2].1, "Run command");
    assert_eq!(forward[call + 1].3.as_deref(), Some("Call 1"));
    assert!(
        forward[call]
            .4
            .iter()
            .all(|(label, _)| label != "Recorded model request" && label != "Tool calls")
    );
    assert!(
        forward[call + 2]
            .4
            .iter()
            .any(|(label, value)| label == "Command" && value == "fixture:start transient")
    );
    assert!(
        forward[call + 2]
            .4
            .iter()
            .any(|(label, value)| label == "Standard output" && value == "started")
    );
}
#[test]
fn result_status_is_evidence_based_and_missing_parent_does_not_hide_tools() {
    let mut t = Transcript::default();
    t.apply(
        observation(
            3,
            "tool_request",
            Some(3),
            json!({"name":"exec","arguments":{"command":"false"}}),
        ),
        None,
    )
    .unwrap();
    let row = &t.window(View::Readable, 0, 64).rows[0];
    assert!(row.nested);
    assert_eq!(row.parent.as_deref(), Some("Call details unavailable"));
    assert_eq!(row.status, "Awaiting recorded result");
    t.apply(request(), None).unwrap();
    assert_eq!(
        t.window(View::Readable, 0, 64).rows[0].title,
        "Call 1 · Handoff request"
    );
    assert_eq!(
        t.window(View::Readable, 0, 64).rows[1].parent.as_deref(),
        Some("Call 1 · Handoff request")
    );
    for (result, expected) in [
        (
            json!({"exit_code":1,"stderr":"failed check"}),
            "Failed · exit status: 1",
        ),
        (json!({"exit_code":0,"timed_out":true}), "Timed out"),
        (json!({"exit_code":0}), "Exit status: 0"),
        (
            json!({}),
            "Exit status unavailable · inspect recorded result",
        ),
        (json!({"error":"rejected"}), "Failed: rejected"),
    ] {
        let mut t = Transcript::default();
        t.apply(
            observation(
                3,
                "tool_request",
                Some(3),
                json!({"name":"exec","arguments":{"command":"false"}}),
            ),
            None,
        )
        .unwrap();
        t.apply(observation(4, "tool_result", Some(3), json!({"name":"exec","request_sequence":3,"result":result,"unabridged_result":null})), None).unwrap();
        assert_eq!(t.window(View::Readable, 0, 64).rows[0].status, expected);
    }
    t.apply(
        observation(
            4,
            "tool_not_dispatched",
            Some(3),
            json!({"request_sequence":3}),
        ),
        None,
    )
    .unwrap();
    assert_eq!(
        t.window(View::Readable, 0, 64).rows[1].status,
        "Not dispatched"
    );
}
#[test]
fn ended_run_does_not_promote_unanswered_tools_or_submitted_mementos() {
    let mut t = Transcript::default();
    t.apply(observation(3, "tool_request", Some(3), json!({"name":"end_session","arguments":{"memento":"Everything succeeded","finish_run":true}})), None).unwrap();
    let mut end: EventEnvelope =
        include_str!("../../playtime-protocol/tests/fixtures/two-session/events.jsonl")
            .lines()
            .map(|line| serde_json::from_str::<EventEnvelope>(line).unwrap())
            .find(|e| e.kind == "run_finished")
            .unwrap();
    end.run_id = playtime_core::RunId::new("readable-calls").unwrap();
    t.apply(end, None).unwrap();
    let window = t.window(View::Readable, 0, 64);
    let row = &window.rows[0];
    assert_eq!(row.title, "End session requested");
    assert_eq!(row.status, "No recorded result");
    assert!(row.sections.iter().any(|s| s.label == "Submitted memento"));
    assert!(!window.rows.iter().any(|r| r.checkpoint.is_some()));
}
#[test]
fn long_multi_tool_calls_are_windowed_and_malformed_or_unknown_tools_keep_source() {
    let mut t = Transcript::default();
    t.apply(request(), None).unwrap();
    for i in 2..1002 {
        t.apply(observation(i, "tool_request", Some(i), json!({"name":if i == 2 { "exec" } else { "future_tool" },"arguments":{"command":42,"value":"é".repeat(1000)}})), None).unwrap();
    }
    let w = t.window(View::Readable, 0, 64);
    assert_eq!(w.total, 1001);
    assert_eq!(w.rows.len(), 64);
    assert!(
        w.rows[1]
            .sections
            .iter()
            .any(|s| s.text.contains("Unavailable"))
    );
    assert!(w.rows[2].title.contains("future_tool"));
    assert!(
        t.raw(w.rows[2].first_sequence, 0)
            .unwrap()
            .text
            .contains("é")
    );
    assert_eq!(t.window(View::Readable, 900, 64).rows.len(), 64);
}

#[test]
fn streaming_text_is_provisional_and_final_text_replaces_it_in_readable() {
    let mut t = Transcript::default();
    t.apply(request(), None).unwrap();
    t.apply(observation(2, "model_chunk", None, json!({"call":1,"chunk":{"message":{"thinking":"unfinished thought","content":"partial answer"}}})), None).unwrap();
    let w = t.window(View::Readable, 0, 64);
    assert_eq!(w.rows[0].status, "Streaming · incomplete");
    assert!(
        w.rows[0]
            .sections
            .iter()
            .any(|s| s.text == "partial answer")
    );
    t.apply(observation(3, "model_response", None, json!({"call":1,"response":{"done":false,"generated_tokens":null,"prompt_tokens":null,"message":{"role":"assistant","thinking":"recorded final thought","content":""}}})), None).unwrap();
    let w = t.window(View::Readable, 0, 64);
    assert_eq!(w.rows[0].status, "Partial response recorded");
    assert_eq!(w.rows[0].sections.len(), 1);
    assert_eq!(w.rows[0].sections[0].text, "recorded final thought");
    assert!(!w.rows[0].status.contains('0'));
    assert!(
        t.raw(3, 0)
            .unwrap()
            .text
            .contains("\"generated_tokens\":null")
    );
}

#[test]
fn upstream_output_truncation_and_missing_fields_are_explicit() {
    let mut t = Transcript::default();
    t.apply(
        observation(
            3,
            "tool_request",
            Some(3),
            json!({"name":"exec","arguments":{"command":"produce output"}}),
        ),
        None,
    )
    .unwrap();
    t.apply(observation(4, "tool_result", Some(3), json!({"name":"exec","request_sequence":3,"result":{"exit_code":0,"stdout":"prefix only","output_truncated":true},"unabridged_result":null})), None).unwrap();
    let w = t.window(View::Readable, 0, 64);
    assert!(w.rows[0].sections.iter().any(|s| s.label == "Output limit"));
    assert!(
        w.rows[0]
            .sections
            .iter()
            .any(|s| s.label == "Standard error" && s.text.contains("Unavailable"))
    );
}

#[test]
fn a_long_stream_cannot_grow_a_prepared_section_beyond_its_limit() {
    let mut t = Transcript::default();
    t.apply(request(), None).unwrap();
    for seq in 2..502 {
        t.apply(
            observation(
                seq,
                "model_chunk",
                None,
                json!({"call":1,"chunk":{"message":{"content":"x".repeat(100)}}}),
            ),
            None,
        )
        .unwrap();
    }
    let w = t.window(View::Readable, 0, 1);
    assert_eq!(w.rows[0].sections.len(), 1);
    assert!(w.rows[0].sections[0].text.len() <= 4096);
    assert!(w.rows[0].sections[0].text.ends_with(" …"));
    assert!(t.raw(501, 0).is_some());
}
