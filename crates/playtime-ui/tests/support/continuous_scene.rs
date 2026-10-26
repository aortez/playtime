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
pub fn events() -> Vec<EventEnvelope> {
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
            json!({"call":1,"response":{"done":true,"message":{"content":"I will check the counter before continuing."}}}),
        ),
        observation(
            4,
            "tool_request",
            Some((1, "work")),
            Some(4),
            json!({"name":"exec","arguments":{"command":"cat /work/counter.txt","cwd":"/work"}}),
        ),
        observation(
            5,
            "tool_result",
            Some((1, "work")),
            Some(4),
            json!({"name":"exec","result":{"exit_code":0,"stdout":"stage 12; process continuity checked","stderr":""}}),
        ),
        observation(
            6,
            "compaction_planned",
            None,
            None,
            json!({"ordinal":1,"source_requests":["r1"],"retained_requests":[],"previous_summary":null,"previous_summary_request":null,"before_messages":[],"input_bound":13000,"projected_tokens":14000,"estimate_basis":"input_bytes","summary_input_bound":12000,"summary_estimate_basis":"input_bytes","evidence":{"schema_version":1,"requests":[{"request_id":"r1","purpose":"work","observation":null}],"plan":null}}),
        ),
        observation(
            7,
            "model_request",
            Some((2, "compaction")),
            None,
            json!({"purpose":"compaction","request_id":"r2","call":2}),
        ),
        observation(
            8,
            "model_response",
            Some((2, "compaction")),
            None,
            json!({"call":2,"response":{"done":true,"message":{"content":"Counter reached stage 12. Continue the probe; verify continuity before finishing."}}}),
        ),
        observation(
            9,
            "compaction_applied",
            Some((2, "compaction")),
            None,
            json!({"ordinal":1,"summary_request_id":"r2","summary":"Counter reached stage 12. Continue the probe; verify continuity before finishing.","messages":[],"input_bound":4000,"projected_tokens":5000,"estimate_basis":"input_bytes","evidence":{"schema_version":1,"requests":[{"request_id":"r2","purpose":"compaction","observation":null}],"plan":null}}),
        ),
        observation(
            10,
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
