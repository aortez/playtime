//! Bounded shared operations. Observers own receivers, not the underlying work.

use super::{Admission, Error, Phase, Reason, Result};
use playtime_core::RunId;
use std::{
    collections::HashMap,
    future::Future,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    sync::{Semaphore, watch},
    time::Instant,
};

const RUNS: usize = 16;
pub(crate) const REFRESH_FLOOR: Duration = Duration::from_millis(250);
const BACKOFF_MAX: Duration = Duration::from_secs(5);
type Completion = Option<std::result::Result<Admission, Arc<Error>>>;

struct Entry {
    pending: Option<watch::Receiver<Completion>>,
    next: Instant,
    failures: u32,
}
#[derive(Clone)]
pub(crate) struct Flights {
    entries: Arc<Mutex<HashMap<RunId, Entry>>>,
    observers: Arc<Semaphore>,
    throttled: bool,
}

impl Flights {
    pub fn refreshes() -> Self {
        Self::new(true, 64)
    }
    pub fn cancellations() -> Self {
        Self::new(false, 16)
    }
    fn new(throttled: bool, observers: usize) -> Self {
        Self {
            entries: Arc::new(Mutex::new(HashMap::new())),
            observers: Arc::new(Semaphore::new(observers)),
            throttled,
        }
    }
    /// None means the caller should read the current saved ledger record; no
    /// work or timer is created for observers arriving during cooldown.
    pub async fn run<F, Fut>(&self, run: RunId, operation: F) -> Result<Option<Admission>>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = Result<Admission>> + Send + 'static,
    {
        let _observer = self
            .observers
            .clone()
            .try_acquire_owned()
            .map_err(|_| Error::Busy)?;
        let now = Instant::now();
        let (mut receiver, sender) = {
            let mut entries = self
                .entries
                .lock()
                .map_err(|_| Error::Invalid("refresh registry failed"))?;
            if !entries.contains_key(&run) && entries.len() == RUNS {
                let unused = entries
                    .iter()
                    .find(|(_, entry)| entry.pending.is_none() && now >= entry.next)
                    .map(|(id, _)| id.clone())
                    .ok_or(Error::Busy)?;
                entries.remove(&unused);
            }
            let entry = entries.entry(run.clone()).or_insert(Entry {
                pending: None,
                next: now,
                failures: 0,
            });
            if let Some(receiver) = &entry.pending {
                (receiver.clone(), None)
            } else if now < entry.next {
                return Ok(None);
            } else {
                let (sender, receiver) = watch::channel(None);
                entry.pending = Some(receiver.clone());
                (receiver, Some(sender))
            }
        };
        if let Some(sender) = sender {
            let this = self.clone();
            // Monitor a bounded child task so a port panic also completes all
            // waiters and releases the slot. Dropped observers never abort it.
            tokio::spawn(async move {
                let result = tokio::spawn(async move { operation().await })
                    .await
                    .unwrap_or_else(|_| Err(Error::Invalid("coordinator operation failed")));
                let failed = result.as_ref().map_or(true, |record| {
                    record.phase == Phase::Reconciling
                        || matches!(
                            record.reason,
                            Some(
                                Reason::ControlUncertain
                                    | Reason::WorkerUnavailable
                                    | Reason::StartUncertain
                                    | Reason::BeginUncertain
                            )
                        )
                });
                if let Ok(mut entries) = this.entries.lock()
                    && let Some(entry) = entries.get_mut(&run)
                {
                    entry.failures = if failed {
                        entry.failures.saturating_add(1)
                    } else {
                        0
                    };
                    entry.next = Instant::now()
                        + if this.throttled {
                            delay(entry.failures)
                        } else {
                            Duration::ZERO
                        };
                    // Publish completion while holding the registry lock, then
                    // allow a later operation; no observer can miss the result.
                    sender.send_replace(Some(result.map_err(Arc::new)));
                    entry.pending = None;
                }
            });
        }
        loop {
            if let Some(result) = receiver.borrow_and_update().clone() {
                return result.map(Some).map_err(Error::Shared);
            }
            receiver
                .changed()
                .await
                .map_err(|_| Error::Invalid("coordinator completion lost"))?;
        }
    }
}
fn delay(failures: u32) -> Duration {
    REFRESH_FLOOR
        .saturating_mul(1u32 << failures.saturating_sub(1).min(5))
        .min(BACKOFF_MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test(start_paused = true)]
    async fn failed_refreshes_back_off_share_work_and_recover_after_observers_leave() {
        let flights = Flights::refreshes();
        let run = RunId::new("fixture").unwrap();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        for wait in [250, 500, 1000, 2000, 4000, 5000, 5000] {
            let calls = calls.clone();
            assert!(
                flights
                    .run(run.clone(), move || async move {
                        calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        Err(Error::Uncertain)
                    })
                    .await
                    .is_err()
            );
            tokio::time::advance(Duration::from_millis(wait - 1)).await;
            assert!(
                flights
                    .run(run.clone(), || async { panic!("refresh before deadline") })
                    .await
                    .unwrap()
                    .is_none()
            );
            tokio::time::advance(Duration::from_millis(1)).await;
        }
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 7);
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let first = {
            let (flights, run, entered, release) = (
                flights.clone(),
                run.clone(),
                entered.clone(),
                release.clone(),
            );
            tokio::spawn(async move {
                flights
                    .run(run, move || async move {
                        entered.notify_one();
                        release.notified().await;
                        Err(Error::Uncertain)
                    })
                    .await
            })
        };
        entered.notified().await;
        first.abort();
        // A dropped observer does not free the outstanding operation.
        let second = {
            let (flights, run) = (flights.clone(), run.clone());
            tokio::spawn(async move {
                flights
                    .run(run, || async { panic!("duplicate refresh") })
                    .await
            })
        };
        tokio::task::yield_now().await;
        release.notify_one();
        assert!(second.await.unwrap().is_err());
    }
}
