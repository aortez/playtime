//! Synthetic adapter/publication fixture; never Docker or experimental execution.
use super::*;
use crate::writer::StorageThread;
use crate::{
    ArchiveQueries, CommitContext, LatestReceipt, NoWriterHooks, PendingPublication, WriterHooks,
};
use playtime_core::{SessionId, UtcTime};
use playtime_docker::{CancellationToken, capture::*};
use playtime_storage::{ArchiveWriter, CommittedFrontier, PublicationObserver, WriteStep};
use std::{
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};
#[path = "../../playtime-docker/tests/support/mod.rs"]
mod docker_fixture;
struct Temp {
    path: std::path::PathBuf,
}
impl Temp {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "playtime-capture-publication-{}",
            playtime_docker::Ownership::random_nonce().unwrap()
        ));
        std::fs::create_dir(&path).unwrap();
        Self { path }
    }
    fn archive(&self) -> std::path::PathBuf {
        self.path.join("archive")
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn fixture() -> (Temp, Encoder, ArchiveWriter, UtcTime) {
    let temp = Temp::new();
    let f = playtime_storage::fixture::two_session().unwrap();
    let run_id = f.descriptor.run_id.clone();
    let manifest = f.descriptor.input_manifest.clone();
    let writer = ArchiveWriter::create(
        temp.archive(),
        f.descriptor,
        f.manifest,
        &f.inputs,
        StorageLimits::default(),
    )
    .unwrap();
    let mut encoder = Encoder::new(run_id, StorageLimits::default(), Default::default());
    let time = UtcTime::new("2026-10-18T00:00:00Z").unwrap();
    encoder.start(&time, manifest).unwrap();
    (temp, encoder, writer, time)
}
fn wait(worker: &StorageThread) -> crate::Result<CommittedFrontier> {
    let end = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(result) = worker.poll() {
            return result;
        }
        assert!(Instant::now() < end);
        std::thread::sleep(Duration::from_millis(1));
    }
}
fn capture_publication(
    encoder: &mut Encoder,
    time: &UtcTime,
    ordinal: u32,
    through: u64,
) -> Publication {
    let profile = CaptureProfile::prepare(
        CaptureLimits::default(),
        8 << 20,
        &StorageLimits::default(),
        ArchiveUsage::default(),
    )
    .unwrap();
    let candidate = tokio::runtime::Runtime::new().unwrap().block_on(async {
        let mut f = docker_fixture::Fixture::running().await;
        let candidate = {
            let mut stopped = f
                .sandbox
                .quiesce(docker_fixture::deadline(), CancellationToken::new())
                .await
                .unwrap();
            stopped
                .capture(&profile, CancellationToken::new())
                .await
                .unwrap()
        };
        assert!(f.cleanup().await.complete());
        candidate
    });
    let captured = candidate.workspace();
    let id = SessionId::new(format!("capture-{ordinal}")).unwrap();
    let snapshot = playtime_engine::Snapshot {
        workspace: reference(&format!("sessions/{id}/workspace.tar"), captured.tar()),
        files: captured
            .entries()
            .iter()
            .map(|e| {
                (
                    e.path.clone(),
                    playtime_engine::FileInfo {
                        size_bytes: e.size_bytes,
                        mode: e.mode,
                        entry_type: e.entry_type.clone(),
                        sha256: e
                            .sha256
                            .as_ref()
                            .map(|s| playtime_core::Sha256Digest::new(s).unwrap()),
                        link: e.link.clone(),
                    },
                )
            })
            .collect(),
        memory: Memory {
            status: "missing".into(),
            content: String::new(),
        },
    };
    let checkpoint = Checkpoint {
        session_id: id,
        ordinal,
        snapshot,
        evidence: None,
        handoff_saved: true,
        interrupted: false,
    };
    let bodies = captured
        .entries()
        .iter()
        .filter_map(|e| {
            e.content_slot
                .map(|slot| (e.path.as_str(), captured.content(slot).unwrap()))
        })
        .collect();
    encoder
        .checkpoint_contents(
            time,
            100 + ordinal as u64,
            &checkpoint,
            captured.tar(),
            &bodies,
            through,
        )
        .unwrap()
}
struct Fail {
    step: WriteStep,
    after: bool,
}
impl PublicationObserver for Fail {
    fn before(&mut self, step: WriteStep) -> std::io::Result<()> {
        if step == self.step && !self.after {
            Err(std::io::Error::other("injected"))
        } else {
            Ok(())
        }
    }
    fn after(&mut self, step: WriteStep) -> std::io::Result<()> {
        if step == self.step && self.after {
            Err(std::io::Error::other("injected"))
        } else {
            Ok(())
        }
    }
}
struct Inject {
    step: WriteStep,
    after: bool,
}
impl WriterHooks for Inject {
    fn before(&mut self, context: &CommitContext, writer: &mut ArchiveWriter) -> crate::Result<()> {
        if context.checkpoint_effect == Some(102) {
            writer.set_observer(Box::new(Fail {
                step: self.step,
                after: self.after,
            }));
        }
        Ok(())
    }
}
#[test]
fn captured_bytes_publish_only_with_receipts_and_failed_publication_keeps_prior_checkpoint() {
    for (step, after) in [
        (WriteStep::ArtifactSync, false),
        (WriteStep::MarkerRename, false),
        (WriteStep::MarkerRename, true),
    ] {
        let (temp, mut encoder, writer, time) = fixture();
        let worker = StorageThread::spawn(writer, Box::new(Inject { step, after }));
        let start = Arc::new(encoder.batch().unwrap());
        worker.begin(start.clone()).unwrap();
        let mut confirmed = wait(&worker).unwrap();
        encoder.confirmed(&start);
        let first = Arc::new(capture_publication(
            &mut encoder,
            &time,
            1,
            confirmed.frontier().committed_sequence,
        ));
        worker.begin(first.clone()).unwrap();
        confirmed = wait(&worker).unwrap();
        encoder.confirmed(&first);
        let queries =
            ArchiveQueries::live(temp.archive(), StorageLimits::default(), &confirmed).unwrap();
        let checkpoints = queries.checkpoints().unwrap();
        assert_eq!(checkpoints.len(), 1);
        let index = checkpoints[0].index.as_ref().unwrap();
        assert_eq!(index.files.len(), 6);
        assert_eq!(index.files[0].mode, 0o640);
        assert!(matches!(
            index.files[1].content,
            FileContent::Unavailable { .. }
        ));
        assert_eq!(index.outgoing_memory.status, "missing");
        assert_eq!(index.incoming_memory.status, "not_delivered");
        let second = Arc::new(capture_publication(
            &mut encoder,
            &time,
            2,
            confirmed.frontier().committed_sequence,
        ));
        // A completed handoff and valid candidate are not evidence of publication.
        assert_eq!(
            ArchiveQueries::live(temp.archive(), StorageLimits::default(), &confirmed)
                .unwrap()
                .checkpoints()
                .unwrap()
                .len(),
            1
        );
        worker.begin(second.clone()).unwrap();
        assert!(wait(&worker).is_err());
        assert_eq!(
            ArchiveQueries::live(temp.archive(), StorageLimits::default(), &confirmed)
                .unwrap()
                .checkpoints()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            encoder
                .last_checkpoint
                .as_ref()
                .unwrap()
                .session_id
                .as_str(),
            "capture-1"
        );
        worker.close(); // failed owner retires; no stopped-writer retry
        let offline = ArchiveQueries::offline(temp.archive(), StorageLimits::default()).unwrap();
        assert_eq!(
            offline.checkpoints().unwrap().len(),
            if after { 2 } else { 1 }
        );
    }
}
struct HoldReceipt {
    arrived: mpsc::Sender<()>,
    release: mpsc::Receiver<()>,
}
impl WriterHooks for HoldReceipt {
    fn after(&mut self, context: &CommitContext, _: &CommittedFrontier) -> crate::ReceiptDelivery {
        if context.checkpoint_effect.is_some() {
            self.arrived.send(()).unwrap();
            self.release.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        crate::ReceiptDelivery::Deliver
    }
}
#[test]
fn late_capture_receipt_keeps_writer_owned_and_advances_only_when_delivered() {
    let (temp, mut encoder, writer, time) = fixture();
    let (arrived, notice) = mpsc::channel();
    let (release, hold) = mpsc::channel();
    let worker = StorageThread::spawn(
        writer,
        Box::new(HoldReceipt {
            arrived,
            release: hold,
        }),
    );
    worker.begin(Arc::new(encoder.batch().unwrap())).unwrap();
    let prior = wait(&worker).unwrap();
    let publication = Arc::new(capture_publication(
        &mut encoder,
        &time,
        1,
        prior.frontier().committed_sequence,
    ));
    let latest = LatestReceipt::default();
    latest.publish(&prior).unwrap();
    worker.begin(publication.clone()).unwrap();
    notice.recv_timeout(Duration::from_secs(5)).unwrap();
    let pending = PendingPublication {
        worker,
        publication,
        confirmed: latest.clone(),
    };
    assert!(pending.poll().is_none());
    assert!(
        ArchiveQueries::live(temp.archive(), StorageLimits::default(), &prior)
            .unwrap()
            .checkpoints()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        ArchiveWriter::open_prepared(
            temp.archive(),
            &prior.frontier().archive,
            StorageLimits::default()
        )
        .err()
        .unwrap()
        .code,
        playtime_storage::ErrorCode::WriterBusy
    );
    release.send(()).unwrap();
    let end = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(receipt) = pending.poll() {
            receipt.unwrap();
            break;
        }
        assert!(Instant::now() < end);
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(
        ArchiveQueries::live(
            temp.archive(),
            StorageLimits::default(),
            &latest.latest().unwrap()
        )
        .unwrap()
        .checkpoints()
        .unwrap()
        .len(),
        1
    );
    pending.worker.close();
}
#[test]
fn actual_candidate_can_convert_without_publishing_or_fabricating_a_receipt() {
    // A synthetic transport supplies reviewed bytes through actual capture;
    // no Docker daemon or experimental worker is involved in this fixture.
    let (_temp, mut encoder, writer, time) = fixture();
    let worker = StorageThread::spawn(writer, Box::new(NoWriterHooks));
    worker.begin(Arc::new(encoder.batch().unwrap())).unwrap();
    let receipt = wait(&worker).unwrap();
    let publication = capture_publication(
        &mut encoder,
        &time,
        1,
        receipt.frontier().committed_sequence,
    );
    let objects = &publication.events[0].objects;
    let tar = objects
        .iter()
        .find(|o| o.reference.path.as_str().ends_with("workspace.tar"))
        .unwrap();
    assert_eq!(
        tar.reference.sha256.as_str(),
        "e465b5958f9892de45a47cfe25802ce2330d1753fc8eb116a4e77dd1ae3b092e"
    );
    assert_eq!(
        objects
            .iter()
            .find(|o| o.reference.path.as_str().ends_with("files/0"))
            .unwrap()
            .bytes,
        b"alpha\n"
    );
    assert!(encoder.last_checkpoint.is_none());
    worker.close();
}
