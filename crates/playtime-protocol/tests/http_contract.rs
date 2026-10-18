use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use playtime_core::{RunId, Sha256Digest};
use playtime_protocol::api::*;

#[test]
fn mutation_fields_are_closed_and_duplicates_are_never_last_wins() {
    for input in [
        r#"{"recipe_id":"recipe","mode":"simulation","preview_revision":"v1","mode":"experiment"}"#,
        r#"{"recipe_id":"recipe","mode":"simulation","preview_revision":"v1","overrides":{}}"#,
        r#"{"recipe_id":"../recipe","mode":"simulation","preview_revision":"v1"}"#,
    ] {
        assert!(serde_json::from_str::<LaunchRequest>(input).is_err());
    }
    assert!(serde_json::from_str::<CancelRequest>(r#"{"grace_seconds":999}"#).is_err());
    assert!(
        serde_json::from_str::<PreviewRequest>(r#"{"mode":"simulation","path":"/tmp/x"}"#).is_err()
    );
    for key in ["_", "-", "abc_123-DEF", &"x".repeat(128)] {
        assert!(IdempotencyKey::new(key).is_ok());
    }
    for key in ["", "é", "a b", "../path", &"x".repeat(129)] {
        assert!(IdempotencyKey::new(key).is_err());
    }
}
fn frontier() -> Frontier {
    Frontier {
        run_id: RunId::new("run-cursor").unwrap(),
        manifest_sha256: Sha256Digest::new("11".repeat(32)).unwrap(),
        prefix: Prefix {
            sequence: 0,
            byte_offset: 0,
            sha256: Sha256Digest::new(
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            )
            .unwrap(),
        },
    }
}
#[test]
fn cursor_empty_prefix_and_numeric_and_encoding_boundaries() {
    let cursor = EventCursor::new(frontier(), 0).unwrap();
    let encoded = cursor.encode().unwrap();
    assert!(encoded.len() <= 2048);
    assert!(!encoded.contains('='));
    assert_eq!(EventCursor::decode(&encoded).unwrap(), cursor);
    assert_eq!(
        EventAnchors::new(frontier()).unwrap().history_cursor,
        EventAnchors::new(frontier()).unwrap().live_cursor
    );
    assert!(EventCursor::new(frontier(), 1).is_err());
    for value in [
        "",
        "not a cursor",
        &"a".repeat(2049),
        &(encoded.clone() + "="),
    ] {
        assert!(EventCursor::decode(value).is_err());
    }
    let raw = serde_json::to_string(&cursor).unwrap();
    for raw in [
        raw.replacen("\"schema_version\":1", "\"schema_version\":2", 1),
        raw.replacen(
            "\"schema_version\":1",
            "\"schema_version\":1,\"schema_version\":1",
            1,
        ),
        raw.replacen("\"after_sequence\":0", "\"after_sequence\":-1", 1),
        raw.replacen(
            "\"after_sequence\":0",
            "\"after_sequence\":18446744073709551616",
            1,
        ),
    ] {
        assert!(EventCursor::decode(&URL_SAFE_NO_PAD.encode(raw)).is_err());
    }
}
