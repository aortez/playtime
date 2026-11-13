mod support;
use playtime_core::RequestPurpose;
use playtime_engine::Outcome;
use playtime_strata::*;
use serde_json::{Value, json};
use support::*;

fn decode(stream: &Stream) -> Result<Completion> {
    let mut decoder = Decoder::new(MODEL, Limits::default()).unwrap();
    decoder.push(&stream.bytes())?;
    decoder.finish()
}

#[test]
fn fragmented_reasoning_and_multiple_calls_round_trip_into_stateless_followup() {
    let mut stream = Stream::new();
    stream.item("reasoning", "Check café 🐦.", "completed");
    stream.item("function_call", r#"{"path":"a"}"#, "completed");
    stream.item("function_call", r#"{"path":"b"}"#, "completed");
    stream.terminal("completed");
    for size in [1, 2, 3, 7, 1024, usize::MAX] {
        let mut d = Decoder::new(MODEL, Limits::default()).unwrap();
        let mut chunks = Vec::new();
        for chunk in stream.bytes().chunks(size) {
            d.push(chunk).unwrap();
            chunks.extend(
                d.take_events()
                    .into_iter()
                    .filter_map(|e| e.controller_chunk()),
            );
        }
        assert!(!chunks.is_empty());
        let done = d.finish().unwrap();
        assert_eq!(done.response.generated_tokens, Some(40));
        assert_eq!(done.response.prompt_tokens, Some(200));
        assert_eq!(done.cached_input_tokens, Some(12));
        assert_eq!(done.reasoning_tokens, Some(20));
        assert_eq!(done.finish_reason, "tool_calls");
        let mut req = request();
        req.messages.push(done.response.message);
        req.messages
            .push(json!({"role":"tool","tool_name":"read_file","content":"first"}));
        req.messages
            .push(json!({"role":"tool","tool_name":"read_file","content":"second"}));
        let body: Value =
            serde_json::from_slice(&encode_request(&req, &lock(), &Limits::default()).unwrap())
                .unwrap();
        assert_eq!(body["input"][1]["content"][0]["text"], "Check café 🐦.");
        assert_eq!(body["input"][3]["call_id"], "call_1");
        assert_eq!(body["input"][5]["call_id"], "call_1");
        assert_eq!(body["input"][6]["call_id"], "call_2");
        assert_eq!(body["input"][3]["arguments"], r#"{"path":"a"}"#);
        assert_eq!(body["tools"][0]["name"], "read_file");
        assert_eq!(body["store"], false);
        assert!(body.get("previous_response_id").is_none());
    }
}

#[test]
fn only_terminal_usage_settles_a_parser_rejection_and_incomplete_calls_never_execute() {
    for (args, status) in [
        ("{", "completed"),
        ("[]", "completed"),
        (r#"{"path":"a"}"#, "incomplete"),
    ] {
        let mut stream = Stream::new();
        stream.item("function_call", args, status);
        stream.terminal("completed");
        let error = decode(&stream).unwrap_err();
        assert_eq!(error.kind, FailureKind::ToolParse);
        assert_eq!(error.generated_tokens, Some(40));
        let result = CallResult {
            completion: Err(error),
            remote: RemoteState::TerminalObserved,
            partial: Value::Null,
            received_bytes: 0,
        };
        assert!(matches!(
            result.into_outcome(),
            Outcome::ModelParseError {
                generated_tokens: 40,
                ..
            }
        ));
        stream.events.last_mut().unwrap()["response"]["usage"] = Value::Null;
        assert_eq!(decode(&stream).unwrap_err().generated_tokens, None);
    }
    let mut stream = Stream::new();
    stream.item("function_call", "{", "incomplete");
    stream.terminal("incomplete");
    let done = decode(&stream).unwrap();
    assert_eq!(done.finish_reason, "length");
    assert_eq!(done.response.message["tool_calls"], json!([]));
    assert_eq!(done.response.generated_tokens, Some(40));
}

#[test]
fn unknown_usage_stays_unknown_and_contradictory_usage_is_rejected() {
    let mut stream = answer();
    stream.events.last_mut().unwrap()["response"]["usage"] = Value::Null;
    let done = decode(&stream).unwrap();
    assert_eq!(done.response.generated_tokens, None);
    assert_eq!(done.response.prompt_tokens, None);
    for (key, value) in [
        ("output_tokens", json!(-1)),
        ("total_tokens", json!(999)),
        ("output_tokens_details", json!({"reasoning_tokens":"40"})),
        ("input_tokens_details", json!({"cached_tokens":201})),
    ] {
        let mut stream = answer();
        stream.events.last_mut().unwrap()["response"]["usage"][key] = value;
        assert_eq!(decode(&stream).unwrap_err().kind, FailureKind::Protocol);
    }
}

#[test]
fn malformed_identity_order_or_terminal_snapshot_cannot_authorize_tools() {
    for case in 0..8 {
        let mut stream = answer();
        match case {
            0 => stream.events[1]["sequence_number"] = json!(0),
            1 => stream.events[1]["response"]["model"] = json!("other"),
            2 => stream.events[4]["item_id"] = json!("other"),
            3 => {
                stream.events.last_mut().unwrap()["response"]["output"][0]["content"][0]["text"] =
                    json!("changed")
            }
            4 => stream.events[2]["item"]["type"] = json!("custom_tool_call"),
            5 => stream.events[2]["output_index"] = json!(2),
            6 => {
                let last = stream.events.last().unwrap().clone();
                stream.events.push(last);
            }
            _ => {
                stream.events.pop();
            }
        }
        assert!(decode(&stream).is_err(), "case {case}");
    }
}

#[test]
fn partial_evidence_survives_a_bad_frame_in_the_same_read_and_decoder_stays_failed() {
    let mut stream = answer();
    stream.events.truncate(5);
    let mut bytes = stream.bytes();
    bytes.extend_from_slice(b"event: broken\ndata: {\n\n");
    let mut decoder = Decoder::new(MODEL, Limits::default()).unwrap();
    assert!(decoder.push(&bytes).is_err());
    assert_eq!(decoder.take_events().len(), 5);
    assert_eq!(decoder.partial()["items"][0]["text"], "h");
    assert!(decoder.push(&[]).is_err());
    assert!(decoder.finish().is_err());
}

#[test]
fn keepalives_multiline_data_and_all_transport_bounds_are_enforced() {
    let bytes = answer().bytes();
    let text = String::from_utf8(bytes)
        .unwrap()
        .replace("data: {", "data: {\ndata: ");
    let mut d = Decoder::new(MODEL, Limits::default()).unwrap();
    d.push(b": keep-alive\r\n\r\n").unwrap();
    d.push(text.as_bytes()).unwrap();
    assert_eq!(d.finish().unwrap().response.message["content"], "héllo");
    for limits in [
        Limits {
            event_bytes: 10,
            ..Limits::default()
        },
        Limits {
            response_bytes: 10,
            ..Limits::default()
        },
        Limits {
            assembled_bytes: 3,
            ..Limits::default()
        },
        Limits {
            events: 3,
            ..Limits::default()
        },
    ] {
        let mut d = Decoder::new(MODEL, limits).unwrap();
        assert_eq!(
            d.push(&answer().bytes()).unwrap_err().kind,
            FailureKind::ByteLimit
        );
    }
    let mut d = Decoder::new(MODEL, Limits::default()).unwrap();
    d.push(&answer().bytes()).unwrap();
    d.push(b"data: {").unwrap();
    assert!(d.finish().is_err());
}

#[test]
fn requests_freeze_thinking_and_reject_unbounded_or_broken_tool_history() {
    let mut req = request();
    req.purpose = RequestPurpose::Compaction;
    let body: Value =
        serde_json::from_slice(&encode_request(&req, &lock(), &Limits::default()).unwrap())
            .unwrap();
    assert_eq!(body["reasoning"]["effort"], "none");
    assert_eq!(body["seed"], 42);
    for maximum in [0, u64::MAX, 163840] {
        req.maximum_tokens = maximum;
        assert!(encode_request(&req, &lock(), &Limits::default()).is_err());
    }
    let mut req = request();
    req.messages
        .push(json!({"role":"tool","tool_name":"read_file","content":"orphan"}));
    assert!(encode_request(&req, &lock(), &Limits::default()).is_err());
    let mut req = request();
    req.messages.push(json!({"role":"assistant","tool_calls":[{"id":"a","function":{"name":"read_file","arguments":{}}}]}));
    assert!(encode_request(&req, &lock(), &Limits::default()).is_err());
    req.messages
        .push(json!({"role":"tool","tool_name":"exec","content":"wrong"}));
    assert!(encode_request(&req, &lock(), &Limits::default()).is_err());
    let mut changed = lock();
    changed.settings.thinking = Thinking::High;
    assert_ne!(
        lock().fingerprint().unwrap(),
        changed.fingerprint().unwrap()
    );
    changed.host.quantization = "different".into();
    assert_ne!(
        lock().fingerprint().unwrap(),
        changed.fingerprint().unwrap()
    );
    assert!(
        encode_request(
            &request(),
            &lock(),
            &Limits {
                request_bytes: 2,
                ..Limits::default()
            }
        )
        .is_err()
    );
}
