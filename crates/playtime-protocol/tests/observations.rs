use playtime_core::UsageTotals;
use playtime_protocol::{
    ReadError,
    events::{DecodedEvent, EventPayload, KnownEvent, decode_event},
    observations::{
        DecodedObservation, KnownObservation, ObservationPayload, RecordedText, TextStatus,
        decode_observation, recorded_text,
    },
};
use serde_json::{Value, json};

const COMPLETED: &str = include_str!("fixtures/controller-observations/completed.jsonl");
const NOT_DISPATCHED: &str = include_str!("fixtures/controller-observations/not-dispatched.jsonl");
const PARTIAL: &str = include_str!("fixtures/controller-observations/partial-failure.jsonl");

fn events(fixture: &str) -> Vec<DecodedEvent> {
    fixture
        .lines()
        .map(|line| decode_event(line).unwrap())
        .collect()
}

fn observations(events: &[DecodedEvent]) -> Vec<&DecodedObservation> {
    events
        .iter()
        .filter_map(|event| match event.payload() {
            EventPayload::Observation(observation) => Some(observation.as_ref()),
            _ => None,
        })
        .collect()
}

fn source(kind: &str) -> Value {
    let all = events(COMPLETED);
    observations(&all)
        .into_iter()
        .find(|o| o.record().unwrap().kind == kind)
        .unwrap()
        .source()
        .clone()
}

#[test]
fn final_model_fields_replace_chunks_without_losing_the_recorded_stream() {
    let all = events(COMPLETED);
    let mut streamed = Vec::new();
    let mut final_message = None;
    for observation in observations(&all) {
        if observation
            .observation()
            .identity
            .request_id
            .as_ref()
            .map(|id| id.as_str())
            != Some("r1")
        {
            continue;
        }
        match observation.payload() {
            ObservationPayload::Known(known) => match known.as_ref() {
                KnownObservation::ModelChunk { chunk, .. } => {
                    streamed.push(chunk["message"].clone())
                }
                KnownObservation::ModelResponse { response, .. } => {
                    assert!(response.done);
                    final_message = Some(response.message.clone());
                }
                _ => {}
            },
            _ => panic!("fixture model records have typed decoding"),
        }
    }
    assert_eq!(streamed[1]["content"], "Streaming draft.");
    let final_message = final_message.unwrap();
    assert_eq!(
        recorded_text(&final_message, "content", TextStatus::Complete).unwrap(),
        RecordedText::Recorded {
            text: "Final text replaces the streamed draft.".into(),
            status: TextStatus::Complete,
        }
    );
    assert_eq!(final_message["thinking"], streamed[0]["thinking"]);
    assert_eq!(streamed.len(), 2);
}

#[test]
fn tool_intent_and_non_dispatch_share_identity_without_inventing_a_result() {
    let all = events(NOT_DISPATCHED);
    let observations = observations(&all);
    let intent = observations.iter().find(|o| matches!(o.payload(), ObservationPayload::Known(k) if matches!(k.as_ref(), KnownObservation::ToolIntent { .. }))).unwrap();
    let cancelled = observations
        .iter()
        .find(|o| o.record().unwrap().kind == "tool_not_dispatched")
        .unwrap();
    assert_eq!(
        intent.observation().identity,
        cancelled.observation().identity
    );
    assert_eq!(intent.observation().identity.effect_id.unwrap().get(), 3);
    assert!(
        !observations
            .iter()
            .any(|o| o.record().unwrap().kind == "tool_result")
    );
    assert!(!all.iter().any(|event| matches!(
        event.payload().known(),
        Some(KnownEvent::CommandFinished { .. })
    )));
    // An internal finished record is not an authoritative archive completion.
    assert!(
        observations
            .iter()
            .any(|o| o.record().unwrap().kind == "controller_finished")
    );
    assert!(!all.iter().any(|event| matches!(
        event.payload().known(),
        Some(KnownEvent::RunFinished { .. })
    )));
}

#[test]
fn partial_failed_session_retains_output_and_full_unknown_allowance() {
    let all = events(PARTIAL);
    let mut usage = UsageTotals::default();
    let mut chunks = 0;
    let mut failures = 0;
    for event in &all {
        if let Some(KnownEvent::RequestFinished { usage: settled, .. }) = event.payload().known() {
            usage.record(*settled).unwrap();
        }
    }
    for observation in observations(&all) {
        if observation
            .observation()
            .identity
            .request_id
            .as_ref()
            .map(|id| id.as_str())
            != Some("r3")
        {
            continue;
        }
        assert_eq!(
            observation
                .record()
                .unwrap()
                .session_id
                .as_ref()
                .unwrap()
                .as_str(),
            "s2"
        );
        let ObservationPayload::Known(known) = observation.payload() else {
            panic!()
        };
        match known.as_ref() {
            KnownObservation::ModelChunk { chunk, .. } => {
                chunks += 1;
                assert_eq!(
                    recorded_text(&chunk["message"], "content", TextStatus::Partial).unwrap(),
                    RecordedText::Recorded {
                        text: "Unfinished session two".into(),
                        status: TextStatus::Partial,
                    }
                );
            }
            KnownObservation::ModelError {
                error,
                token_budget_charge,
                ..
            } => {
                failures += 1;
                assert_eq!(error.kind, "connection");
                assert_eq!(*token_budget_charge, 100);
            }
            KnownObservation::ModelResponse { .. } => panic!("no final response was recorded"),
            _ => {}
        }
    }
    assert_eq!((chunks, failures), (1, 1));
    assert_eq!(usage.known_generated_tokens(), 13);
    assert_eq!(usage.unknown_reserved_tokens(), 100);
    assert_eq!(usage.budget_charged_tokens(), 113);
}

#[test]
fn memory_and_evidence_keep_original_text_and_resolvable_controller_references() {
    let all = events(COMPLETED);
    let observations = observations(&all);
    let incoming = observations
        .iter()
        .find(|o| {
            o.record().unwrap().kind == "memory_received"
                && o.record().unwrap().data["ordinal"] == 2
        })
        .unwrap();
    let outgoing = observations
        .iter()
        .find(|o| {
            o.record().unwrap().kind == "controller_checkpoint"
                && o.record().unwrap().data["ordinal"] == 1
        })
        .unwrap();
    assert_eq!(
        incoming.record().unwrap().data["memory"],
        outgoing.record().unwrap().data["memory"]
    );
    assert_eq!(
        incoming.record().unwrap().data["evidence"],
        outgoing.record().unwrap().data["evidence"]
    );
    let data = &incoming.record().unwrap().data;
    assert_eq!(
        data["memory"]["content"],
        "Read persistent.txt in the next session."
    );
    assert!(
        data["evidence"]["content"]
            .as_str()
            .unwrap()
            .contains("\"command_record\":14")
    );
    let mapping: Value = serde_json::from_str(include_str!(
        "fixtures/controller-observations/mapping.json"
    ))
    .unwrap();
    for field in ["request", "dispatch", "result"] {
        let sequence = data["evidence"]["command_sources"][0][field]
            .as_u64()
            .unwrap();
        let location = mapping["cases"]["completed"]["locations"]
            .as_array()
            .unwrap()
            .iter()
            .find(|location| location["controller_sequence"] == sequence)
            .unwrap();
        assert_ne!(location["archive_sequence"], sequence);
        let event = &all[location["archive_sequence"].as_u64().unwrap() as usize - 1];
        let EventPayload::Observation(observation) = event.payload() else {
            panic!()
        };
        assert_eq!(observation.record().unwrap().sequence.get(), sequence);
    }
}

#[test]
fn missing_empty_partial_and_truncated_text_are_distinct() {
    let message = json!({"content": ""});
    assert_eq!(
        recorded_text(&message, "thinking", TextStatus::Complete).unwrap(),
        RecordedText::NotRecorded
    );
    for status in [
        TextStatus::Partial,
        TextStatus::Complete,
        TextStatus::Truncated,
    ] {
        assert_eq!(
            recorded_text(&message, "content", status).unwrap(),
            RecordedText::Recorded {
                text: String::new(),
                status
            }
        );
    }
    assert!(recorded_text(&json!({"content": null}), "content", TextStatus::Partial).is_err());
}

#[test]
fn unknown_fields_kinds_and_raw_chunks_remain_inspectable() {
    let mut value = source("tool_result");
    value["future_wrapper"] = json!({"new": true});
    value["future_record"] = json!(0);
    value["data"]["value"]["result"]["context_truncated"] = json!(true);
    value["data"]["value"]["unabridged_result"] =
        json!({"stdout": "full recorded output", "unknown_result_field": false});
    let decoded = decode_observation(&value.to_string()).unwrap();
    assert_eq!(decoded.source(), &value);
    let ObservationPayload::Known(known) = decoded.payload() else {
        panic!()
    };
    let KnownObservation::ToolResult {
        result,
        unabridged_result,
        ..
    } = known.as_ref()
    else {
        panic!()
    };
    assert_eq!(result["context_truncated"], true);
    assert_eq!(
        unabridged_result.as_ref().unwrap()["unknown_result_field"],
        false
    );
    value["observation_kind"] = json!("future_observation");
    value["data"]["value"] = json!([false, 0, null, {"new": "kept"}]);
    let decoded = decode_observation(&value.to_string()).unwrap();
    assert_eq!(decoded.source(), &value);
    assert!(matches!(
        decoded.payload(),
        ObservationPayload::Unknown { .. }
    ));
    // The controller records raw chunks, even malformed model fields. The
    // typed record remains readable; text interpretation can fail separately.
    let mut value = source("model_chunk");
    value["data"]["value"]["chunk"] = json!({"message": {"content": 12}});
    assert!(decode_observation(&value.to_string()).is_ok());
}

#[test]
fn unsupported_versions_malformed_known_payloads_and_conflicting_identities_fail_explicitly() {
    assert!(matches!(
        decode_observation(r#"{"schema_version":2,"future":"body"}"#),
        Err(ReadError::UnsupportedVersion { .. })
    ));
    let base = source("model_request");
    for (pointer, replacement) in [
        ("/controller_sequence", json!(0)),
        ("/session_id", Value::Null),
        ("/data/value/maximum_tokens", json!(0)),
        ("/data/value/messages", json!("wrong shape")),
        ("/identity/effect_id", Value::Null),
        ("/identity/request_id", json!("different-request")),
    ] {
        let mut value = base.clone();
        *value.pointer_mut(pointer).unwrap() = replacement;
        assert!(decode_observation(&value.to_string()).is_err(), "{pointer}");
    }
    let mut value = source("tool_dispatch");
    value["identity"]["tool_request_sequence"] = json!(999);
    assert!(decode_observation(&value.to_string()).is_err());
    let mut value = source("request_settled");
    value["data"]["value"]["totals"]["budget_charged_tokens"] = json!(999);
    assert!(decode_observation(&value.to_string()).is_err());
    let event = json!({"schema_version": 1, "run_id": "simulation", "sequence": 1,
        "time": "2026-01-01T00:00:00Z", "kind": "controller_observation", "data": value});
    assert!(decode_event(&event.to_string()).is_err());
}

#[test]
fn source_run_session_and_cleanup_cannot_contradict_the_typed_envelope() {
    for kind in ["controller_started", "controller_finished"] {
        let event = json!({"schema_version": 1, "run_id": "another-run", "sequence": 1,
            "time": "2026-01-01T00:00:00Z", "kind": "controller_observation", "data": source(kind)});
        assert!(decode_event(&event.to_string()).is_err());
    }
    let mut phase = source("phase_changed");
    phase["data"]["value"]["session_id"] = json!("another-session");
    assert!(decode_observation(&phase.to_string()).is_err());
    let mut finished = source("controller_finished");
    finished["data"]["value"]["cleanup"] = json!({"status": "failed", "errors": []});
    assert!(decode_observation(&finished.to_string()).is_err());
    let mut intent = source("tool_dispatch");
    intent["identity"]["effect_id"] = Value::Null;
    assert!(decode_observation(&intent.to_string()).is_err());
}

#[test]
fn large_payloads_require_verified_artifacts_and_decode_to_the_same_source_value() {
    use playtime_protocol::observations::{INLINE_OBSERVATION_BYTES, ObservationPayload};
    use sha2::{Digest, Sha256};
    let mut wrapper = source("model_response");
    let mut value = wrapper["data"]["value"].clone();
    value["response"]["message"]["content"] = json!("é\"\n".repeat(100_000));
    let bytes = serde_json::to_vec(&value).unwrap();
    assert!(bytes.len() > INLINE_OBSERVATION_BYTES);
    wrapper["data"]["value"] = value.clone();
    assert!(decode_observation(&wrapper.to_string()).is_err());
    let hash: String = Sha256::digest(&bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    wrapper["data"] = json!({"storage": "artifact", "content": {
        "path": "artifacts/observations/9.json", "sha256": hash, "size_bytes": bytes.len()
    }});
    let unresolved = decode_observation(&wrapper.to_string()).unwrap();
    assert!(unresolved.record().is_none());
    assert!(matches!(
        unresolved.payload(),
        ObservationPayload::Referenced { .. }
    ));
    let resolved = unresolved.resolve(&bytes).unwrap();
    assert_eq!(resolved.record().unwrap().data, value);
    assert!(
        matches!(resolved.payload(), ObservationPayload::Known(k) if matches!(k.as_ref(), KnownObservation::ModelResponse { .. }))
    );
    assert!(unresolved.resolve(&bytes[..bytes.len() - 1]).is_err());
    let mut changed = bytes.clone();
    changed[1] ^= 1;
    assert!(unresolved.resolve(&changed).is_err());

    // A wrapper cannot use a referenced value to evade its run binding.
    let mut finished = source("controller_finished");
    let mut data = finished["data"]["value"].clone();
    data["future"] = json!("x".repeat(INLINE_OBSERVATION_BYTES));
    let bytes = serde_json::to_vec(&data).unwrap();
    let hash: String = Sha256::digest(&bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    finished["data"] = json!({"storage":"artifact","content":{"path":"artifacts/observations/final.json","sha256":hash,"size_bytes":bytes.len()}});
    let event = json!({"schema_version":1,"run_id":"different-run","sequence":1,"time":"2026-01-01T00:00:00Z","kind":"controller_observation","data":finished});
    let decoded = decode_event(&event.to_string()).unwrap();
    let EventPayload::Observation(observation) = decoded.payload() else {
        panic!()
    };
    assert!(observation.resolve(&bytes).is_err());
}

#[test]
fn inline_threshold_is_exact_serialized_value_bytes() {
    use playtime_protocol::observations::INLINE_OBSERVATION_BYTES;
    let mut wrapper = source("model_chunk");
    wrapper["observation_kind"] = json!("future_observation");
    wrapper["data"]["value"] = json!("x".repeat(INLINE_OBSERVATION_BYTES - 2));
    assert_eq!(
        serde_json::to_vec(&wrapper["data"]["value"]).unwrap().len(),
        INLINE_OBSERVATION_BYTES
    );
    assert!(decode_observation(&wrapper.to_string()).is_ok());
    wrapper["data"]["value"] = json!("x".repeat(INLINE_OBSERVATION_BYTES - 1));
    assert!(decode_observation(&wrapper.to_string()).is_err());
}
