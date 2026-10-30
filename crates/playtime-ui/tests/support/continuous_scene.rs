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
