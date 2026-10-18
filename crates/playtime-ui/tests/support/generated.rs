//! Public synthetic inputs, generated on demand; no private runs or large archive.
use playtime_protocol::events::EventEnvelope;
use serde_json::json;
pub fn event(index: usize) -> EventEnvelope {
    let sequence = index + 1;
    let mut data = json!({
        "schema_version":1, "controller_sequence":sequence, "elapsed_ms":index * 10,
        "session_id":format!("s{}", index / 1000 + 1),
        "observation_kind":"model_chunk",
        "identity":{"effect_id":index / 10 + 1,"request_id":format!("r{}", index / 10 + 1),"tool_request_sequence":null},
        "data":{"storage":"inline","value":{"call":1,"chunk":{"message":{"content":"Recorded Unicode: café 日本語 🦀"}}}}
    });
    let mut kind = "controller_observation";
    if index % 10 == 9 {
        data["observation_kind"] = json!("model_response");
        data["data"]["value"] = json!({"call":1,"response":{"done":true,"generated_tokens":8,"prompt_tokens":null,"message":{"role":"assistant","content":"Final saved response replaces the streamed draft."}}});
    }
    if index % 1000 == 8 {
        kind = "future_observation";
        data = json!({"unknown_field":{"source":"retained exactly","disconnect_after":true}});
    }
    if index == 1234 {
        kind = "future_large_source";
        data = json!({"long_line":"é".repeat(490_000)});
    }
    serde_json::from_value(json!({"schema_version":1,"run_id":"synthetic-ui","sequence":sequence,"time":"2026-01-01T00:00:00Z","kind":kind,"data":data})).unwrap()
}
