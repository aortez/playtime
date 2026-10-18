mod common;

use common::*;
use playtime_storage::{
    ArchiveReader, ArchiveWriter, BatchClass, ErrorCode, PublicationObserver, StorageLimits,
    WriteStep, WriterLock, fixture,
};
use std::{
    fs, io,
    path::PathBuf,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

struct FailAt {
    step: WriteStep,
    after: bool,
}
impl PublicationObserver for FailAt {
    fn before(&mut self, step: WriteStep) -> io::Result<()> {
        if !self.after && step == self.step {
            Err(io::Error::other("injected publication failure"))
        } else {
            Ok(())
        }
    }
    fn after(&mut self, step: WriteStep) -> io::Result<()> {
        if self.after && step == self.step {
            Err(io::Error::other("injected publication failure"))
        } else {
            Ok(())
        }
    }
}

#[test]
fn failed_event_commits_stop_the_writer_and_live_readers_keep_the_last_receipt() {
    for step in [
        WriteStep::EventsWrite,
        WriteStep::EventsSync,
        WriteStep::MarkerWrite,
        WriteStep::MarkerSync,
        WriteStep::MarkerRename,
        WriteStep::CommitDirectorySync,
        WriteStep::StagingDirectorySync,
    ] {
        for after in [false, true] {
            let temp = Temp::new();
            let (mut writer, fixture) = writer(&temp, StorageLimits::default());
            artifacts(&mut writer, &fixture);
            let confirmed = writer
                .append(&fixture.events[..8], BatchClass::Ordinary)
                .unwrap();
            let reader = reader(&temp);
            writer.set_observer(Box::new(FailAt { step, after }));
            let error = writer
                .append(&fixture.events[8..], BatchClass::Finalization)
                .unwrap_err();
            assert_eq!(error.code, ErrorCode::IoError, "{step:?} after={after}");
            assert!(error.outcome_uncertain);
            assert!(writer.is_stopped());
            assert_eq!(writer.confirmed_frontier().frontier(), confirmed.frontier());
            let before_retry = fs::read(temp.archive().join("events.jsonl")).unwrap();
            assert!(
                writer
                    .append(&fixture.events[8..], BatchClass::Finalization)
                    .unwrap_err()
                    .outcome_uncertain
            );
            assert_eq!(
                fs::read(temp.archive().join("events.jsonl")).unwrap(),
                before_retry
            );
            let live = reader.live_snapshot(&confirmed).unwrap();
            assert_eq!(live.frontier().committed_sequence, 8);
            assert!(live.inspection().incomplete_evidence);
            assert_eq!(
                reader.offline_snapshot().unwrap_err().code,
                ErrorCode::WriterBusy
            );
            drop(writer);
            let recovered = reader.offline_snapshot().unwrap();
            let renamed = matches!(
                step,
                WriteStep::CommitDirectorySync | WriteStep::StagingDirectorySync
            ) || (step == WriteStep::MarkerRename && after);
            assert_eq!(
                recovered.frontier().committed_sequence,
                if renamed { 16 } else { 8 },
                "{step:?} after={after}"
            );
            assert_eq!(recovered.inspection().incomplete_evidence, !renamed);
            assert_eq!(
                fs::read(temp.archive().join("events.jsonl")).unwrap(),
                before_retry
            );
        }
    }
}

#[test]
fn failed_artifact_publication_is_not_checkpoint_evidence() {
    for step in [
        WriteStep::ArtifactWrite,
        WriteStep::ArtifactSync,
        WriteStep::ArtifactRename,
        WriteStep::ArtifactDirectorySync,
        WriteStep::StagingDirectorySync,
        WriteStep::DirectorySync,
    ] {
        for after in [false, true] {
            let temp = Temp::new();
            let (mut writer, fixture) = writer(&temp, StorageLimits::default());
            writer
                .append(&fixture.events[..7], BatchClass::Ordinary)
                .unwrap();
            writer.set_observer(Box::new(FailAt { step, after }));
            let artifact = &fixture.artifacts[0];
            let error = writer
                .publish_artifact(&artifact.reference, &artifact.bytes)
                .unwrap_err();
            assert!(error.outcome_uncertain, "{step:?} after={after}");
            assert!(writer.is_stopped());
            drop(writer);
            let reader = reader(&temp);
            let snapshot = reader.offline_snapshot().unwrap();
            assert_eq!(snapshot.frontier().committed_sequence, 7);
            assert!(snapshot.inspection().checkpoints.is_empty());
            assert!(snapshot.inspection().incomplete_evidence);
            assert_eq!(
                reader
                    .read_content(&snapshot, &artifact.reference)
                    .unwrap_err()
                    .code,
                ErrorCode::ContentMismatch
            );
        }
    }
}

#[test]
fn failed_checkpoint_dependency_sync_retains_the_preceding_checkpoint() {
    for step in [WriteStep::ArtifactSync, WriteStep::ArtifactDirectorySync] {
        let temp = Temp::new();
        let (mut writer, fixture) = writer(&temp, StorageLimits::default());
        artifacts(&mut writer, &fixture);
        let previous = writer
            .append(&fixture.events[..12], BatchClass::Ordinary)
            .unwrap();
        writer.set_observer(Box::new(FailAt { step, after: false }));
        assert!(
            writer
                .append(&fixture.events[12..13], BatchClass::Ordinary)
                .unwrap_err()
                .outcome_uncertain
        );
        let reader = reader(&temp);
        let snapshot = reader.live_snapshot(&previous).unwrap();
        assert_eq!(snapshot.inspection().checkpoints.len(), 1);
        assert_eq!(
            snapshot.inspection().checkpoints[0].session_id.as_str(),
            "s1"
        );
        assert_eq!(snapshot.inspection().uncommitted_event_bytes, 0);
    }
}

#[test]
fn preparation_failures_preserve_partial_objects_without_a_prepared_archive() {
    for step in [
        WriteStep::ArtifactWrite,
        WriteStep::ArtifactSync,
        WriteStep::ArtifactRename,
        WriteStep::EventsSync,
        WriteStep::MarkerWrite,
        WriteStep::MarkerSync,
        WriteStep::MarkerRename,
        WriteStep::CommitDirectorySync,
    ] {
        let temp = Temp::new();
        let fixture = fixture::two_session().unwrap();
        let error = ArchiveWriter::create_with_observer(
            temp.archive(),
            fixture.descriptor,
            fixture.manifest,
            &fixture.inputs,
            StorageLimits::default(),
            Box::new(FailAt { step, after: true }),
        )
        .err()
        .unwrap();
        assert!(error.outcome_uncertain);
        assert!(temp.archive().exists());
        assert!(!temp.archive().join("archive.json").exists());
        assert!(ArchiveReader::open(temp.archive(), StorageLimits::default()).is_err());
        let _lock = WriterLock::try_acquire(temp.archive()).unwrap();
    }
}

struct ShortWrites {
    max: usize,
}
impl PublicationObserver for ShortWrites {
    fn write_limit(&mut self, _step: WriteStep, requested: usize) -> usize {
        requested.min(self.max)
    }
}

#[test]
fn short_writes_complete_all_bytes_and_zero_writes_stop_without_committing() {
    let temp = Temp::new();
    let fixture = fixture::two_session().unwrap();
    let mut writer = ArchiveWriter::create_with_observer(
        temp.archive(),
        fixture.descriptor.clone(),
        fixture.manifest,
        &fixture.inputs,
        StorageLimits::default(),
        Box::new(ShortWrites { max: 7 }),
    )
    .unwrap();
    all_events(&mut writer, &fixture);
    writer.regenerate_result().unwrap();
    drop(writer);
    assert!(
        !reader(&temp)
            .offline_snapshot()
            .unwrap()
            .inspection()
            .incomplete_evidence
    );
    assert_eq!(
        fs::read(temp.archive().join("events.jsonl")).unwrap(),
        fixture.events.concat()
    );
    let other = Temp::new();
    let (mut writer, fixture) = common::writer(&other, StorageLimits::default());
    writer.set_observer(Box::new(ShortWrites { max: 0 }));
    let error = writer
        .append(&fixture.events[..1], BatchClass::Ordinary)
        .unwrap_err();
    assert_eq!(error.io_kind, Some(io::ErrorKind::WriteZero));
    assert!(error.outcome_uncertain);
    drop(writer);
    assert_eq!(
        reader(&other)
            .offline_snapshot()
            .unwrap()
            .frontier()
            .committed_sequence,
        0
    );
}

#[test]
fn result_write_failure_does_not_revoke_a_committed_terminal_event() {
    let temp = Temp::new();
    let (mut writer, fixture) = writer(&temp, StorageLimits::default());
    all_events(&mut writer, &fixture);
    let receipt = writer.confirmed_frontier().clone();
    writer.set_observer(Box::new(FailAt {
        step: WriteStep::ResultWrite,
        after: false,
    }));
    assert!(writer.regenerate_result().is_err());
    let reader = reader(&temp);
    assert!(
        !reader
            .live_snapshot(&receipt)
            .unwrap()
            .inspection()
            .incomplete_evidence
    );
    drop(writer);
    assert!(
        !reader
            .offline_snapshot()
            .unwrap()
            .inspection()
            .incomplete_evidence
    );
}

struct ParkAt {
    step: WriteStep,
    after: bool,
    ready: PathBuf,
}
impl ParkAt {
    fn park(&self) -> ! {
        fs::write(&self.ready, b"ready").unwrap();
        loop {
            thread::park();
        }
    }
}
impl PublicationObserver for ParkAt {
    fn before(&mut self, step: WriteStep) -> io::Result<()> {
        if step == self.step && !self.after {
            self.park();
        }
        Ok(())
    }
    fn after(&mut self, step: WriteStep) -> io::Result<()> {
        if step == self.step && self.after {
            self.park();
        }
        Ok(())
    }
    fn write_limit(&mut self, step: WriteStep, requested: usize) -> usize {
        if step == WriteStep::EventsWrite && self.step == step {
            requested.min(17)
        } else {
            requested
        }
    }
}

// Invoked only in a disposable child process by the crash-boundary test below.
#[test]
#[ignore = "subprocess helper, invoked by process_termination_releases_lock_and_preserves_commit_boundaries"]
fn crash_child() {
    let path = PathBuf::from(std::env::var_os("PLAYTIME_STORAGE_CRASH_DIRECTORY").unwrap());
    let boundary = std::env::var("PLAYTIME_STORAGE_CRASH_BOUNDARY").unwrap();
    let fixture = fixture::two_session().unwrap();
    let mut writer = ArchiveWriter::create(
        &path,
        fixture.descriptor.clone(),
        fixture.manifest,
        &fixture.inputs,
        StorageLimits::default(),
    )
    .unwrap();
    artifacts(&mut writer, &fixture);
    writer
        .append(&fixture.events[..8], BatchClass::Ordinary)
        .unwrap();
    let (step, after) = match boundary.as_str() {
        "partial_event" => (WriteStep::EventsWrite, true),
        "before_marker" => (WriteStep::MarkerRename, false),
        "after_marker" => (WriteStep::MarkerRename, true),
        "after_directory_sync" => (WriteStep::CommitDirectorySync, true),
        _ => panic!("unknown crash boundary"),
    };
    writer.set_observer(Box::new(ParkAt {
        step,
        after,
        ready: path.parent().unwrap().join("ready"),
    }));
    writer
        .append(&fixture.events[8..], BatchClass::Finalization)
        .unwrap();
    panic!("child should have parked before returning a receipt");
}

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn process_termination_releases_lock_and_preserves_commit_boundaries() {
    for boundary in [
        "partial_event",
        "before_marker",
        "after_marker",
        "after_directory_sync",
    ] {
        let temp = Temp::new();
        let child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "crash_child", "--ignored", "--nocapture"])
            .env("PLAYTIME_STORAGE_CRASH_DIRECTORY", temp.archive())
            .env("PLAYTIME_STORAGE_CRASH_BOUNDARY", boundary)
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut child = ChildGuard(child);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !temp.path.join("ready").exists() {
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "child exited early at {boundary}"
            );
            assert!(Instant::now() < deadline, "child did not reach {boundary}");
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            WriterLock::try_acquire(temp.archive()).err().unwrap().code,
            ErrorCode::WriterBusy
        );
        assert_eq!(
            reader(&temp).offline_snapshot().unwrap_err().code,
            ErrorCode::WriterBusy
        );
        child.0.kill().unwrap();
        let status = child.0.wait().unwrap();
        assert!(!status.success());
        drop(WriterLock::try_acquire(temp.archive()).unwrap());
        let reader = reader(&temp);
        let snapshot = reader.offline_snapshot().unwrap();
        let committed = matches!(boundary, "after_marker" | "after_directory_sync");
        assert_eq!(
            snapshot.frontier().committed_sequence,
            if committed { 16 } else { 8 }
        );
        assert_eq!(snapshot.inspection().incomplete_evidence, !committed);
        if boundary == "partial_event" {
            assert_eq!(snapshot.inspection().uncommitted_event_bytes, 17);
        }
        assert_eq!(
            snapshot.inspection().checkpoints.len(),
            if committed { 2 } else { 1 }
        );
    }
}
