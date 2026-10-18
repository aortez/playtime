use crate::{Failure, FailureKind, Limits, Result};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::sync::mpsc;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ObservationKind {
    NativeChunk,
    HttpError { status: u16, truncated: bool },
    ProtocolFragment,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Observation {
    pub kind: ObservationKind,
    pub bytes: Vec<u8>,
}
#[derive(Clone)]
pub struct ObservationSender {
    sender: mpsc::Sender<Observation>,
    queued: Arc<AtomicUsize>,
    limit: usize,
}
pub struct ObservationReceiver {
    receiver: mpsc::Receiver<Observation>,
    queued: Arc<AtomicUsize>,
}
pub fn observations(limits: &Limits) -> Result<(ObservationSender, ObservationReceiver)> {
    limits.validate()?;
    let (sender, receiver) = mpsc::channel(limits.queue_records);
    let queued = Arc::new(AtomicUsize::new(0));
    Ok((
        ObservationSender {
            sender,
            queued: queued.clone(),
            limit: limits.queue_bytes,
        },
        ObservationReceiver { receiver, queued },
    ))
}
impl ObservationSender {
    pub(crate) fn record(&self, observation: Observation) -> Result<()> {
        let size = observation.bytes.len();
        let error = || {
            Failure::new(
                FailureKind::ByteLimit,
                "bounded observation queue is full or closed",
            )
        };
        self.queued
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                n.checked_add(size).filter(|sum| *sum <= self.limit)
            })
            .map_err(|_| error())?;
        if self.sender.try_send(observation).is_err() {
            self.queued.fetch_sub(size, Ordering::AcqRel);
            return Err(error());
        }
        Ok(())
    }
}
impl ObservationReceiver {
    pub async fn recv(&mut self) -> Option<Observation> {
        let observation = self.receiver.recv().await?;
        self.queued
            .fetch_sub(observation.bytes.len(), Ordering::AcqRel);
        Some(observation)
    }
    pub fn try_recv(&mut self) -> Option<Observation> {
        let observation = self.receiver.try_recv().ok()?;
        self.queued
            .fetch_sub(observation.bytes.len(), Ordering::AcqRel);
        Some(observation)
    }
}
