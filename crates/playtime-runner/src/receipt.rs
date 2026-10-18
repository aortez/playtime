use std::sync::{Arc, Mutex};

use playtime_storage::CommittedFrontier;

use crate::{Error, Result};

/// Read-only, coalesced publication visibility for one worker host. The mutex
/// protects only an in-memory clone/assignment and is never held across I/O or
/// controller transitions. There are no per-observer queues or filesystem reads.
#[derive(Clone, Default)]
pub struct LatestReceipt(Arc<Mutex<Option<CommittedFrontier>>>);
impl LatestReceipt {
    pub fn latest(&self) -> Option<CommittedFrontier> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
    pub(crate) fn publish(&self, receipt: &CommittedFrontier) -> Result<()> {
        let mut latest = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if latest.as_ref().is_some_and(|previous| {
            previous.frontier().archive != receipt.frontier().archive
                || previous.frontier().committed_sequence > receipt.frontier().committed_sequence
        }) {
            return Err(Error::Invalid(
                "receipt observer must stay bound to one archive and advance monotonically".into(),
            ));
        }
        *latest = Some(receipt.clone());
        Ok(())
    }
}
