mod common;
use common::*;
use playtime_protocol::storage::ArchiveIdentity;
use playtime_storage::{
    BatchClass, ConfirmationAuthority, ErrorCode, ReadPrefix, StorageLimits, fixture,
};
use std::io::Write;
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

/// Appends each fixture record as its own batch, as a live run confirms, and
/// returns every receipt from the empty prefix on.
fn one_record_receipts(temp: &Temp) -> Vec<TestAuthority> {
    let (mut writer, fixture) = writer(temp, StorageLimits::default());
    artifacts(&mut writer, &fixture);
    let mut receipts = vec![authority(writer.confirmed_frontier())];
    for (index, line) in fixture.events.iter().enumerate() {
        let class = if index + 1 == fixture.events.len() {
            BatchClass::Finalization
        } else {
            BatchClass::Ordinary
        };
        writer.append(&[line], class).unwrap();
        receipts.push(authority(writer.confirmed_frontier()));
    }
    receipts
}

/// Changes one byte of a file and returns what puts it back.
fn tamper(path: std::path::PathBuf, at: impl FnOnce(&[u8]) -> usize) -> impl FnOnce() {
    let original = std::fs::read(&path).unwrap();
    let mut bytes = original.clone();
    let index = at(&bytes);
    bytes[index] ^= 1;
    std::fs::write(&path, bytes).unwrap();
    move || std::fs::write(&path, original).unwrap()
}

/// The last digit of a fixture record's unique time, which a flip keeps a
/// valid record of the same length.
fn time(second: &'static str) -> impl FnOnce(&[u8]) -> usize {
    move |bytes| {
        let text = format!("12:00:{second}Z");
        bytes
            .windows(text.len())
            .position(|window| window == text.as_bytes())
            .unwrap()
            + text.len()
            - 2
    }
}

#[test]
fn a_continued_confirmation_equals_a_full_pass_from_every_boundary() {
    let temp = Temp::new();
    let receipts = one_record_receipts(&temp);
    let reader = reader(&temp);
    let full: Vec<_> = receipts
        .iter()
        .map(|receipt| reader.confirmed_snapshot(receipt).unwrap())
        .collect();
    for (start, previous) in full.iter().enumerate() {
        for (receipt, expected) in receipts.iter().zip(&full).skip(start) {
            let continued = reader
                .confirmed_snapshot_continuing(receipt, previous)
                .unwrap();
            assert_eq!(continued.frontier(), expected.frontier());
            assert_eq!(continued.inspection(), expected.inspection());
        }
    }
    // Continued one record at a time, every page still returns the exact
    // committed records.
    let fixture = fixture::two_session().unwrap();
    let chained = receipts[1..]
        .iter()
        .fold(full[0].clone(), |previous, receipt| {
            reader
                .confirmed_snapshot_continuing(receipt, &previous)
                .unwrap()
        });
    let last = full.last().unwrap();
    for after in 0..=fixture.events.len() {
        for limit in [1, 3, 1000] {
            let page = reader
                .page(&chained, &chained.query(after as u64, limit))
                .unwrap();
            let end = fixture.events.len().min(after + limit as usize);
            assert_eq!(
                page.events
                    .iter()
                    .map(|event| event.bytes.as_slice())
                    .collect::<Vec<_>>(),
                fixture.events[after..end]
            );
            let same = reader.page(last, &last.query(after as u64, limit)).unwrap();
            assert_eq!(page.events, same.events);
            assert_eq!(
                (page.next_after_sequence, page.has_more),
                (end as u64, end < fixture.events.len())
            );
        }
    }
}

#[test]
fn a_continued_confirmation_reads_only_new_bytes_and_reads_check_what_they_return() {
    let temp = Temp::new();
    let (mut writer, fixture) = writer(&temp, StorageLimits::default());
    artifacts(&mut writer, &fixture);
    writer
        .append(&fixture.events[..8], BatchClass::Ordinary)
        .unwrap();
    let first = authority(writer.confirmed_frontier());
    writer
        .append(&fixture.events[8..], BatchClass::Finalization)
        .unwrap();
    let last = authority(writer.confirmed_frontier());
    let reader = reader(&temp);
    let old = reader.confirmed_snapshot(&first).unwrap();
    let expected = reader.confirmed_snapshot(&last).unwrap();
    let continued = || reader.confirmed_snapshot_continuing(&last, &old);
    let file = |path: &str| temp.archive().join(path);

    // Content that an earlier record references isn't verified again; a full
    // pass and a read of that content still verify it.
    let earlier = old.inspection().checkpoints[0].workspace.clone();
    let restore = tamper(file(earlier.path.as_str()), |_| 0);
    let snapshot = continued().unwrap();
    assert_eq!(snapshot.frontier(), expected.frontier());
    assert_eq!(snapshot.inspection(), expected.inspection());
    assert_eq!(
        reader.confirmed_snapshot(&last).unwrap_err().code,
        ErrorCode::ContentMismatch
    );
    assert_eq!(
        reader.read_content(&snapshot, &earlier).unwrap_err().code,
        ErrorCode::ContentMismatch
    );
    restore();
    // Content that a new record references is verified.
    let newer = expected.inspection().checkpoints[1].workspace.clone();
    let restore = tamper(file(newer.path.as_str()), |_| 0);
    assert_eq!(continued().unwrap_err().code, ErrorCode::ContentMismatch);
    restore();

    // An earlier record isn't read again. A page that returns it, a
    // sub-snapshot that ends after it and a full pass check it.
    let restore = tamper(file("events.jsonl"), time("02"));
    let snapshot = continued().unwrap();
    assert_eq!(snapshot.frontier(), expected.frontier());
    assert_eq!(
        reader
            .page(&snapshot, &snapshot.query(0, 1000))
            .unwrap_err()
            .code,
        ErrorCode::ArchiveCorrupt
    );
    assert_eq!(
        reader
            .page(&snapshot, &snapshot.query(2, 1000))
            .unwrap()
            .events
            .len(),
        fixture.events.len() - 2
    );
    assert_eq!(
        reader
            .sub_snapshot(&snapshot, first.prefix.clone())
            .unwrap_err()
            .code,
        ErrorCode::ArchiveCorrupt
    );
    assert_eq!(
        reader.confirmed_snapshot(&last).unwrap_err().code,
        ErrorCode::ArchiveCorrupt
    );
    restore();
    // A new record is validated as a full pass validates it.
    let restore = tamper(file("events.jsonl"), time("10"));
    assert_eq!(continued().unwrap_err().code, ErrorCode::ArchiveCorrupt);
    restore();
    assert_eq!(continued().unwrap().frontier(), expected.frontier());
}

#[test]
fn a_continued_confirmation_starts_at_or_before_its_receipt() {
    let temp = Temp::new();
    let (mut writer, fixture) = writer(&temp, StorageLimits::default());
    artifacts(&mut writer, &fixture);
    writer
        .append(&fixture.events[..8], BatchClass::Ordinary)
        .unwrap();
    let first = authority(writer.confirmed_frontier());
    writer
        .append(&fixture.events[8..15], BatchClass::Ordinary)
        .unwrap();
    let last = authority(writer.confirmed_frontier());
    drop(writer);
    let reader = reader(&temp);
    let newer = reader.confirmed_snapshot(&last).unwrap();
    assert_eq!(
        reader
            .confirmed_snapshot_continuing(&first, &newer)
            .unwrap_err()
            .code,
        ErrorCode::CursorAhead
    );
    // An equal receipt reads no record but sees bytes past the commit.
    let tail = b"{\"partial";
    std::fs::OpenOptions::new()
        .append(true)
        .open(temp.archive().join("events.jsonl"))
        .unwrap()
        .write_all(tail)
        .unwrap();
    let equal = reader.confirmed_snapshot_continuing(&last, &newer).unwrap();
    assert_eq!(equal.frontier(), newer.frontier());
    assert_eq!(newer.inspection().uncommitted_event_bytes, 0);
    assert_eq!(
        equal.inspection().uncommitted_event_bytes,
        tail.len() as u64
    );
    // A receipt whose digest the log doesn't match fails as a full pass does.
    let mut forged = authority_for(&last, last.prefix.clone());
    forged.prefix.sha256 = first.prefix.sha256.clone();
    assert_eq!(
        reader
            .confirmed_snapshot_continuing(&forged, &newer)
            .unwrap_err()
            .code,
        ErrorCode::ArchiveCorrupt
    );
}

#[test]
fn a_continued_confirmation_keeps_the_rules_across_records_and_its_reader() {
    let temp = Temp::new();
    let (mut writer, fixture) = writer(&temp, StorageLimits::default());
    all_events(&mut writer, &fixture);
    let done = authority(writer.confirmed_frontier());
    drop(writer);
    let reader = common::reader(&temp);
    let finished = reader.confirmed_snapshot(&done).unwrap();
    // Only the reader whose pass validated a snapshot continues it.
    assert_eq!(
        common::reader(&temp)
            .confirmed_snapshot_continuing(&done, &finished)
            .unwrap_err()
            .code,
        ErrorCode::InvalidCursor
    );
    // A copy kept for reads pages and checks positions as the snapshot does,
    // but nothing continues it.
    let reads = finished.without_continuation();
    assert_eq!(
        reader.page(&reads, &reads.query(3, 1000)).unwrap().events,
        reader
            .page(&finished, &finished.query(3, 1000))
            .unwrap()
            .events
    );
    reads.check_position(&done.prefix).unwrap();
    assert_eq!(
        reader
            .confirmed_snapshot_continuing(&done, &reads)
            .unwrap_err()
            .code,
        ErrorCode::InvalidCursor
    );
    // An earlier record renumbered to follow the run's end decodes, so only
    // state the earlier pass carried, that the run has ended, refuses it. A
    // continued pass refuses it as a full pass does.
    let path = temp.archive().join("events.jsonl");
    let after = changed(fixture.events[1], |value| value["sequence"] = 17.into());
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(&after)
        .unwrap();
    let log = std::fs::read(&path).unwrap();
    let longer = authority_for(
        &done,
        ReadPrefix {
            sequence: 17,
            bytes: log.len() as u64,
            sha256: playtime_storage::sha256(&log),
        },
    );
    let full = reader.confirmed_snapshot(&longer).unwrap_err();
    assert_eq!(full.code, ErrorCode::ArchiveCorrupt);
    assert!(
        full.message.contains("after run_finished"),
        "{}",
        full.message
    );
    let continued = reader
        .confirmed_snapshot_continuing(&longer, &finished)
        .unwrap_err();
    assert_eq!(
        (continued.code, continued.message),
        (full.code, full.message)
    );
}

fn authority_for(receipt: &TestAuthority, prefix: ReadPrefix) -> TestAuthority {
    TestAuthority {
        identity: receipt.identity.clone(),
        prefix,
    }
}

#[test]
fn a_position_is_checked_against_the_records_its_pass_validated() {
    let temp = Temp::new();
    let receipts = one_record_receipts(&temp);
    let reader = reader(&temp);
    let covering = reader.confirmed_snapshot(receipts.last().unwrap()).unwrap();
    for receipt in &receipts {
        covering.check_position(&receipt.prefix).unwrap();
    }
    let middle = &receipts[8].prefix;
    let mut inside = middle.clone();
    inside.bytes -= 1;
    let mut forged = middle.clone();
    forged.sha256 = receipts[9].prefix.sha256.clone();
    let mut wrong = middle.clone();
    wrong.sequence += 1;
    for position in [inside, forged, wrong] {
        assert_eq!(
            covering.check_position(&position).unwrap_err().code,
            ErrorCode::InvalidCursor
        );
    }
    let older = reader.confirmed_snapshot(&receipts[8]).unwrap();
    assert_eq!(
        older.check_position(&receipts[9].prefix).unwrap_err().code,
        ErrorCode::CursorAhead
    );
    // It reads nothing, so a changed log doesn't change its answer.
    std::fs::write(temp.archive().join("events.jsonl"), b"").unwrap();
    covering.check_position(middle).unwrap();
}
