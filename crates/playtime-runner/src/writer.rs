use std::{
    sync::{
        Arc,
        mpsc::{self, Receiver, SyncSender, TryRecvError},
    },
    thread::{self, JoinHandle},
};

use playtime_storage::{ArchiveWriter, CommittedFrontier};

use crate::{Error, Result, mapping::Publication};

#[derive(Clone, Debug)]
pub struct CommitContext {
    pub first_sequence: u64,
    pub last_sequence: u64,
    pub kinds: Vec<String>,
    pub effect_ids: Vec<u64>,
    pub checkpoint_effect: Option<u64>,
    pub capture_effect: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReceiptDelivery {
    Deliver,
    Lose,
}

/// Fault/latency instrumentation around real storage calls. Hooks can withhold
/// a returned receipt, but cannot manufacture a successful storage operation.
pub trait WriterHooks: Send + 'static {
    fn before(&mut self, _context: &CommitContext, _writer: &mut ArchiveWriter) -> Result<()> {
        Ok(())
    }
    fn after(&mut self, _context: &CommitContext, _receipt: &CommittedFrontier) -> ReceiptDelivery {
        ReceiptDelivery::Deliver
    }
}
pub struct NoWriterHooks;
impl WriterHooks for NoWriterHooks {}

pub(crate) struct StorageThread {
    sender: Option<SyncSender<Arc<Publication>>>,
    receiver: Receiver<Result<CommittedFrontier>>,
    thread: Option<JoinHandle<()>>,
}
impl StorageThread {
    pub fn spawn(mut writer: ArchiveWriter, mut hooks: Box<dyn WriterHooks>) -> Self {
        let (sender, work) = mpsc::sync_channel::<Arc<Publication>>(1);
        let (replies, receiver) = mpsc::sync_channel(1);
        let thread = thread::spawn(move || {
            while let Ok(publication) = work.recv() {
                let context = CommitContext {
                    first_sequence: publication.events.first().unwrap().sequence,
                    last_sequence: publication.events.last().unwrap().sequence,
                    kinds: publication
                        .events
                        .iter()
                        .map(|event| event.kind.clone())
                        .collect(),
                    effect_ids: publication
                        .events
                        .iter()
                        .filter_map(|event| event.effect_id)
                        .collect(),
                    capture_effect: publication.capture.as_ref().map(|(effect, _)| *effect),
                    checkpoint_effect: publication
                        .checkpoint
                        .as_ref()
                        .map(|(effect, _, _)| *effect),
                };
                let result = (|| {
                    hooks.before(&context, &mut writer)?;
                    for event in &publication.events {
                        for object in &event.objects {
                            writer.publish_artifact(&object.reference, &object.bytes)?;
                        }
                    }
                    let lines: Vec<_> = publication
                        .events
                        .iter()
                        .map(|event| event.line.as_slice())
                        .collect();
                    let receipt = writer.append(&lines, publication.class)?;
                    if hooks.after(&context, &receipt) == ReceiptDelivery::Lose {
                        return Err(Error::Uncertain(
                            "writer committed but its receipt was lost".into(),
                        ));
                    }
                    Ok(receipt)
                })();
                let failed = result.is_err();
                if replies.send(result).is_err() || failed {
                    break;
                }
            }
            // All failed operations retire this owner, even a semantic refusal
            // before publication. There is no same-writer append retry here.
        });
        Self {
            sender: Some(sender),
            receiver,
            thread: Some(thread),
        }
    }
    pub fn begin(&self, publication: Arc<Publication>) -> Result<()> {
        self.sender
            .as_ref()
            .unwrap()
            .try_send(publication)
            .map_err(|_| Error::Uncertain("storage owner is unavailable or already busy".into()))
    }
    pub fn poll(&self) -> Option<Result<CommittedFrontier>> {
        match self.receiver.try_recv() {
            Ok(result) => Some(result),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err(Error::Uncertain(
                "storage owner exited without a receipt".into(),
            ))),
        }
    }
    pub fn close(mut self) {
        self.sender.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Returned to supervision when a serialized publication is still unresolved.
/// Dropping this handle does not cancel the filesystem operation or release its
/// writer lock. It only detaches observation; the storage thread retains both.
pub struct PendingPublication {
    pub(crate) worker: StorageThread,
    pub(crate) publication: Arc<Publication>,
    pub(crate) confirmed: crate::LatestReceipt,
}
impl PendingPublication {
    pub fn capture_effect(&self) -> Option<u64> {
        self.publication.capture.as_ref().map(|(id, _)| *id)
    }
    pub fn checkpoint_effect(&self) -> Option<u64> {
        self.publication.checkpoint.as_ref().map(|(id, _, _)| *id)
    }
    pub fn queued_bytes(&self) -> u64 {
        self.publication.queued_bytes()
    }
    pub fn poll(&self) -> Option<Result<CommittedFrontier>> {
        self.worker.poll().map(|result| {
            let receipt = result?;
            self.confirmed.publish(&receipt)?;
            Ok(receipt)
        })
    }
}
