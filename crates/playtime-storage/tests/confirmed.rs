mod common;
use common::*;
use playtime_protocol::storage::ArchiveIdentity;
use playtime_storage::{BatchClass, ConfirmationAuthority, ErrorCode, ReadPrefix, StorageLimits};
struct TestAuthority {
    identity: ArchiveIdentity,
    prefix: ReadPrefix,
}
impl ConfirmationAuthority for TestAuthority {
    fn authorize(&self, archive: &ArchiveIdentity) -> playtime_storage::Result<ReadPrefix> {
        assert_eq!(archive, &self.identity);
        Ok(self.prefix.clone())
    }
}
fn authority(receipt: &playtime_storage::CommittedFrontier) -> TestAuthority {
    let f = receipt.frontier();
    TestAuthority {
        identity: f.archive.clone(),
        prefix: ReadPrefix {
            sequence: f.committed_sequence,
            bytes: f.committed_bytes,
            sha256: f.prefix_sha256.clone(),
        },
    }
}
#[test]
fn trusted_prefix_ignores_newer_marker_and_historical_bytes_are_independently_verified() {
    let temp = Temp::new();
    let (mut writer, fixture) = writer(&temp, StorageLimits::default());
    artifacts(&mut writer, &fixture);
    writer
        .append(&fixture.events[..1], BatchClass::Ordinary)
        .unwrap();
    let first = authority(writer.confirmed_frontier());
    writer
        .append(&fixture.events[1..], BatchClass::Finalization)
        .unwrap();
    let last = authority(writer.confirmed_frontier());
    let reader = reader(&temp);
    assert_eq!(
        reader.offline_snapshot().unwrap_err().code,
        ErrorCode::WriterBusy
    );
    let old = reader.confirmed_snapshot(&first).unwrap();
    assert_eq!(old.frontier().committed_sequence, 1);
    assert!(old.inspection().terminal.is_none());
    let covering = reader.confirmed_snapshot(&last).unwrap();
    assert_eq!(
        reader
            .sub_snapshot(&covering, first.prefix.clone())
            .unwrap()
            .frontier(),
        old.frontier()
    );
    let mut forged = first.prefix.clone();
    forged.sha256 = last.prefix.sha256.clone();
    assert!(reader.sub_snapshot(&covering, forged).is_err());
    let mut forged = first.prefix.clone();
    forged.bytes += 1;
    assert!(reader.sub_snapshot(&covering, forged).is_err());
    assert_eq!(
        reader
            .sub_snapshot(&old, last.prefix.clone())
            .unwrap_err()
            .code,
        ErrorCode::CursorAhead
    );
    std::fs::write(
        temp.archive().join("events.commit.json"),
        b"invalid marker ignored by live reads",
    )
    .unwrap();
    assert_eq!(
        reader.confirmed_snapshot(&first).unwrap().frontier(),
        old.frontier()
    );
    std::fs::OpenOptions::new()
        .write(true)
        .open(temp.archive().join("events.jsonl"))
        .unwrap()
        .set_len(first.prefix.bytes - 1)
        .unwrap();
    assert!(reader.confirmed_snapshot(&first).is_err());
}
#[test]
fn remote_confirmation_revalidates_committed_content() {
    let temp = Temp::new();
    let (mut writer, fixture) = writer(&temp, StorageLimits::default());
    all_events(&mut writer, &fixture);
    let receipt = authority(writer.confirmed_frontier());
    let reader = reader(&temp);
    let snapshot = reader.confirmed_snapshot(&receipt).unwrap();
    let content = &snapshot.inspection().checkpoints[0].workspace;
    std::fs::write(temp.archive().join(content.path.as_str()), b"tampered").unwrap();
    assert_eq!(
        reader.confirmed_snapshot(&receipt).unwrap_err().code,
        ErrorCode::ContentMismatch
    );
}

#[test]
fn one_pass_extension_checks_verify_the_previous_prefix_as_an_exact_boundary() {
    let temp = Temp::new();
    let (mut writer, fixture) = writer(&temp, StorageLimits::default());
    artifacts(&mut writer, &fixture);
    writer
        .append(&fixture.events[..1], BatchClass::Ordinary)
        .unwrap();
    let first = authority(writer.confirmed_frontier());
    writer
        .append(&fixture.events[1..], BatchClass::Finalization)
        .unwrap();
    let last = authority(writer.confirmed_frontier());
    let reader = reader(&temp);
    let old = reader.confirmed_snapshot(&first).unwrap();
    let covering = reader.confirmed_snapshot(&last).unwrap();

    // Extending the first confirmation, or the same one, is one verified pass
    // with the same result as a plain confirmed read.
    for previous in [first.prefix.clone(), last.prefix.clone()] {
        let extended = reader
            .confirmed_snapshot_extending(&last, previous)
            .unwrap();
        assert_eq!(extended.frontier(), covering.frontier());
        assert_eq!(extended.inspection(), covering.inspection());
    }
    let empty = ReadPrefix {
        sequence: 0,
        bytes: 0,
        sha256: playtime_storage::sha256(b""),
    };
    assert_eq!(
        reader
            .confirmed_snapshot_extending(&last, empty)
            .unwrap()
            .frontier(),
        covering.frontier()
    );
    // A sub-snapshot from that pass equals a pass that stops at the position.
    let sub = reader
        .sub_snapshot(&covering, first.prefix.clone())
        .unwrap();
    assert_eq!(sub.frontier(), old.frontier());
    assert_eq!(sub.inspection(), old.inspection());

    // A position inside a record, with another history or with the wrong
    // sequence is refused, and one beyond the confirmation is ahead.
    let mut inside = first.prefix.clone();
    inside.bytes -= 1;
    let mut forged = first.prefix.clone();
    forged.sha256 = last.prefix.sha256.clone();
    let mut wrong = first.prefix.clone();
    wrong.sequence += 1;
    for previous in [inside, forged, wrong] {
        assert_eq!(
            reader
                .confirmed_snapshot_extending(&last, previous.clone())
                .unwrap_err()
                .code,
            ErrorCode::InvalidCursor
        );
        assert_eq!(
            reader.sub_snapshot(&covering, previous).unwrap_err().code,
            ErrorCode::InvalidCursor
        );
    }
    assert_eq!(
        reader
            .confirmed_snapshot_extending(&first, last.prefix.clone())
            .unwrap_err()
            .code,
        ErrorCode::CursorAhead
    );

    // A prefix that fails its own validation is corruption, not a bad
    // position, even when the difference first shows at the position.
    let path = temp.archive().join("events.jsonl");
    let mut bytes = std::fs::read(&path).unwrap();
    let second = bytes
        .windows(9)
        .position(|window| window == b"12:00:01Z")
        .unwrap();
    assert!((second as u64) < first.prefix.bytes);
    bytes[second + 7] = b'0';
    std::fs::write(&path, &bytes).unwrap();
    assert_eq!(
        reader
            .confirmed_snapshot_extending(&last, first.prefix.clone())
            .unwrap_err()
            .code,
        ErrorCode::ArchiveCorrupt
    );
    assert_eq!(
        reader
            .sub_snapshot(&covering, first.prefix)
            .unwrap_err()
            .code,
        ErrorCode::ArchiveCorrupt
    );
}
