use playtime_protocol::recovery::*;
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
};
fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/recovery")
}
fn bytes(path: &str) -> Vec<u8> {
    fs::read(root().join(path)).unwrap()
}
fn golden<T: RecoveryRecord>(path: &str) {
    let expected = bytes(path);
    let decoded: T = decode(&expected, 4 * 1024 * 1024).unwrap();
    assert_eq!(
        serde_json::to_vec(&decoded).unwrap(),
        expected,
        "exact {path} encoding"
    );
}
#[test]
fn exact_versioned_control_goldens() {
    golden::<ExecutionContractV1>("inputs/execution-contract.json");
    golden::<RecoveryBindingV1>("binding.json");
    golden::<RecoveryAttemptV1>("attempt.json");
    golden::<ConfirmedBoundsV1>("bounds.json");
    golden::<RecoveryPlanV1>("artifacts/recovery/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/plan.json");
    golden::<RecoveryOriginV1>("origin.json");
    golden::<RecoverySelectionV1>("recovery/intent.json");
    for name in ["terminal", "never-started", "abandoned"] {
        golden::<RecoveryResolutionV1>(&format!("resolution-{name}.json"));
    }
    let expected = bytes("source.json");
    let source: SourceEventV1 = serde_json::from_slice(&expected).unwrap();
    assert_eq!(serde_json::to_vec(&source).unwrap(), expected);
}
#[test]
fn every_golden_reference_hashes_exact_authored_bytes() {
    use sha2::{Digest, Sha256};
    let hashes: std::collections::BTreeMap<String, playtime_protocol::archive::ContentRef> =
        serde_json::from_slice(&bytes("hashes.json")).unwrap();
    for (path, reference) in hashes {
        let data = bytes(&path);
        assert_eq!(reference.size_bytes, data.len() as u64, "{path}");
        assert_eq!(
            reference.sha256.as_str(),
            Sha256::digest(&data)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>(),
            "{path}"
        );
    }
}
#[test]
fn strict_controls_reject_unknown_missing_duplicate_and_unsupported_fields() {
    let original: Value = serde_json::from_slice(&bytes("inputs/execution-contract.json")).unwrap();
    for mutation in 0..8 {
        let mut value = original.clone();
        match mutation {
            0 => {
                value["schema_version"] = json!(2);
            }
            1 => {
                value["future_control"] = json!(true);
            }
            2 => {
                value["configuration"]["host_path"] = json!("/not-authority");
            }
            3 => {
                value["storage_limits"]
                    .as_object_mut()
                    .unwrap()
                    .remove("events");
            }
            4 => {
                value["required_event_kinds"].as_array_mut().unwrap().pop();
            }
            5 => {
                value["effect_kinds"][0] = json!("unsupported_effect");
            }
            6 => {
                value["log_bytes"] = json!(0);
            }
            7 => {
                value["storage_limits"]["event_log_bytes"] = json!(1);
            }
            _ => unreachable!(),
        }
        assert!(
            decode::<ExecutionContractV1>(&serde_json::to_vec(&value).unwrap(), 65536).is_err(),
            "mutation {mutation}"
        );
    }
    let duplicate = String::from_utf8(bytes("inputs/execution-contract.json"))
        .unwrap()
        .replacen(
            "\"schema_version\":1",
            "\"schema_version\":1,\"schema_version\":1",
            1,
        );
    assert!(decode::<ExecutionContractV1>(duplicate.as_bytes(), 65536).is_err());
    let nested_duplicate = String::from_utf8(bytes("inputs/execution-contract.json"))
        .unwrap()
        .replace("\"events\":100000", "\"events\":100000,\"events\":100000");
    assert!(decode::<ExecutionContractV1>(nested_duplicate.as_bytes(), 65536).is_err());
    assert!(decode::<ExecutionContractV1>(&bytes("inputs/execution-contract.json"), 1).is_err());
}
#[test]
fn recovery_origin_is_omitted_for_original_events_and_exact_for_patch_records() {
    use playtime_protocol::events::{KnownEvent, decode_event};
    for line in String::from_utf8(bytes("events.jsonl")).unwrap().lines() {
        let event = decode_event(line).unwrap();
        if let Some(known @ (KnownEvent::RequestFinished { .. } | KnownEvent::RunFinished { .. })) =
            event.payload().known()
        {
            assert!(
                serde_json::to_value(known).unwrap()["data"]
                    .get("recovery_origin")
                    .is_none()
            );
        }
    }
    for line in String::from_utf8(bytes(
        "recovery/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/patch.jsonl",
    ))
    .unwrap()
    .lines()
    {
        let event = decode_event(line).unwrap();
        let origin = match event.payload().known().unwrap() {
            KnownEvent::RequestFinished {
                recovery_origin, ..
            }
            | KnownEvent::RunFinished {
                recovery_origin, ..
            } => recovery_origin.as_ref().unwrap(),
            _ => panic!("no invented controller observations"),
        };
        origin.validate().unwrap();
        assert_eq!(serde_json::to_vec(origin).unwrap(), bytes("origin.json"));
    }
}
