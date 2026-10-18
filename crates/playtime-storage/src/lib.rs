//! Synchronous, Linux-first portable archive storage. No controller, execution,
//! clock, admission ledger, or background flush lives here.

#[cfg(not(target_os = "linux"))]
compile_error!(
    "playtime-storage currently implements Linux directory durability and no-replace publication"
);

mod error;
mod files;
pub mod fixture;
mod log;
mod reader;
mod writer;

pub use error::{ErrorCode, Result, StorageError};
pub use files::{PublicationObserver, WriteStep, WriterLock, content_ref, sha256};
pub use playtime_protocol::storage::StorageLimits;
pub use reader::{ArchiveReader, EventPage, Inspection, Snapshot, StoredEvent, WorkerAvailability};
pub use writer::{ArchiveWriter, BatchClass, CommittedFrontier, FrozenInput};

pub(crate) fn validate_limits(limits: &StorageLimits) -> Result<()> {
    let positive = [
        limits.record_bytes,
        limits.batch_bytes,
        limits.content_bytes,
        limits.manifest_bytes,
        limits.verified_content_bytes,
        limits.references,
        limits.events,
        limits.page_bytes,
    ];
    if positive.iter().any(|v| *v == 0 || *v > isize::MAX as u64)
        || limits.event_log_bytes > isize::MAX as u64
        || limits.record_bytes > limits.batch_bytes
        || limits.record_bytes > limits.page_bytes
        || limits.manifest_bytes > limits.content_bytes
    {
        return Err(StorageError::malformed(
            "invalid or inconsistent storage limits",
        ));
    }
    Ok(())
}
