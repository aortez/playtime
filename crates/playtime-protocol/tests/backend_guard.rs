use playtime_protocol::backend_guard::*;

#[test]
fn strict_independent_request_and_secret_diagnostics() {
    let bytes = br#"{"lease_id":"11111111111111111111111111111111","generation":1,"ordinal":1,"request_id":"22222222222222222222222222222222","effect_id":"33333333333333333333333333333333","body_sha256":"2d711642b726b04401627ca9fbac32f5c8530fb1903cc4db02258717921a4881","body_bytes":1,"original_max_tokens":256,"deadline_ms":5000}"#;
    let request: RequestSpec = serde_json::from_slice(bytes).unwrap();
    request.validate().unwrap();
    assert_eq!(serde_json::to_vec(&request).unwrap(), bytes);
    let mut value = serde_json::to_value(&request).unwrap();
    value["usage"] = serde_json::json!(0);
    assert!(serde_json::from_value::<RequestSpec>(value).is_err());
    assert!(
        serde_json::from_str::<RequestSpec>(
            &String::from_utf8(bytes.to_vec())
                .unwrap()
                .replace("\"ordinal\":1", "\"ordinal\":1,\"ordinal\":2")
        )
        .is_err()
    );
    for (field, value) in [
        ("body_bytes", BODY_BYTES as u64 + 1),
        ("ordinal", 0),
        ("generation", 0),
        ("original_max_tokens", 0),
    ] {
        let mut changed = serde_json::to_value(&request).unwrap();
        changed[field] = value.into();
        assert!(
            serde_json::from_value::<RequestSpec>(changed)
                .unwrap()
                .validate()
                .is_err()
        );
    }
    let cap = Capability::new("a".repeat(64)).unwrap();
    assert_eq!(format!("{cap:?}"), "Capability([redacted])");
    assert!(Capability::new("A".repeat(64)).is_err());
}

#[test]
fn strict_versioned_operations_have_no_accounting_or_termination_authority() {
    assert!(serde_json::from_str::<Command>(r#"{"command":"query","resend":true}"#).is_err());
    assert!(serde_json::from_str::<Event>(r#"{"kind":"settlement","tokens":0}"#).is_err());
    assert!(
        serde_json::from_str::<ManagerObservation>(r#"{"state":"terminated","pid":10}"#).is_err()
    );
    assert_eq!(SUPERVISOR_STOP_MS, 90_000);
    assert_eq!(STOP_OBSERVATION_MS, 120_000);
    assert_eq!(
        (
            CONTROL_BYTES,
            BODY_BYTES,
            FRAME_BYTES,
            RESPONSE_BYTES,
            QUEUE_BYTES
        ),
        (65_536, 4_194_304, 524_288, 4_194_304, 1_048_576)
    );
}
