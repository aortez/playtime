mod common;

use common::*;
use playtime_storage::{ArchiveWriter, BatchClass, ErrorCode, StorageLimits};
use std::{fs, process::Command};

#[test]
fn prepared_handoff_verifies_identity_empty_frontier_inputs_and_exclusive_ownership() {
    let temp = Temp::new();
    let (writer, fixture) = writer(&temp, StorageLimits::default());
    let identity = writer.confirmed_frontier().frontier().archive.clone();
    assert_eq!(
        ArchiveWriter::open_prepared(temp.archive(), &identity, StorageLimits::default())
            .err()
            .unwrap()
            .code,
        ErrorCode::WriterBusy
    );
    drop(writer);
    let mut wrong = identity.clone();
    wrong.run_id = playtime_core::RunId::new("different").unwrap();
    assert_eq!(
        ArchiveWriter::open_prepared(temp.archive(), &wrong, StorageLimits::default())
            .err()
            .unwrap()
            .code,
        ErrorCode::ContentMismatch
    );
    let mut opened =
        ArchiveWriter::open_prepared(temp.archive(), &identity, StorageLimits::default()).unwrap();
    assert_eq!(opened.confirmed_frontier().frontier().committed_sequence, 0);
    opened
        .append(&fixture.events[..1], BatchClass::Ordinary)
        .unwrap();
    drop(opened);
    assert_eq!(
        ArchiveWriter::open_prepared(temp.archive(), &identity, StorageLimits::default())
            .err()
            .unwrap()
            .code,
        ErrorCode::MalformedData
    );
}

#[test]
fn prepared_handoff_rejects_even_an_uncommitted_tail_or_missing_frozen_input() {
    for damage in ["tail", "input", "marker"] {
        let temp = Temp::new();
        let (writer, fixture) = writer(&temp, StorageLimits::default());
        let identity = writer.confirmed_frontier().frontier().archive.clone();
        drop(writer);
        match damage {
            "tail" => fs::write(temp.archive().join("events.jsonl"), b"partial").unwrap(),
            "input" => fs::write(
                temp.archive()
                    .join(fixture.inputs[0].reference.path.as_str()),
                b"damaged",
            )
            .unwrap(),
            _ => fs::write(temp.archive().join("events.commit.json"), b"{}").unwrap(),
        }
        assert!(
            ArchiveWriter::open_prepared(temp.archive(), &identity, StorageLimits::default())
                .is_err(),
            "{damage}"
        );
    }
}

#[test]
#[ignore = "subprocess helper invoked by prepared_handoff_crosses_a_process_boundary"]
fn prepared_child() {
    let path = std::env::var_os("PLAYTIME_PREPARED_TEST_DIRECTORY").unwrap();
    let identity =
        serde_json::from_str(&std::env::var("PLAYTIME_PREPARED_TEST_IDENTITY").unwrap()).unwrap();
    let mut writer =
        ArchiveWriter::open_prepared(path, &identity, StorageLimits::default()).unwrap();
    let fixture = playtime_storage::fixture::two_session().unwrap();
    writer
        .append(&fixture.events[..1], BatchClass::Ordinary)
        .unwrap();
}

#[test]
fn prepared_handoff_crosses_a_process_boundary() {
    let temp = Temp::new();
    let (writer, _) = writer(&temp, StorageLimits::default());
    let identity = writer.confirmed_frontier().frontier().archive.clone();
    drop(writer);
    let child = Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "prepared_child"])
        .env("PLAYTIME_PREPARED_TEST_DIRECTORY", temp.archive())
        .env(
            "PLAYTIME_PREPARED_TEST_IDENTITY",
            serde_json::to_string(&identity).unwrap(),
        )
        .output()
        .unwrap();
    assert!(
        child.status.success(),
        "{}",
        String::from_utf8_lossy(&child.stderr)
    );
    assert_eq!(
        reader(&temp)
            .offline_snapshot()
            .unwrap()
            .frontier()
            .committed_sequence,
        1
    );
    assert!(
        ArchiveWriter::open_prepared(temp.archive(), &identity, StorageLimits::default()).is_err()
    );
}
