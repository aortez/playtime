mod common;

use common::*;
use playtime_protocol::{archive::ContentRef, observations::INLINE_OBSERVATION_BYTES};
use playtime_storage::{
    ArchiveWriter, BatchClass, ErrorCode, StorageLimits, content_ref, fixture::Fixture, sha256,
};
use serde_json::{Value, json};

fn line(value: Value) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(&value).unwrap();
    bytes.push(b'\n');
    bytes
}

fn observation(fixture: &Fixture, data: Value) -> Vec<u8> {
    line(
        json!({"schema_version":1,"run_id":fixture.descriptor.run_id,"sequence":2,"time":"2026-10-17T12:00:00Z","kind":"controller_observation","data":{
            "schema_version":1,"controller_sequence":7,"elapsed_ms":11,"session_id":"s1","observation_kind":"future_observation",
            "identity":{},"data":data
        }}),
    )
}

fn publish(writer: &mut ArchiveWriter, path: &str, bytes: &[u8]) -> ContentRef {
    let reference = content_ref(playtime_core::ArchivePath::new(path).unwrap(), bytes);
    writer.publish_artifact(&reference, bytes).unwrap();
    reference
}

fn bundle(
    writer: &mut ArchiveWriter,
    fixture: &Fixture,
    raw: &[u8],
    change_map: impl FnOnce(&mut Value),
    change_index: impl FnOnce(&mut Value),
) -> Vec<u8> {
    let original: Value = serde_json::from_slice(fixture.events[7]).unwrap();
    let checkpoint = &original["data"]["checkpoint"];
    let mut map = json!({"schema_version":1,"run_id":fixture.descriptor.run_id,"entries":[{"controller_sequence":7,"archive_sequence":2,"record_sha256":sha256(raw)}]});
    change_map(&mut map);
    let source_map = publish(
        writer,
        "sessions/s1/source-map.json",
        &serde_json::to_vec(&map).unwrap(),
    );
    let file_bytes = br#"{"untrusted_path":"/etc/shadow","content":{"path":"../../outside"}}"#;
    let file = publish(writer, "sessions/s1/files/file.json", file_bytes);
    let provenance = publish(writer, "sessions/s1/evidence-sources.json", b"{}");
    let empty = publish(writer, "sessions/s1/incoming-memory.txt", b"");
    let mut index = json!({"schema_version":1,"run_id":fixture.descriptor.run_id,"session_id":"s1","ordinal":1,
        "workspace":checkpoint["workspace"],
        "files":[{"path":"logical.json","size_bytes":file.size_bytes,"mode":420,"entry_type":"0","sha256":file.sha256,"link":null,"content":{"availability":"retained","content":file}}],
        "changes":[{"path":"logical.json","change":"added"}],
        "incoming_memory":{"status":"missing","content":empty},
        "outgoing_memory":{"status":"loaded","content":checkpoint["memento"]["content"]},
        "incoming_evidence":null,"outgoing_evidence":{"text":checkpoint["evidence"],"provenance":provenance},"source_map":source_map});
    change_index(&mut index);
    let reference = publish(
        writer,
        "sessions/s1/checkpoint-index.json",
        &serde_json::to_vec(&index).unwrap(),
    );
    changed(fixture.events[7], |event| {
        event["sequence"] = 3.into();
        event["data"]["checkpoint"]["index"] = json!(reference);
    })
}

#[test]
fn checkpoint_traversal_verifies_defined_leaves_and_exact_preceding_source_records() {
    let temp = Temp::new();
    let (mut writer, fixture) = writer(&temp, StorageLimits::default());
    artifacts(&mut writer, &fixture);
    let raw = observation(
        &fixture,
        json!({"storage":"inline","value":{"unknown":"retained","path":"../../outside"}}),
    );
    writer
        .append(&[fixture.events[0], &raw], BatchClass::Ordinary)
        .unwrap();
    let checkpoint = bundle(&mut writer, &fixture, &raw, |_| {}, |_| {});
    writer.append(&[&checkpoint], BatchClass::Ordinary).unwrap();
    drop(writer);
    let reader = reader(&temp);
    let snapshot = reader.offline_snapshot().unwrap();
    let refs = &snapshot.inspection().referenced_content;
    for path in [
        "sessions/s1/checkpoint-index.json",
        "sessions/s1/source-map.json",
        "sessions/s1/files/file.json",
        "sessions/s1/incoming-memory.txt",
        "sessions/s1/evidence-sources.json",
    ] {
        assert!(
            refs.iter().any(|reference| reference.path.as_str() == path),
            "{path}"
        );
    }
    fs_tamper(&temp, "sessions/s1/files/file.json");
    assert_eq!(
        reader.offline_snapshot().unwrap_err().code,
        ErrorCode::ContentMismatch
    );
}

fn fs_tamper(temp: &Temp, path: &str) {
    std::fs::write(temp.archive().join(path), b"corruption").unwrap();
}

#[test]
fn source_maps_cannot_reference_an_uncommitted_batch_or_a_wrong_record() {
    for damage in [
        "same_batch",
        "lf_missing",
        "future",
        "wrong_source",
        "wrong_run",
    ] {
        let temp = Temp::new();
        let (mut writer, fixture) = writer(&temp, StorageLimits::default());
        artifacts(&mut writer, &fixture);
        let raw = observation(&fixture, json!({"storage":"inline","value":{}}));
        writer
            .append(&fixture.events[..1], BatchClass::Ordinary)
            .unwrap();
        if damage != "same_batch" {
            writer.append(&[&raw], BatchClass::Ordinary).unwrap();
        }
        let checkpoint = bundle(
            &mut writer,
            &fixture,
            &raw,
            |map| match damage {
                "lf_missing" => {
                    map["entries"][0]["record_sha256"] = json!(sha256(&raw[..raw.len() - 1]))
                }
                "future" => map["entries"][0]["archive_sequence"] = 3.into(),
                "wrong_source" => map["entries"][0]["controller_sequence"] = 8.into(),
                "wrong_run" => map["run_id"] = "other".into(),
                _ => {}
            },
            |_| {},
        );
        let lines: Vec<&[u8]> = if damage == "same_batch" {
            vec![&raw, &checkpoint]
        } else {
            vec![&checkpoint]
        };
        assert!(
            writer.append(&lines, BatchClass::Ordinary).is_err(),
            "{damage}"
        );
        assert!(!writer.is_stopped(), "invalid input failed before writing");
    }
}

#[test]
fn manifest_cycles_conflicting_references_unknown_versions_and_link_content_are_rejected() {
    for damage in [
        "cycle",
        "conflict",
        "version",
        "link",
        "traversal",
        "identity",
    ] {
        let temp = Temp::new();
        let (mut writer, fixture) = writer(&temp, StorageLimits::default());
        artifacts(&mut writer, &fixture);
        let raw = observation(&fixture, json!({"storage":"inline","value":{}}));
        writer
            .append(&[fixture.events[0], &raw], BatchClass::Ordinary)
            .unwrap();
        let checkpoint = bundle(
            &mut writer,
            &fixture,
            &raw,
            |_| {},
            |index| match damage {
                "cycle" => index["source_map"]["path"] = "sessions/s1/checkpoint-index.json".into(),
                "conflict" => {
                    let mut file = index["files"][0].clone();
                    file["path"] = "second".into();
                    file["content"]["content"]["sha256"] = json!(sha256(b"different"));
                    file["sha256"] = json!(sha256(b"different"));
                    index["files"].as_array_mut().unwrap().push(file);
                }
                "version" => index["schema_version"] = 2.into(),
                "link" => index["files"][0]["link"] = "/etc/shadow".into(),
                "traversal" => {
                    index["files"][0]["content"]["availability"] = "recursive_manifest".into()
                }
                _ => index["session_id"] = "wrong".into(),
            },
        );
        assert!(
            writer.append(&[&checkpoint], BatchClass::Ordinary).is_err(),
            "{damage}"
        );
        assert_eq!(writer.confirmed_frontier().frontier().committed_sequence, 2);
    }
}

#[test]
fn observation_payload_objects_are_verified_and_do_not_authorize_arbitrary_json_paths() {
    let temp = Temp::new();
    let (mut writer, fixture) = writer(&temp, StorageLimits::default());
    let value = json!({"content":"x".repeat(INLINE_OBSERVATION_BYTES),"content_like":{"path":"../../outside"}});
    let bytes = serde_json::to_vec(&value).unwrap();
    let reference = publish(&mut writer, "artifacts/observation.json", &bytes);
    let raw = observation(&fixture, json!({"storage":"artifact","content":reference}));
    writer
        .append(&[fixture.events[0], &raw], BatchClass::Ordinary)
        .unwrap();
    drop(writer);
    let reader = reader(&temp);
    let snapshot = reader.offline_snapshot().unwrap();
    assert!(
        snapshot
            .inspection()
            .referenced_content
            .contains(&reference)
    );
    fs_tamper(&temp, "artifacts/observation.json");
    assert_eq!(
        reader.offline_snapshot().unwrap_err().code,
        ErrorCode::ContentMismatch
    );
}

#[test]
fn transitive_manifests_and_content_obey_the_same_byte_and_reference_limits() {
    for bound in ["manifest", "references", "aggregate"] {
        let temp = Temp::new();
        let (mut writer, fixture) = writer(&temp, StorageLimits::default());
        artifacts(&mut writer, &fixture);
        let raw = observation(&fixture, json!({"storage":"inline","value":{}}));
        writer
            .append(&[fixture.events[0], &raw], BatchClass::Ordinary)
            .unwrap();
        let checkpoint = bundle(
            &mut writer,
            &fixture,
            &raw,
            |_| {},
            |index| {
                index["future_extension"] = json!("x".repeat(fixture.manifest.len()));
            },
        );
        writer.append(&[&checkpoint], BatchClass::Ordinary).unwrap();
        drop(writer);
        let initial = reader(&temp).offline_snapshot().unwrap();
        let mut limits = StorageLimits::default();
        match bound {
            "manifest" => limits.manifest_bytes = fixture.manifest.len() as u64,
            "references" => {
                limits.references = initial.inspection().referenced_content.len() as u64 - 1
            }
            _ => {
                limits.verified_content_bytes = initial
                    .inspection()
                    .referenced_content
                    .iter()
                    .map(|reference| reference.size_bytes)
                    .sum::<u64>()
                    - 1
            }
        }
        // The checkpoint manifest is larger than the frozen input manifest.
        if bound == "manifest" {
            assert!(
                initial.inspection().checkpoints[0]
                    .index
                    .as_ref()
                    .unwrap()
                    .size_bytes
                    > limits.manifest_bytes
            );
        }
        let reader = playtime_storage::ArchiveReader::open(temp.archive(), limits).unwrap();
        assert_eq!(
            reader.offline_snapshot().unwrap_err().code,
            ErrorCode::LimitExceeded,
            "{bound}"
        );
    }
}
