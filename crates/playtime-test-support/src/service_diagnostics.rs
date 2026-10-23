//! Evidence summaries for failed process gates, never substitutes for assertions.
use playtime_protocol::events::EventEnvelope;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[derive(Default, Serialize)]
struct Session {
    work_model_calls: u64,
    forced_handoff_model_calls: u64,
    // A mention is not proof that the command wrote the file successfully.
    result_file_tool_request_sequences: Vec<u64>,
}

pub(super) fn summarize<'a>(events: impl Iterator<Item = &'a EventEnvelope>) -> Value {
    let mut sessions = BTreeMap::<String, Session>::new();
    let mut work_turn_limit = None;
    let mut uninspected_observations = 0_u64;
    for event in events.filter(|e| e.kind == "controller_observation") {
        let kind = event.data["observation_kind"].as_str().unwrap_or("");
        if !matches!(
            kind,
            "controller_started" | "model_request" | "tool_request"
        ) {
            continue;
        }
        let Some(value) = event.data["data"].get("value") else {
            uninspected_observations += 1;
            continue;
        };
        if kind == "controller_started" {
            work_turn_limit = value["parameters"]["limits"]["work_turns_per_session"].as_u64();
            continue;
        }
        let Some(id) = event.data["session_id"].as_str() else {
            uninspected_observations += 1;
            continue;
        };
        let session = sessions.entry(id.into()).or_default();
        match kind {
            "model_request" => match value["handoff"].as_bool() {
                Some(true) => session.forced_handoff_model_calls += 1,
                Some(false) => session.work_model_calls += 1,
                None => uninspected_observations += 1,
            },
            "tool_request" => {
                let arguments = &value["arguments"];
                let mentions_result = match value["name"].as_str() {
                    Some("exec") => arguments["command"]
                        .as_str()
                        .is_some_and(|s| s.contains("result.txt")),
                    Some("write_file") => matches!(
                        arguments["path"].as_str(),
                        Some("result.txt" | "/work/result.txt")
                    ),
                    _ => false,
                };
                if mentions_result {
                    session
                        .result_file_tool_request_sequences
                        .push(event.sequence.get());
                }
            }
            _ => (),
        }
    }
    json!({"work_turn_limit": work_turn_limit, "sessions": sessions,
        "uninspected_observations": uninspected_observations})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(sequence: u64, session: Option<&str>, kind: &str, value: Value) -> EventEnvelope {
        serde_json::from_value(json!({
            "schema_version": 1, "run_id": "diagnostic-test", "sequence": sequence,
            "time": "2026-10-23T00:00:00Z", "kind": "controller_observation",
            "data": {"session_id": session, "observation_kind": kind,
                "data": {"storage": "inline", "value": value}}
        }))
        .unwrap()
    }

    #[test]
    fn distinguishes_work_limit_handoff_and_file_mentions_from_success() {
        let mut events = vec![event(
            1,
            None,
            "controller_started",
            json!({"parameters": {"limits": {"work_turns_per_session": 6}}}),
        )];
        events.push(event(
            2,
            Some("s1"),
            "model_request",
            json!({"handoff": false}),
        ));
        for sequence in 3..9 {
            events.push(event(
                sequence,
                Some("s2"),
                "model_request",
                json!({"handoff": false}),
            ));
        }
        events.push(event(
            9,
            Some("s2"),
            "model_request",
            json!({"handoff": true}),
        ));
        // A false memento claim must not be mistaken for a file tool request.
        events.push(event(
            10,
            Some("s2"),
            "tool_request",
            json!({"name": "end_session", "arguments": {"memento": "result.txt exists"}}),
        ));
        let summary = summarize(events.iter());
        assert_eq!(summary["work_turn_limit"], 6);
        assert_eq!(summary["sessions"]["s1"]["work_model_calls"], 1);
        assert_eq!(summary["sessions"]["s2"]["work_model_calls"], 6);
        assert_eq!(summary["sessions"]["s2"]["forced_handoff_model_calls"], 1);
        assert_eq!(
            summary["sessions"]["s2"]["result_file_tool_request_sequences"],
            json!([])
        );
        events.push(event(
            11,
            Some("s2"),
            "tool_request",
            json!({"name": "exec", "arguments": {"command": "cat result.txt"}}),
        ));
        assert_eq!(
            summarize(events.iter())["sessions"]["s2"]["result_file_tool_request_sequences"],
            json!([11])
        );
    }

    #[test]
    fn referenced_observations_are_reported_as_uninspected() {
        let mut event = event(1, Some("s2"), "model_request", json!({"handoff": false}));
        event.data["data"] = json!({"storage": "content_ref"});
        let summary = summarize([event].iter());
        assert_eq!(summary["uninspected_observations"], 1);
        assert_eq!(summary["work_turn_limit"], Value::Null);
        assert_eq!(summary["sessions"], json!({}));
    }
}
