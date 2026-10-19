mod common;

use std::{fs, io::Write, os::unix::fs::symlink};

use common::*;
use playtime_core::{ArchivePath, RunId};
use playtime_protocol::{
    events::{CleanupOutcome, KnownEvent, decode_event},
    storage::{
        EventQuery, FINALIZATION_RESERVE_BYTES, decode_archive_descriptor, decode_commit_marker,
    },
};
use playtime_storage::{
    ArchiveReader, ArchiveWriter, BatchClass, ErrorCode, StorageLimits, WriterLock, content_ref,
    fixture, sha256,
};
use serde_json::json;

#[test]
fn storage_format_goldens_pin_metadata_and_exact_prefix_identity() {
    let temp = Temp::new();
    let (mut writer, fixture) = writer(&temp, StorageLimits::default());
    assert_eq!(
        fs::read(temp.archive().join("archive.json")).unwrap(),
        include_bytes!("fixtures/archive.json")
    );
    assert_eq!(
        fs::read(temp.archive().join("events.commit.json")).unwrap(),
        include_bytes!("fixtures/empty.commit.json")
    );
    assert_eq!(
        writer.confirmed_frontier().frontier().marker.sha256,
        sha256(include_bytes!("fixtures/empty.commit.json"))
    );
    all_events(&mut writer, &fixture);
    assert_eq!(
        fs::read(temp.archive().join("events.commit.json")).unwrap(),
        include_bytes!("fixtures/complete.commit.json")
    );
    assert_eq!(
        writer.confirmed_frontier().frontier().marker.sha256,
        sha256(include_bytes!("fixtures/complete.commit.json"))
    );
    assert_eq!(writer.confirmed_frontier().frontier().committed_bytes, 5314);
}

#[test]
fn folder_only_copy_has_identical_inputs_events_accounting_checkpoints_and_cleanup() {
    let temp = Temp::new();
    let original = completed(&temp);
    let first = original.offline_snapshot().unwrap();
    let copy = temp.path.join("detached-copy");
    copy_directory(&temp.archive(), &copy);
    drop(original);
    // The reopened copy cannot consult the original archive directory.
    fs::remove_dir_all(temp.archive()).unwrap();
    let reader = ArchiveReader::open(copy, StorageLimits::default()).unwrap();
    let snapshot = reader.offline_snapshot().unwrap();
    assert_eq!(first.frontier(), snapshot.frontier());
    assert_eq!(first.inspection(), snapshot.inspection());
    let inspection = snapshot.inspection();
    assert_eq!(inspection.usage.known_generated_tokens, 10);
    assert_eq!(inspection.usage.unknown_reserved_tokens, 100);
    assert_eq!(inspection.usage.budget_charged_tokens, 110);
    assert!(!inspection.usage.usage_complete);
    assert_eq!(inspection.checkpoints.len(), 2);
    assert!(!inspection.incomplete_evidence);
    assert!(!inspection.worker.execution_checked);
    assert!(matches!(
        decode_event(std::str::from_utf8(&inspection.terminal.as_ref().unwrap().bytes).unwrap())
            .unwrap()
            .payload()
            .known(),
        Some(KnownEvent::RunFinished {
            cleanup: CleanupOutcome::Complete,
            ..
        })
    ));
    let fixture = fixture::two_session().unwrap();
    assert_eq!(
        reader
            .read_content(&snapshot, &reader.descriptor().input_manifest)
            .unwrap(),
        fixture.manifest
    );
    for input in fixture.inputs.iter().chain(&fixture.artifacts) {
        assert_eq!(
            reader.read_content(&snapshot, &input.reference).unwrap(),
            input.bytes
        );
    }
    let page = reader.page(&snapshot, &snapshot.query(0, 200)).unwrap();
    assert_eq!(
        page.events
            .into_iter()
            .map(|event| event.bytes)
            .collect::<Vec<_>>(),
        fixture.events
    );
    assert_eq!(
        snapshot.frontier().prefix_sha256,
        sha256(&fixture.events.concat())
    );
}

#[test]
fn fixed_frontier_pagination_does_not_advance_during_appends() {
    let temp = Temp::new();
    let (mut writer, fixture) = writer(&temp, StorageLimits::default());
    artifacts(&mut writer, &fixture);
    let receipt = writer
        .append(&fixture.events[..8], BatchClass::Ordinary)
        .unwrap();
    let reader = reader(&temp);
    assert_eq!(
        reader.offline_snapshot().unwrap_err().code,
        ErrorCode::WriterBusy
    );
    let snapshot = reader.live_snapshot(&receipt).unwrap();
    let first = reader.page(&snapshot, &snapshot.query(0, 3)).unwrap();
    assert_eq!(first.next_after_sequence, 3);
    assert!(first.has_more);
    let newer = writer
        .append(&fixture.events[8..15], BatchClass::Ordinary)
        .unwrap();
    let second = reader
        .page(&snapshot, &snapshot.query(first.next_after_sequence, 3))
        .unwrap();
    let last = reader
        .page(&snapshot, &snapshot.query(second.next_after_sequence, 3))
        .unwrap();
    let sequences: Vec<_> = first
        .events
        .into_iter()
        .chain(second.events)
        .chain(last.events)
        .map(|event| event.envelope.sequence.get())
        .collect();
    assert_eq!(sequences, (1..=8).collect::<Vec<_>>());
    assert!(!last.has_more);
    assert_eq!(last.frontier, *snapshot.frontier());
    assert_eq!(
        reader
            .live_snapshot(&newer)
            .unwrap()
            .frontier()
            .committed_sequence,
        15
    );
    assert_eq!(
        reader
            .live_snapshot(&receipt)
            .unwrap()
            .frontier()
            .committed_sequence,
        8
    );
}

#[test]
fn cursor_identity_bounds_and_default_limit_are_explicit() {
    let temp = Temp::new();
    let reader = completed(&temp);
    let snapshot = reader.offline_snapshot().unwrap();
    let query = snapshot.query(0, 200);
    let mut value = serde_json::to_value(&query).unwrap();
    value.as_object_mut().unwrap().remove("limit");
    assert_eq!(
        serde_json::from_value::<EventQuery>(value).unwrap().limit,
        200
    );
    for limit in [0, 1001] {
        let mut bad = query.clone();
        bad.limit = limit;
        assert_eq!(
            reader.page(&snapshot, &bad).unwrap_err().code,
            ErrorCode::InvalidCursor
        );
    }
    let mut bad = query.clone();
    bad.archive.run_id = RunId::new("another-run").unwrap();
    assert_eq!(
        reader.page(&snapshot, &bad).unwrap_err().code,
        ErrorCode::InvalidCursor
    );
    let mut bad = query.clone();
    bad.archive.input_manifest.sha256 = sha256(b"other");
    assert_eq!(
        reader.page(&snapshot, &bad).unwrap_err().code,
        ErrorCode::InvalidCursor
    );
    let mut bad = query.clone();
    bad.after_sequence = 17;
    assert_eq!(
        reader.page(&snapshot, &bad).unwrap_err().code,
        ErrorCode::CursorAhead
    );
    let mut bad = query.clone();
    bad.through_sequence = 17;
    assert_eq!(
        reader.page(&snapshot, &bad).unwrap_err().code,
        ErrorCode::CursorAhead
    );
    let mut bad = query;
    bad.through_sequence = 15;
    assert_eq!(
        reader.page(&snapshot, &bad).unwrap_err().code,
        ErrorCode::InvalidCursor
    );
}

#[test]
fn empty_log_is_prepared_but_incomplete_and_has_an_empty_page() {
    let temp = Temp::new();
    let (writer, _) = writer(&temp, StorageLimits::default());
    let receipt = writer.confirmed_frontier().clone();
    let reader = reader(&temp);
    let snapshot = reader.live_snapshot(&receipt).unwrap();
    assert_eq!(snapshot.frontier().prefix_sha256, sha256(b""));
    assert_eq!(snapshot.frontier().committed_bytes, 0);
    assert!(snapshot.inspection().incomplete_evidence);
    let page = reader.page(&snapshot, &snapshot.query(0, 200)).unwrap();
    assert!(page.events.is_empty());
    assert_eq!(page.next_after_sequence, 0);
    assert!(!page.has_more);
}

#[test]
fn unknown_events_and_optional_envelope_fields_keep_their_exact_bytes() {
    let temp = Temp::new();
    let (mut writer, fixture) = writer(&temp, StorageLimits::default());
    let unknown = changed(fixture.events[1], |value| {
        value["kind"] = json!("future_observation_v2");
        value["data"] = json!({"partial": "snowman ☃\n<script>", "unknown_tokens": null});
        value["future_envelope_field"] = json!([1, 2, 3]);
    });
    let receipt = writer
        .append(&[fixture.events[0], &unknown], BatchClass::Ordinary)
        .unwrap();
    let reader = reader(&temp);
    let snapshot = reader.live_snapshot(&receipt).unwrap();
    let page = reader.page(&snapshot, &snapshot.query(0, 2)).unwrap();
    assert_eq!(page.events[1].bytes, unknown);
    assert_eq!(page.events[1].sha256, sha256(&unknown));
    assert!(
        decode_event(std::str::from_utf8(&unknown).unwrap())
            .unwrap()
            .payload()
            .known()
            .is_none()
    );
}

#[test]
fn malformed_known_events_wrong_run_and_noncontiguous_sequences_do_not_write() {
    let temp = Temp::new();
    let (mut writer, fixture) = writer(&temp, StorageLimits::default());
    writer
        .append(&fixture.events[..1], BatchClass::Ordinary)
        .unwrap();
    let before = fs::read(temp.archive().join("events.jsonl")).unwrap();
    let mut bad_lines = vec![fixture.events[0].to_vec(), fixture.events[2].to_vec()];
    bad_lines.push(changed(fixture.events[1], |v| v["run_id"] = json!("wrong")));
    bad_lines.push(changed(fixture.events[1], |v| v["data"] = json!({})));
    bad_lines.push(changed(fixture.events[1], |v| v["sequence"] = json!(0)));
    bad_lines.push(fixture.events[1][..fixture.events[1].len() - 1].to_vec());
    bad_lines.push(vec![0xff, b'\n']);
    for line in bad_lines {
        assert_eq!(
            writer
                .append(&[&line], BatchClass::Ordinary)
                .unwrap_err()
                .code,
            ErrorCode::MalformedData
        );
        assert!(!writer.is_stopped());
        assert_eq!(
            fs::read(temp.archive().join("events.jsonl")).unwrap(),
            before
        );
    }
    writer
        .append(&fixture.events[1..2], BatchClass::Ordinary)
        .unwrap();
}

#[test]
fn terminal_usage_and_checkpoint_must_agree_with_the_log() {
    let temp = Temp::new();
    let (mut writer, fixture) = writer(&temp, StorageLimits::default());
    artifacts(&mut writer, &fixture);
    writer
        .append(&fixture.events[..15], BatchClass::Ordinary)
        .unwrap();
    for terminal in [
        changed(fixture.events[15], |v| {
            v["data"]["usage"]["known_generated_tokens"] = json!(11);
            v["data"]["usage"]["budget_charged_tokens"] = json!(111);
        }),
        changed(fixture.events[15], |v| {
            v["data"]["last_checkpoint"] = serde_json::from_slice::<serde_json::Value>(
                fixture.events[7],
            )
            .unwrap()["data"]["checkpoint"]
                .clone()
        }),
        changed(fixture.events[15], |v| {
            v["data"]["last_checkpoint"] = json!(null)
        }),
    ] {
        assert_eq!(
            writer
                .append(&[&terminal], BatchClass::Finalization)
                .unwrap_err()
                .code,
            ErrorCode::MalformedData
        );
    }
    assert_eq!(
        writer
            .append(&fixture.events[15..], BatchClass::Ordinary)
            .unwrap_err()
            .code,
        ErrorCode::MalformedData
    );
    writer
        .append(&fixture.events[15..], BatchClass::Finalization)
        .unwrap();
    let after = changed(fixture.events[1], |v| v["sequence"] = json!(17));
    assert_eq!(
        writer
            .append(&[&after], BatchClass::Finalization)
            .unwrap_err()
            .code,
        ErrorCode::MalformedData
    );
}

#[test]
fn duplicate_request_settlement_cannot_inflate_or_erase_usage() {
    let temp = Temp::new();
    let (mut writer, fixture) = writer(&temp, StorageLimits::default());
    writer
        .append(&fixture.events[..4], BatchClass::Ordinary)
        .unwrap();
    let duplicate = changed(fixture.events[3], |v| {
        v["sequence"] = json!(5);
        v["data"]["usage"] = json!({"status":"known", "generated_tokens":0});
    });
    assert_eq!(
        writer
            .append(&[&duplicate], BatchClass::Ordinary)
            .unwrap_err()
            .code,
        ErrorCode::MalformedData
    );
    let reader = reader(&temp);
    let snapshot = reader.live_snapshot(writer.confirmed_frontier()).unwrap();
    assert_eq!(snapshot.inspection().usage.unknown_reserved_tokens, 100);
}

#[test]
fn checkpoint_requires_verified_content_and_orphans_are_not_evidence() {
    let temp = Temp::new();
    let (mut writer, fixture) = writer(&temp, StorageLimits::default());
    writer
        .append(&fixture.events[..7], BatchClass::Ordinary)
        .unwrap();
    assert!(
        writer
            .append(&fixture.events[7..8], BatchClass::Ordinary)
            .is_err()
    );
    artifacts(&mut writer, &fixture);
    let reader = reader(&temp);
    let before = reader.live_snapshot(writer.confirmed_frontier()).unwrap();
    assert_eq!(
        reader
            .read_content(&before, &fixture.artifacts[0].reference)
            .unwrap_err()
            .code,
        ErrorCode::ContentMismatch
    );
    assert_eq!(reader.uncommitted_objects(&before).unwrap().len(), 6);
    let receipt = writer
        .append(&fixture.events[7..8], BatchClass::Ordinary)
        .unwrap();
    let after = reader.live_snapshot(&receipt).unwrap();
    assert_eq!(after.inspection().checkpoints.len(), 1);
    assert_eq!(reader.uncommitted_objects(&after).unwrap().len(), 3);
}

#[test]
fn immutable_objects_never_overwrite_different_bytes_or_frozen_inputs() {
    let temp = Temp::new();
    let (mut writer, fixture) = writer(&temp, StorageLimits::default());
    let artifact = &fixture.artifacts[0];
    writer
        .publish_artifact(&artifact.reference, &artifact.bytes)
        .unwrap();
    writer
        .publish_artifact(&artifact.reference, &artifact.bytes)
        .unwrap();
    let different = content_ref(artifact.reference.path.clone(), b"different");
    assert_eq!(
        writer
            .publish_artifact(&different, b"different")
            .unwrap_err()
            .code,
        ErrorCode::ContentMismatch
    );
    assert_eq!(
        fs::read(temp.archive().join(artifact.reference.path.as_str())).unwrap(),
        artifact.bytes
    );
    assert_eq!(
        writer
            .publish_artifact(&fixture.inputs[0].reference, &fixture.inputs[0].bytes)
            .unwrap_err()
            .code,
        ErrorCode::MalformedData
    );
    let staging = content_ref(ArchivePath::new(".staging/hidden").unwrap(), b"hidden");
    assert_eq!(
        writer
            .publish_artifact(&staging, b"hidden")
            .unwrap_err()
            .code,
        ErrorCode::MalformedData
    );
}

#[test]
fn result_is_optional_derived_and_cannot_fabricate_completion() {
    let temp = Temp::new();
    let (mut writer, fixture) = writer(&temp, StorageLimits::default());
    writer
        .append(&fixture.events[..1], BatchClass::Ordinary)
        .unwrap();
    fs::write(temp.archive().join("result.json"), fixture.events[15]).unwrap();
    let reader = reader(&temp);
    assert!(
        reader
            .live_snapshot(writer.confirmed_frontier())
            .unwrap()
            .inspection()
            .incomplete_evidence
    );
    assert_eq!(
        writer.regenerate_result().unwrap_err().code,
        ErrorCode::MalformedData
    );
    artifacts(&mut writer, &fixture);
    writer
        .append(&fixture.events[1..], BatchClass::Finalization)
        .unwrap();
    fs::write(temp.archive().join("result.json"), b"stale").unwrap();
    assert!(
        !reader
            .live_snapshot(writer.confirmed_frontier())
            .unwrap()
            .inspection()
            .incomplete_evidence
    );
    writer.regenerate_result().unwrap();
    assert_eq!(
        fs::read(temp.archive().join("result.json")).unwrap(),
        fixture.events[15]
    );
    fs::remove_file(temp.archive().join("result.json")).unwrap();
    drop(writer);
    assert!(
        !reader
            .offline_snapshot()
            .unwrap()
            .inspection()
            .incomplete_evidence
    );
}

#[test]
fn finalization_reserve_counts_exact_serialized_bytes_and_failed_terminal_stays_incomplete() {
    let temp = Temp::new();
    let fixture = fixture::two_session().unwrap();
    let limits = StorageLimits {
        event_log_bytes: FINALIZATION_RESERVE_BYTES + fixture.events[0].len() as u64,
        ..StorageLimits::default()
    };
    let (mut writer, fixture) = writer(&temp, limits);
    writer
        .append(&fixture.events[..1], BatchClass::Ordinary)
        .unwrap();
    assert_eq!(
        writer
            .append(&fixture.events[1..2], BatchClass::Ordinary)
            .unwrap_err()
            .code,
        ErrorCode::LimitExceeded
    );
    let terminal = changed(fixture.events[15], |v| {
        v["sequence"] = json!(2);
        v["data"]["usage"] = json!({"known_generated_tokens":0,"unknown_reserved_tokens":0,"unknown_requests":0,"budget_charged_tokens":0,"usage_complete":true});
        v["data"]["last_checkpoint"] = json!(null);
    });
    writer
        .append(&[&terminal], BatchClass::Finalization)
        .unwrap();
    let second = Temp::new();
    let limits = StorageLimits {
        event_log_bytes: fixture.events[0].len() as u64,
        ..StorageLimits::default()
    };
    let (mut writer, _) = common::writer(&second, limits);
    assert_eq!(
        writer
            .append(&fixture.events[..1], BatchClass::Ordinary)
            .unwrap_err()
            .code,
        ErrorCode::LimitExceeded
    );
    writer
        .append(&fixture.events[..1], BatchClass::Finalization)
        .unwrap();
    assert_eq!(
        writer
            .append(&[&terminal], BatchClass::Finalization)
            .unwrap_err()
            .code,
        ErrorCode::LimitExceeded
    );
    drop(writer);
    assert!(
        reader(&second)
            .offline_snapshot()
            .unwrap()
            .inspection()
            .incomplete_evidence
    );
}

#[test]
fn record_batch_content_and_page_limits_are_enforced_without_skipping_records() {
    let temp = Temp::new();
    let fixture = fixture::two_session().unwrap();
    let limits = StorageLimits {
        record_bytes: fixture.events[0].len() as u64 - 1,
        ..StorageLimits::default()
    };
    let (mut writer, _) = writer(&temp, limits);
    assert_eq!(
        writer
            .append(&fixture.events[..1], BatchClass::Ordinary)
            .unwrap_err()
            .code,
        ErrorCode::LimitExceeded
    );
    let another = Temp::new();
    let limits = StorageLimits {
        batch_bytes: 1024,
        record_bytes: 1024,
        page_bytes: 1024,
        ..StorageLimits::default()
    };
    let (mut writer, fixture) = common::writer(&another, limits.clone());
    artifacts(&mut writer, &fixture);
    assert_eq!(
        writer
            .append(&fixture.events[..7], BatchClass::Ordinary)
            .unwrap_err()
            .code,
        ErrorCode::LimitExceeded
    );
    for line in &fixture.events[..15] {
        writer.append(&[line], BatchClass::Ordinary).unwrap();
    }
    let reader = ArchiveReader::open(another.archive(), limits).unwrap();
    let snapshot = reader.live_snapshot(writer.confirmed_frontier()).unwrap();
    let mut after = 0;
    let mut sequences = Vec::new();
    loop {
        let page = reader
            .page(&snapshot, &snapshot.query(after, 1000))
            .unwrap();
        assert!(
            page.events
                .iter()
                .map(|event| event.bytes.len())
                .sum::<usize>()
                <= 1024
        );
        sequences.extend(
            page.events
                .iter()
                .map(|event| event.envelope.sequence.get()),
        );
        after = page.next_after_sequence;
        if !page.has_more {
            break;
        }
    }
    assert_eq!(sequences, (1..=15).collect::<Vec<_>>());
}

#[test]
fn creation_settings_and_versions_fail_explicitly() {
    assert!(serde_json::from_value::<StorageLimits>(json!({"surprise":true})).is_err());
    assert!(
        decode_archive_descriptor(r#"{"schema_version":2,"different_body":true}"#)
            .unwrap_err()
            .to_string()
            .contains("Unsupported")
    );
    assert!(
        decode_commit_marker(r#"{"schema_version":2,"different_body":true}"#)
            .unwrap_err()
            .to_string()
            .contains("Unsupported")
    );
    let temp = Temp::new();
    let fixture = fixture::two_session().unwrap();
    let mut descriptor = fixture.descriptor.clone();
    descriptor.schema_version = 2;
    let error = ArchiveWriter::create(
        temp.archive(),
        descriptor,
        fixture.manifest,
        &fixture.inputs,
        StorageLimits::default(),
    )
    .err()
    .unwrap();
    assert_eq!(error.code, ErrorCode::UnsupportedVersion);
    assert!(!temp.archive().exists());
    let mut value = serde_json::to_value(&fixture.descriptor).unwrap();
    value["unexpected_setting"] = json!(true);
    assert!(decode_archive_descriptor(&value.to_string()).is_err());
}

#[test]
fn existing_archive_is_preserved_and_writer_lock_lives_until_drop() {
    let temp = Temp::new();
    let (writer, fixture) = writer(&temp, StorageLimits::default());
    assert_eq!(
        WriterLock::try_acquire(temp.archive()).err().unwrap().code,
        ErrorCode::WriterBusy
    );
    let descriptor = fs::read(temp.archive().join("archive.json")).unwrap();
    assert!(
        ArchiveWriter::create(
            temp.archive(),
            fixture.descriptor,
            fixture.manifest,
            &fixture.inputs,
            StorageLimits::default()
        )
        .is_err()
    );
    assert_eq!(
        fs::read(temp.archive().join("archive.json")).unwrap(),
        descriptor
    );
    drop(writer);
    let _lock = WriterLock::try_acquire(temp.archive()).unwrap();
}

#[test]
fn uncommitted_complete_and_partial_lines_are_visible_only_as_diagnostics() {
    let temp = Temp::new();
    let (mut writer, fixture) = writer(&temp, StorageLimits::default());
    writer
        .append(&fixture.events[..1], BatchClass::Ordinary)
        .unwrap();
    drop(writer);
    let tail = [fixture.events[1], b"{\"partial"].concat();
    fs::OpenOptions::new()
        .append(true)
        .open(temp.archive().join("events.jsonl"))
        .unwrap()
        .write_all(&tail)
        .unwrap();
    let reader = reader(&temp);
    let snapshot = reader.offline_snapshot().unwrap();
    assert_eq!(snapshot.frontier().committed_sequence, 1);
    assert_eq!(
        snapshot.inspection().uncommitted_event_bytes,
        tail.len() as u64
    );
    assert_eq!(
        reader
            .page(&snapshot, &snapshot.query(0, 1000))
            .unwrap()
            .events
            .len(),
        1
    );
    assert_eq!(
        reader
            .diagnostic_event_bytes(snapshot.frontier().committed_bytes, tail.len() as u64)
            .unwrap(),
        tail
    );
    assert_eq!(
        fs::metadata(temp.archive().join("events.jsonl"))
            .unwrap()
            .len(),
        snapshot.frontier().committed_bytes + tail.len() as u64
    );
}

#[test]
fn prefix_corruption_is_not_silently_skipped_even_with_a_recomputed_digest() {
    for mutation in [
        "hash",
        "offset",
        "gap",
        "payload",
        "missing_lf",
        "marker_run",
        "marker_sequence",
    ] {
        let temp = Temp::new();
        completed(&temp);
        let fixture = fixture::two_session().unwrap();
        let mut bytes = fs::read(temp.archive().join("events.jsonl")).unwrap();
        let mut marker: serde_json::Value =
            serde_json::from_slice(&fs::read(temp.archive().join("events.commit.json")).unwrap())
                .unwrap();
        match mutation {
            "hash" => {
                bytes[0] = b' ';
            }
            "offset" => marker["committed_bytes"] = json!(bytes.len() + 1),
            "gap" => {
                let mut lines: Vec<Vec<u8>> =
                    fixture.events.iter().map(|line| line.to_vec()).collect();
                lines[3] = changed(&lines[3], |v| v["sequence"] = json!(5));
                bytes = lines.concat();
                marker["prefix_sha256"] = json!(sha256(&bytes));
                marker["committed_bytes"] = json!(bytes.len());
            }
            "payload" => {
                let mut lines: Vec<Vec<u8>> =
                    fixture.events.iter().map(|line| line.to_vec()).collect();
                lines[3] = changed(&lines[3], |v| v["data"] = json!({}));
                bytes = lines.concat();
                marker["prefix_sha256"] = json!(sha256(&bytes));
                marker["committed_bytes"] = json!(bytes.len());
            }
            "missing_lf" => {
                bytes.pop();
                marker["prefix_sha256"] = json!(sha256(&bytes));
                marker["committed_bytes"] = json!(bytes.len());
            }
            "marker_run" => marker["run_id"] = json!("other"),
            "marker_sequence" => marker["committed_sequence"] = json!(15),
            _ => unreachable!(),
        }
        fs::write(temp.archive().join("events.jsonl"), &bytes).unwrap();
        fs::write(
            temp.archive().join("events.commit.json"),
            marker.to_string(),
        )
        .unwrap();
        let reader = reader(&temp);
        assert_eq!(
            reader.offline_snapshot().unwrap_err().code,
            ErrorCode::ArchiveCorrupt,
            "{mutation}"
        );
        assert_eq!(
            reader
                .diagnostic_event_bytes(0, bytes.len() as u64)
                .unwrap(),
            bytes
        );
    }
}

#[test]
fn tampered_inputs_and_checkpoint_content_are_rejected_by_digest() {
    let temp = Temp::new();
    let reader = completed(&temp);
    let snapshot = reader.offline_snapshot().unwrap();
    let content = &snapshot.inspection().checkpoints[0].workspace;
    let path = temp.archive().join(content.path.as_str());
    let bytes = fs::read(&path).unwrap();
    let mut corrupt = bytes.clone();
    corrupt[0] ^= 1;
    fs::write(&path, corrupt).unwrap();
    assert_eq!(
        reader.offline_snapshot().unwrap_err().code,
        ErrorCode::ContentMismatch
    );
    fs::write(&path, bytes).unwrap();
    let path = temp.archive().join("inputs/prompt.txt");
    let mut bytes = fs::read(&path).unwrap();
    bytes[0] ^= 1;
    fs::write(path, bytes).unwrap();
    assert_eq!(
        ArchiveReader::open(temp.archive(), StorageLimits::default())
            .err()
            .unwrap()
            .code,
        ErrorCode::ContentMismatch
    );
}

#[test]
fn symlinks_hardlinks_special_files_and_parent_traversal_are_rejected() {
    for kind in ["symlink", "hardlink", "fifo", "directory", "ancestor"] {
        let temp = Temp::new();
        completed(&temp);
        let path = temp.archive().join("inputs/prompt.txt");
        let outside = temp.path.join("outside");
        fs::copy(&path, &outside).unwrap();
        if kind == "ancestor" {
            fs::rename(
                temp.archive().join("inputs"),
                temp.path.join("moved-inputs"),
            )
            .unwrap();
            symlink(
                temp.path.join("moved-inputs"),
                temp.archive().join("inputs"),
            )
            .unwrap();
        } else {
            fs::remove_file(&path).unwrap();
            match kind {
                "symlink" => symlink(&outside, &path).unwrap(),
                "hardlink" => fs::hard_link(&outside, &path).unwrap(),
                "fifo" => rustix::fs::mknodat(
                    rustix::fs::CWD,
                    &path,
                    rustix::fs::FileType::Fifo,
                    rustix::fs::Mode::from_raw_mode(0o600),
                    0,
                )
                .unwrap(),
                "directory" => fs::create_dir(&path).unwrap(),
                _ => unreachable!(),
            }
        }
        assert_eq!(
            ArchiveReader::open(temp.archive(), StorageLimits::default())
                .err()
                .unwrap()
                .code,
            ErrorCode::ContentMismatch,
            "{kind}"
        );
    }
    let temp = Temp::new();
    completed(&temp);
    symlink(temp.archive(), temp.path.join("linked-root")).unwrap();
    assert_eq!(
        ArchiveReader::open(temp.path.join("linked-root"), StorageLimits::default())
            .err()
            .unwrap()
            .code,
        ErrorCode::ContentMismatch
    );
    assert!(
        ArchiveReader::open(temp.archive().join("../archive"), StorageLimits::default()).is_err()
    );
    assert!(ArchivePath::new("inputs/../outside").is_err());
}

#[test]
fn preparation_and_content_limits_reject_before_allocating_an_archive() {
    let fixture = fixture::two_session().unwrap();
    let total = fixture.manifest.len() as u64
        + fixture
            .inputs
            .iter()
            .map(|input| input.bytes.len() as u64)
            .sum::<u64>();
    for limits in [
        StorageLimits {
            manifest_bytes: fixture.manifest.len() as u64 - 1,
            ..StorageLimits::default()
        },
        StorageLimits {
            verified_content_bytes: total - 1,
            ..StorageLimits::default()
        },
        StorageLimits {
            references: fixture.inputs.len() as u64,
            ..StorageLimits::default()
        },
    ] {
        let temp = Temp::new();
        let error = ArchiveWriter::create(
            temp.archive(),
            fixture.descriptor.clone(),
            fixture.manifest,
            &fixture.inputs,
            limits,
        )
        .err()
        .unwrap();
        assert_eq!(error.code, ErrorCode::LimitExceeded);
        assert!(!temp.archive().exists());
    }
    let temp = Temp::new();
    let limit = fixture.manifest.len() as u64;
    let (mut writer, _) = writer(
        &temp,
        StorageLimits {
            content_bytes: limit,
            manifest_bytes: limit,
            events: 3,
            ..StorageLimits::default()
        },
    );
    let large = vec![b'x'; limit as usize + 1];
    let reference = content_ref(ArchivePath::new("artifacts/oversized").unwrap(), &large);
    assert_eq!(
        writer
            .publish_artifact(&reference, &large)
            .unwrap_err()
            .code,
        ErrorCode::LimitExceeded
    );
    assert_eq!(
        writer
            .append(&fixture.events[..4], BatchClass::Ordinary)
            .unwrap_err()
            .code,
        ErrorCode::LimitExceeded
    );
    assert_eq!(writer.confirmed_frontier().frontier().committed_sequence, 0);
    let receipt = writer
        .append(&fixture.events[..3], BatchClass::Ordinary)
        .unwrap();
    let bounded = ArchiveReader::open(
        temp.archive(),
        StorageLimits {
            event_log_bytes: receipt.frontier().committed_bytes - 1,
            ..StorageLimits::default()
        },
    )
    .unwrap();
    assert_eq!(
        bounded.live_snapshot(&receipt).unwrap_err().code,
        ErrorCode::LimitExceeded
    );
}

#[test]
fn publication_cannot_follow_destination_or_staging_symlinks() {
    for staging in [false, true] {
        let temp = Temp::new();
        let (mut writer, fixture) = writer(&temp, StorageLimits::default());
        let outside = temp.path.join("outside");
        fs::create_dir(&outside).unwrap();
        let target = temp
            .archive()
            .join(if staging { ".staging" } else { "sessions" });
        if staging {
            fs::remove_dir(&target).unwrap();
        }
        symlink(&outside, target).unwrap();
        let artifact = &fixture.artifacts[0];
        let error = writer
            .publish_artifact(&artifact.reference, &artifact.bytes)
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::ContentMismatch);
        assert_eq!(fs::read_dir(outside).unwrap().count(), 0);
        assert_eq!(writer.confirmed_frontier().frontier().committed_sequence, 0);
    }
}

#[test]
fn cumulative_receipt_usage_counts_inputs_and_committed_objects_not_staged_bytes() {
    let temp = Temp::new();
    let (mut writer, fixture) = writer(&temp, StorageLimits::default());
    let before = writer.confirmed_frontier().clone();
    let inputs: u64 = fixture
        .inputs
        .iter()
        .map(|i| i.bytes.len() as u64)
        .sum::<u64>()
        + fixture.manifest.len() as u64;
    assert_eq!(before.usage().content_bytes, inputs);
    assert_eq!(before.usage().references, fixture.inputs.len() as u64 + 1);
    artifacts(&mut writer, &fixture);
    assert_eq!(writer.confirmed_frontier().usage(), before.usage());
    let receipt = writer
        .append(&fixture.events, BatchClass::Finalization)
        .unwrap();
    let retained: u64 = fixture.artifacts.iter().map(|i| i.bytes.len() as u64).sum();
    assert_eq!(receipt.usage().content_bytes, inputs + retained);
    assert_eq!(
        receipt.usage().references,
        (fixture.inputs.len() + fixture.artifacts.len()) as u64 + 1
    );
    assert_eq!(receipt.usage().events, fixture.events.len() as u64);
    assert_eq!(
        receipt.usage().log_bytes,
        fixture.events.iter().map(|l| l.len() as u64).sum::<u64>()
    );
    assert_eq!(
        before.usage().content_bytes,
        inputs,
        "old receipt remains immutable"
    );
}
