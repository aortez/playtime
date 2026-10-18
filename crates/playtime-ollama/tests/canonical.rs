use playtime_ollama::{FailureKind, ModelLock, Profile, json};
use serde_json::Value;

#[test]
fn independently_frozen_python_canonical_vectors() {
    let vectors = [
        (
            include_bytes!("fixtures/canonical-0.input.json").as_slice(),
            include_bytes!("fixtures/canonical-0.expected.json").as_slice(),
        ),
        (
            include_bytes!("fixtures/canonical-1.input.json").as_slice(),
            include_bytes!("fixtures/canonical-1.expected.json").as_slice(),
        ),
        (
            include_bytes!("fixtures/canonical-2.input.json").as_slice(),
            include_bytes!("fixtures/canonical-2.expected.json").as_slice(),
        ),
        (
            include_bytes!("fixtures/canonical-3.input.json").as_slice(),
            include_bytes!("fixtures/canonical-3.expected.json").as_slice(),
        ),
        (
            include_bytes!("fixtures/canonical-4.input.json").as_slice(),
            include_bytes!("fixtures/canonical-4.expected.json").as_slice(),
        ),
    ];
    let manifest: Vec<Value> =
        serde_json::from_slice(include_bytes!("fixtures/canonical-manifest.json")).unwrap();
    for ((input, expected), meta) in vectors.into_iter().zip(manifest) {
        let native = json::canonical(&json::parse(input, 16384, true).unwrap(), 16384).unwrap();
        assert_eq!(
            String::from_utf8_lossy(&native),
            String::from_utf8_lossy(expected)
        );
        assert_eq!(native.len() as u64, meta["length"].as_u64().unwrap());
        assert_eq!(json::hash(&native), meta["sha256"]);
    }
}

#[test]
fn committed_definition_matches_independent_bytes_without_lock_changes() {
    let lock = ModelLock::parse(include_bytes!("../../../models/gpt-oss-20b.lock.json")).unwrap();
    let bytes = json::canonical(&lock.definition, 16384).unwrap();
    assert_eq!(bytes, include_bytes!("fixtures/definition.canonical.json"));
    assert_eq!(bytes.len(), 8845);
    assert_eq!(json::hash(&bytes), Profile::DEFINITION);
    for field in ["name", "digest", "ollama_version", "definition_sha256"] {
        let mut value = serde_json::to_value(&lock).unwrap();
        value[field] = "changed".into();
        assert_eq!(
            ModelLock::parse(&serde_json::to_vec(&value).unwrap())
                .unwrap_err()
                .kind,
            FailureKind::Identity
        );
    }
}

#[test]
fn numeric_types_escaping_and_unsupported_identity_values() {
    assert_eq!(
        json::canonical(&json::parse(b"[-0,0,-0.0,0.0]", 256, true).unwrap(), 256).unwrap(),
        b"[0,0,-0.0,0.0]"
    );
    for input in [
        "18446744073709551616",
        "-9223372036854775809",
        "1e309",
        "1e-999",
        "NaN",
        "Infinity",
        "\"\\ud800\"",
        "{\"x\":0,\"x\":1}",
    ] {
        assert!(json::parse(input.as_bytes(), 256, true).is_err(), "{input}");
    }
    assert!(json::parse(b"[0]", 2, true).is_err());
    assert!(json::canonical(&Value::String("é".into()), 7).is_err());
    assert_eq!(
        json::canonical(&Value::String("é".into()), 8).unwrap(),
        br#""\u00e9""#
    );
    let supported = format!("{}0{}", "[".repeat(64), "]".repeat(64));
    assert!(json::parse(supported.as_bytes(), 1024, true).is_ok());
    assert!(json::parse(format!("[{supported}]").as_bytes(), 1024, true).is_err());
}
