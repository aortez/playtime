use playtime_protocol::{
    ReadError,
    archive::{decode_input_manifest, decode_readable_input_manifest},
    events::{EventPayload, decode_event},
    execution::{self, Fact},
};
use serde_json::{Value, json};
const CONTRACT: &str =
    include_str!("../../playtime-runner/tests/fixtures/workspace-execution-v2.json");
fn records() -> Vec<Value> {
    serde_json::from_str(CONTRACT).unwrap()
}
fn envelope(record: &Value) -> Value {
    json!({"schema_version":1,"run_id":"probe","sequence":1,"time":"2026-10-17T00:00:00Z","kind":"execution_v2","data":record})
}
#[test]
fn new_contract_roundtrips_all_record_kinds_and_preserves_raw_optional_fields() {
    for record in records() {
        let decoded = execution::decode_record(&record.to_string()).unwrap();
        assert_eq!(serde_json::to_value(&decoded).unwrap(), record);
        let mut wire = envelope(&record);
        wire["data"]["future_optional"] = json!({"retained":true});
        let event = decode_event(&wire.to_string()).unwrap();
        assert!(matches!(event.payload(), EventPayload::Execution(_)));
        assert_eq!(serde_json::to_value(event.envelope()).unwrap(), wire);
        if let Fact::Observation(observation) = &decoded.fact {
            assert!(matches!(
                observation.scope,
                playtime_core::RequestScope::Run
            ));
        }
    }
}
#[test]
fn future_versions_fail_explicitly_before_interpreting_unknown_bodies() {
    for version in [0, 1, 3, u32::MAX] {
        let record = json!({"schema_version":version,"future":[]});
        assert!(
            matches!(execution::decode_record(&record.to_string()), Err(ReadError::UnsupportedVersion { received, .. }) if received == version)
        );
        assert!(
            matches!(decode_event(&envelope(&record).to_string()), Err(ReadError::UnsupportedVersion { received, .. }) if received == version)
        );
        assert!(
            matches!(execution::decode_capture_index(&record.to_string()), Err(ReadError::UnsupportedVersion { received, .. }) if received == version)
        );
    }
    let mut manifest: Value =
        serde_json::from_str(include_str!("fixtures/two-session/inputs/manifest.json")).unwrap();
    manifest["schema_version"] = json!(2);
    assert!(matches!(
        decode_input_manifest(&manifest.to_string()),
        Err(ReadError::UnsupportedVersion { received: 2, .. })
    ));
    assert_eq!(
        decode_readable_input_manifest(&manifest.to_string())
            .unwrap()
            .schema_version,
        2
    );
}
#[test]
fn ambiguous_purpose_or_scope_and_missing_admission_identity_are_rejected() {
    let request = records()
        .into_iter()
        .find(|record| record["fact"]["data"]["observation_kind"] == "model_request")
        .unwrap();
    for (pointer, value) in [
        ("/fact/data/request/purpose", json!("handoff")),
        (
            "/fact/data/request/scope",
            json!({"kind":"session","session_id":"s1"}),
        ),
        ("/fact/data/request/call", json!(2)),
        ("/fact/data/identity/request_id", json!("another")),
        ("/fact/data/request", Value::Null),
    ] {
        let mut invalid = request.clone();
        *invalid.pointer_mut(pointer).unwrap() = value;
        assert!(
            execution::decode_record(&invalid.to_string()).is_err(),
            "{pointer}"
        );
    }
    let mut invalid = request;
    invalid["fact"]["data"]["data"]["value"]["handoff"] = json!(false);
    assert!(execution::decode_record(&invalid.to_string()).is_err());
}

#[test]
fn capture_index_is_memoryless_and_retained_file_metadata_is_exact() {
    let bytes = include_str!("../../playtime-runner/tests/fixtures/workspace-capture-v2.json");
    let index = execution::decode_capture_index(bytes).unwrap();
    let value: Value = serde_json::from_str(bytes).unwrap();
    assert_eq!(serde_json::to_value(index).unwrap(), value);
    for change in [0, 1, 2] {
        let mut invalid = value.clone();
        match change {
            0 => invalid["files"][0]["size_bytes"] = json!(7),
            1 => invalid["files"][0]["link"] = json!("../../outside"),
            _ => invalid["files"][1]["path"] = invalid["files"][0]["path"].clone(),
        }
        assert!(execution::decode_capture_index(&invalid.to_string()).is_err());
    }
    assert!(value.get("session_id").is_none());
    assert!(value.get("memento").is_none());
}
