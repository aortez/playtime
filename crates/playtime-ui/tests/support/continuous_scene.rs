//! Explicit synthetic v2 evidence for presentation tests; no model or worker.
use playtime_protocol::events::EventEnvelope;
use serde_json::{Value, json};

fn event(seq: u64, fact: Value) -> EventEnvelope {
    serde_json::from_value(json!({"schema_version":1,"run_id":"continuous-ui","sequence":seq,"time":"2026-10-25T00:00:00Z","kind":"execution_v2","data":{"schema_version":2,"fact":fact}})).unwrap()
}
fn observation(
    seq: u64,
    kind: &str,
    call: Option<(u64, &str)>,
    tool: Option<u64>,
    value: Value,
) -> EventEnvelope {
    let request = call.map(|(call,purpose)| json!({"effect_id":call,"request_id":format!("r{call}"),"call":call,"scope":{"kind":"run"},"purpose":purpose}));
    event(
        seq,
        json!({"kind":"observation","data":{"controller_sequence":seq,"elapsed_ms":seq*1000,"scope":{"kind":"run"},"observation_kind":kind,"identity":{"effect_id":call.map(|(id,_)|id),"request_id":call.map(|(id,_)|format!("r{id}")),"tool_request_sequence":tool},"request":request,"data":{"storage":"inline","value":value}}}),
    )
}
/// A work call's response, recorded at `seq`.
pub fn response(seq: u64, call: u64, content: &str) -> EventEnvelope {
    observation(
        seq,
        "model_response",
        Some((call, "work")),
        None,
        json!({"call":call,"response":{"done":true,"message":{"content":content}}}),
    )
}
/// The long response's text: 300 lines with the narrow no-break space,
/// non-breaking hyphen and arrow that models write.
pub fn long_text() -> String {
    (1..=300)
        .map(|n| {
            format!(
                "Zoom {n}: the side\u{202f}by\u{202f}side renders agree, and the box\u{2011}counting estimate → 1.{:03}.\n",
                260 + n % 40
            )
        })
        .collect()
}
/// One work call whose response runs past a row's excerpt and a page.
pub fn long_response_events() -> Vec<EventEnvelope> {
    vec![
        observation(
            1,
            "controller_started",
            None,
            None,
            json!({"parameters":{"limits":{"wall_seconds":300,"generated_tokens":16384,"context_tokens":32768}}}),
        ),
        observation(
            2,
            "model_request",
            Some((1, "work")),
            None,
            json!({"purpose":"work","request_id":"r1","call":1,"context_input_bound":1150,"context_basis":"input_bytes","context_reserve":4096}),
        ),
        observation(
            3,
            "model_response",
            Some((1, "work")),
            None,
            json!({"call":1,"response":{"done":true,"message":{"thinking":"List each zoom with its estimate.","content":long_text()}}}),
        ),
    ]
}
/// One work call with a command, then a compaction and a completion claim.
/// Each record's controller time is its sequence in seconds, so the calls
/// take one second each. The compaction records start at sequence 7.
pub fn events() -> Vec<EventEnvelope> {
    vec![
        observation(
            1,
            "controller_started",
            None,
            None,
            json!({"parameters":{"limits":{"wall_seconds":300,"generated_tokens":16384,"context_tokens":32768}}}),
        ),
        observation(
            2,
            "compaction_configured",
            None,
            None,
            json!({"harness":"continuous_compaction_v1","safety_margin":1638,"settings":{"schema_version":1,"trigger_percent":75,"recent_exchanges":2,"summary_tokens":2048,"allow_finish_run":true}}),
        ),
        observation(
            3,
            "model_request",
            Some((1, "work")),
            None,
            json!({"purpose":"work","request_id":"r1","call":1,"context_input_bound":1150,"context_basis":"input_bytes","context_reserve":4096}),
        ),
        observation(
            4,
            "model_response",
            Some((1, "work")),
            None,
            json!({"call":1,"response":{"done":true,"prompt_tokens":1200,"generated_tokens":48,"message":{"content":"I will check the counter before continuing."}}}),
        ),
        observation(
            5,
            "tool_request",
            Some((1, "work")),
            Some(5),
            json!({"name":"exec","arguments":{"command":"cat /work/counter.txt","cwd":"/work"}}),
        ),
        observation(
            6,
            "tool_result",
            Some((1, "work")),
            Some(5),
            json!({"name":"exec","result":{"exit_code":0,"stdout":"stage 12; process continuity checked","stderr":""}}),
        ),
        observation(
            7,
            "compaction_planned",
            None,
            None,
            json!({"ordinal":1,"source_requests":["r1"],"retained_requests":[],"previous_summary":null,"previous_summary_request":null,"before_messages":[],"input_bound":13000,"projected_tokens":14000,"estimate_basis":"input_bytes","summary_input_bound":12000,"summary_estimate_basis":"input_bytes","evidence":{"schema_version":1,"requests":[{"request_id":"r1","purpose":"work","observation":null}],"plan":null}}),
        ),
        observation(
            8,
            "model_request",
            Some((2, "compaction")),
            None,
            json!({"purpose":"compaction","request_id":"r2","call":2,"context_input_bound":11800,"context_basis":"calibrated_bytes","context_reserve":4096}),
        ),
        observation(
            9,
            "model_response",
            Some((2, "compaction")),
            None,
            json!({"call":2,"response":{"done":true,"prompt_tokens":11800,"generated_tokens":36,"message":{"content":"Counter reached stage 12. Continue the probe; verify continuity before finishing."}}}),
        ),
        observation(
            10,
            "compaction_applied",
            Some((2, "compaction")),
            None,
            json!({"ordinal":1,"summary_request_id":"r2","summary":"Counter reached stage 12. Continue the probe; verify continuity before finishing.","messages":[],"input_bound":4000,"projected_tokens":5000,"estimate_basis":"input_bytes","evidence":{"schema_version":1,"requests":[{"request_id":"r2","purpose":"compaction","observation":null}],"plan":null}}),
        ),
        observation(
            11,
            "completion_declared",
            Some((3, "work")),
            None,
            json!({"request_id":"r3","summary":"The probe reports continuity-ok; final output is in /work/result.txt.","evidence":{"schema_version":1,"requests":[{"request_id":"r3","purpose":"work","observation":null}],"plan":null}}),
        ),
    ]
}
/// A summary that reaches its token limit, so the controller stops the run.
pub fn failed_summary_events() -> Vec<EventEnvelope> {
    vec![
        observation(
            1,
            "controller_started",
            None,
            None,
            json!({"parameters":{"limits":{"wall_seconds":300,"generated_tokens":16384}}}),
        ),
        observation(
            2,
            "model_request",
            Some((1, "work")),
            None,
            json!({"purpose":"work","request_id":"r1","call":1}),
        ),
        observation(
            3,
            "model_response",
            Some((1, "work")),
            None,
            json!({"call":1,"response":{"done":true,"message":{"content":"Stage 30 recorded; continuing the probe."}}}),
        ),
        observation(
            4,
            "model_completion",
            Some((1, "work")),
            None,
            json!({"call":1,"finish_reason":"stop"}),
        ),
        observation(
            5,
            "compaction_planned",
            None,
            None,
            json!({"ordinal":1,"source_requests":["r1"],"retained_requests":[],"previous_summary":null,"previous_summary_request":null,"before_messages":[],"input_bound":4800,"projected_tokens":5300,"estimate_basis":"server_usage_plus_new_bytes","summary_input_bound":14000,"summary_estimate_basis":"input_bytes","evidence":{"schema_version":1,"requests":[{"request_id":"r1","purpose":"work","observation":null}],"plan":null}}),
        ),
        observation(
            6,
            "model_request",
            Some((2, "compaction")),
            None,
            json!({"purpose":"compaction","request_id":"r2","call":2}),
        ),
        observation(
            7,
            "model_response",
            Some((2, "compaction")),
            None,
            json!({"call":2,"response":{"done":true,"message":{"thinking":"The summary must keep the task, the stage count and","content":""}}}),
        ),
        observation(
            8,
            "model_completion",
            Some((2, "compaction")),
            None,
            json!({"call":2,"finish_reason":"length"}),
        ),
        observation(
            9,
            "controller_stop",
            None,
            None,
            json!({"reason":"compaction_failed","usage":{"known_generated_tokens":2560,"unknown_reserved_tokens":0,"unknown_requests":0,"budget_charged_tokens":2560,"usage_complete":true}}),
        ),
        observation(
            10,
            "controller_finished",
            None,
            None,
            json!({"reason":"compaction_failed","diagnostics":["summary requires explicit normal completion"]}),
        ),
    ]
}
/// Independent rejection records; a real run stops at the first one.
pub fn rejected_events() -> Vec<EventEnvelope> {
    vec![
        observation(
            1,
            "model_request",
            Some((1, "work")),
            None,
            json!({"purpose":"work","request_id":"r1","call":1}),
        ),
        observation(
            2,
            "model_response",
            Some((1, "work")),
            None,
            json!({"call":1,"response":{"done":true,"message":{"tool_calls":[{"function":{"name":"finish_run","arguments":{"summary":"Done."}}},{"function":{"name":"exec","arguments":{"command":"ls"}}}]}}}),
        ),
        observation(
            3,
            "tool_batch_rejected",
            Some((1, "work")),
            None,
            json!({"reason":"invalid_completion","error":"finish_run requires a normally completed response containing only that call"}),
        ),
        observation(
            4,
            "compaction_rejected",
            None,
            None,
            json!({"ordinal":0,"reason":"context_limit","detail":"summary request cannot fit with its allowance and safety margin"}),
        ),
    ]
}
/// A parse failure, a failed command, a deferred compaction and then a
/// rejected one, for the header's summary counts.
pub fn troubled_events() -> Vec<EventEnvelope> {
    vec![
        observation(
            1,
            "controller_started",
            None,
            None,
            json!({"parameters":{"limits":{"wall_seconds":300,"generated_tokens":16384,"context_tokens":32768}}}),
        ),
        observation(
            2,
            "compaction_configured",
            None,
            None,
            json!({"harness":"continuous_compaction_v1","safety_margin":1638,"settings":{"schema_version":1,"trigger_percent":50,"recent_exchanges":1,"summary_tokens":2048,"allow_finish_run":false}}),
        ),
        observation(
            3,
            "model_request",
            Some((1, "work")),
            None,
            json!({"purpose":"work","request_id":"r1","call":1,"context_input_bound":20000,"context_basis":"server_usage_plus_new_bytes","context_reserve":4096}),
        ),
        observation(
            4,
            "model_error",
            Some((1, "work")),
            None,
            json!({"consecutive_errors":1,"error":{"kind":"parse","message":"error parsing tool call: raw='{\"path\":'"},"recovery_allowed":true,"token_budget_charge":0}),
        ),
        observation(
            5,
            "model_request",
            Some((2, "work")),
            None,
            json!({"purpose":"work","request_id":"r2","call":2,"context_input_bound":20480,"context_basis":"calibrated_prefix_plus_new_bytes","context_reserve":4096}),
        ),
        observation(
            6,
            "model_response",
            Some((2, "work")),
            None,
            json!({"call":2,"response":{"done":true,"prompt_tokens":20480,"generated_tokens":10,"message":{"content":""}}}),
        ),
        observation(
            7,
            "tool_request",
            Some((2, "work")),
            Some(7),
            json!({"name":"exec","arguments":{"command":"python3 check.py"}}),
        ),
        observation(
            8,
            "tool_result",
            Some((2, "work")),
            Some(7),
            json!({"name":"exec","result":{"exit_code":1,"stdout":"","stderr":"check failed"}}),
        ),
        observation(
            9,
            "compaction_deferred",
            None,
            None,
            json!({"ordinal":1,"remaining_ms":20000,"expected_summary_ms":45000,"projected_tokens":24576,"estimate_basis":"server_usage_plus_new_bytes","summary_input_bound":20480,"summary_estimate_basis":"calibrated_bytes"}),
        ),
        observation(
            10,
            "compaction_planned",
            None,
            None,
            json!({"ordinal":1,"source_requests":["r1","r2"],"retained_requests":[],"previous_summary":null,"previous_summary_request":null,"before_messages":[],"input_bound":20480,"projected_tokens":24576,"estimate_basis":"server_usage_plus_new_bytes","summary_input_bound":20480,"summary_estimate_basis":"calibrated_bytes","evidence":{"schema_version":1,"requests":[{"request_id":"r1","purpose":"work","observation":null},{"request_id":"r2","purpose":"work","observation":null}],"plan":null}}),
        ),
        observation(
            11,
            "compaction_rejected",
            None,
            None,
            json!({"ordinal":1,"reason":"context_limit","detail":"summary request cannot fit with its allowance and safety margin"}),
        ),
    ]
}

/// A work call the wall limit cut before its response, and one that failed to
/// parse: both settle with unknown usage, and only the second records an error.
pub fn cut_off_events() -> Vec<EventEnvelope> {
    let settled = |seq: u64, call: u64| {
        observation(
            seq,
            "request_settled",
            Some((call, "work")),
            None,
            json!({"call":call,"usage":{"status":"unknown","reserved_tokens":8192},
                "totals":{"budget_charged_tokens":8192,"known_generated_tokens":0,"unknown_requests":1,"unknown_reserved_tokens":8192,"usage_complete":false}}),
        )
    };
    vec![
        observation(
            1,
            "model_request",
            Some((1, "work")),
            None,
            json!({"purpose":"work","request_id":"r1","call":1,"context_input_bound":1150,"context_basis":"input_bytes","context_reserve":4096}),
        ),
        observation(
            2,
            "model_chunk",
            Some((1, "work")),
            None,
            json!({"call":1,"chunk":{"done":false,"message":{"thinking":"unfinished thought"}}}),
        ),
        settled(3, 1),
        observation(
            4,
            "model_error",
            Some((1, "work")),
            None,
            json!({
                "error":{"kind":"parse","message":"error parsing tool call: raw='x', err=unexpected end of JSON input"},
                "recovery_allowed":true,"token_budget_charge":8192,"consecutive_errors":1,"tool_parse_errors":1,
                "usage":{"budget_charged_tokens":8192,"known_generated_tokens":0,"unknown_requests":1,"unknown_reserved_tokens":8192,"usage_complete":false}}),
        ),
        observation(
            5,
            "model_request",
            Some((2, "work")),
            None,
            json!({"purpose":"work","request_id":"r2","call":2,"context_input_bound":1150,"context_basis":"input_bytes","context_reserve":4096}),
        ),
        observation(
            6,
            "model_chunk",
            Some((2, "work")),
            None,
            json!({"call":2,"chunk":{"done":false,"message":{"thinking":"cut by the wall"}}}),
        ),
        settled(7, 2),
        observation(
            8,
            "controller_stop",
            None,
            None,
            json!({"reason":"wall_limit"}),
        ),
    ]
}

/// The scene with an archive write the driver found slow, after the run's
/// first tool result.
pub fn delayed_events() -> Vec<EventEnvelope> {
    let mut events = events();
    let seq = events.len() as u64 + 1;
    events.push(observation(
        seq,
        "publication_delayed",
        None,
        None,
        json!({"waited_ms":6200,"events":3,"bytes":47827}),
    ));
    events
}
/// A run for the Results tab: three work calls, one of which fails to parse
/// and settles with unknown usage, two failed tools and a non-zero exit, a
/// compaction applied and one deferred, then the wall limit, the final
/// capture and the run's end. Each record's controller time is its sequence
/// in seconds.
pub fn results_events() -> Vec<EventEnvelope> {
    let capture = json!({"index":{"path":"artifacts/final/capture-index-v2.json","sha256":"b".repeat(64),"size_bytes":14400},
        "workspace":{"path":"artifacts/final/workspace.tar","sha256":"c".repeat(64),"size_bytes":13209600}});
    let settled = |seq: u64, call: u64, purpose: &str, usage: Value, totals: Value| {
        observation(
            seq,
            "request_settled",
            Some((call, purpose)),
            None,
            json!({"call":call,"usage":usage,"totals":totals}),
        )
    };
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
            json!({"harness":"continuous_compaction_v1","safety_margin":1638,"settings":{"schema_version":1,"trigger_percent":50,"recent_exchanges":1,"summary_tokens":2048,"allow_finish_run":false}}),
        ),
        observation(
            3,
            "model_request",
            Some((1, "work")),
            None,
            json!({"purpose":"work","request_id":"r1","call":1,"context_input_bound":1150,"context_basis":"input_bytes","context_reserve":4096}),
        ),
        settled(
            4,
            1,
            "work",
            json!({"status":"known","generated_tokens":120}),
            json!({"budget_charged_tokens":120,"known_generated_tokens":120,"unknown_requests":0,"unknown_reserved_tokens":0,"usage_complete":true}),
        ),
        observation(
            5,
            "model_response",
            Some((1, "work")),
            None,
            json!({"call":1,"response":{"done":true,"prompt_tokens":900,"generated_tokens":120,"message":{"content":"Checking the seed before plotting."}}}),
        ),
        observation(
            6,
            "tool_request",
            Some((1, "work")),
            Some(6),
            json!({"name":"exec","arguments":{"command":"python3 make.py","cwd":"/work"}}),
        ),
        observation(
            7,
            "tool_result",
            Some((1, "work")),
            Some(6),
            json!({"name":"exec","result":{"exit_code":0,"stdout":"wrote plot.png","stderr":""}}),
        ),
        observation(
            8,
            "tool_request",
            Some((1, "work")),
            Some(8),
            json!({"name":"write_file","arguments":{"path":"/work/notes.txt","content":"draft"}}),
        ),
        observation(
            9,
            "tool_result",
            Some((1, "work")),
            Some(8),
            json!({"name":"write_file","result":{"error":"OSError: [Errno 28] No space left on device"}}),
        ),
        observation(
            10,
            "model_request",
            Some((2, "work")),
            None,
            json!({"purpose":"work","request_id":"r2","call":2,"context_input_bound":1300,"context_basis":"server_usage_plus_new_bytes","context_reserve":4096}),
        ),
        observation(
            11,
            "model_error",
            Some((2, "work")),
            None,
            json!({
                "error":{"kind":"parse","message":"error parsing tool call: raw='{\"path\":'"},
                "recovery_allowed":true,"token_budget_charge":4096,"consecutive_errors":1,"tool_parse_errors":1,
                "usage":{"budget_charged_tokens":4216,"known_generated_tokens":120,"unknown_requests":1,"unknown_reserved_tokens":4096,"usage_complete":false}}),
        ),
        settled(
            12,
            2,
            "work",
            json!({"status":"unknown","reserved_tokens":4096}),
            json!({"budget_charged_tokens":4216,"known_generated_tokens":120,"unknown_requests":1,"unknown_reserved_tokens":4096,"usage_complete":false}),
        ),
        observation(
            13,
            "model_request",
            Some((3, "work")),
            None,
            json!({"purpose":"work","request_id":"r3","call":3,"context_input_bound":1400,"context_basis":"server_usage_plus_new_bytes","context_reserve":4096}),
        ),
        settled(
            14,
            3,
            "work",
            json!({"status":"known","generated_tokens":40}),
            json!({"budget_charged_tokens":4256,"known_generated_tokens":160,"unknown_requests":1,"unknown_reserved_tokens":4096,"usage_complete":false}),
        ),
        observation(
            15,
            "model_response",
            Some((3, "work")),
            None,
            json!({"call":3,"response":{"done":true,"generated_tokens":40,"message":{"content":"Listing the workshop root."}}}),
        ),
        observation(
            16,
            "tool_request",
            Some((3, "work")),
            Some(16),
            json!({"name":"exec","arguments":{"command":"ls /","cwd":"/"}}),
        ),
        observation(
            17,
            "tool_result",
            Some((3, "work")),
            Some(16),
            json!({"name":"exec","result":{"error":"ValueError: Writable paths and working directories must be inside /work"}}),
        ),
        observation(
            18,
            "compaction_planned",
            None,
            None,
            json!({"ordinal":1,"source_requests":["r1","r2","r3"],"retained_requests":[],"previous_summary":null,"previous_summary_request":null,"before_messages":[],"input_bound":13000,"projected_tokens":14000,"estimate_basis":"input_bytes","summary_input_bound":12000,"summary_estimate_basis":"input_bytes","evidence":{"schema_version":1,"requests":[{"request_id":"r1","purpose":"work","observation":null},{"request_id":"r2","purpose":"work","observation":null},{"request_id":"r3","purpose":"work","observation":null}],"plan":null}}),
        ),
        observation(
            19,
            "model_request",
            Some((4, "compaction")),
            None,
            json!({"purpose":"compaction","request_id":"r4","call":4,"context_input_bound":11800,"context_basis":"calibrated_bytes","context_reserve":4096}),
        ),
        settled(
            20,
            4,
            "compaction",
            json!({"status":"known","generated_tokens":36}),
            json!({"budget_charged_tokens":4292,"known_generated_tokens":196,"unknown_requests":1,"unknown_reserved_tokens":4096,"usage_complete":false}),
        ),
        observation(
            21,
            "model_response",
            Some((4, "compaction")),
            None,
            json!({"call":4,"response":{"done":true,"prompt_tokens":11800,"generated_tokens":36,"message":{"content":"plot.png is written; the disk filled while writing notes."}}}),
        ),
        observation(
            22,
            "compaction_applied",
            Some((4, "compaction")),
            None,
            json!({"ordinal":1,"summary_request_id":"r4","summary":"plot.png is written; the disk filled while writing notes.","messages":[],"input_bound":4000,"projected_tokens":5000,"estimate_basis":"input_bytes","evidence":{"schema_version":1,"requests":[{"request_id":"r4","purpose":"compaction","observation":null}],"plan":null}}),
        ),
        observation(
            23,
            "compaction_deferred",
            None,
            None,
            json!({"ordinal":2,"remaining_ms":20000,"expected_summary_ms":45000,"projected_tokens":24576,"estimate_basis":"server_usage_plus_new_bytes","summary_input_bound":20480,"summary_estimate_basis":"calibrated_bytes"}),
        ),
        observation(
            24,
            "controller_stop",
            None,
            None,
            json!({"reason":"wall_limit"}),
        ),
        event(
            25,
            json!({"kind":"capture_published","data":{"capture":capture}}),
        ),
        event(
            26,
            json!({"kind":"run_finished","data":{"cleanup":{"status":"complete"},"last_capture":capture,"reason":"wall_limit",
                "usage":{"budget_charged_tokens":4292,"known_generated_tokens":196,"unknown_requests":1,"unknown_reserved_tokens":4096,"usage_complete":false}}}),
        ),
    ]
}
