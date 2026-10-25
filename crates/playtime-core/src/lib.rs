//! Pure experiment values and invariants, independent of runtime and I/O.

mod accounting;
mod limits;
mod values;
mod vocabulary;

pub use accounting::{AccountingOverflow, GenerationUsage, UsageTotals};
pub use limits::{CommonLimits, CommonParameters, ResourceLimits, RunParameters, WorkshopLimits};
pub use values::{
    ArchivePath, HarnessId, InvalidValue, RequestId, RunId, SessionId, Sha256Digest, UtcTime,
};
pub use vocabulary::{
    CommandOutcome, MemoryCondition, RequestPurpose, RequestScope, RunMode, RunPhase, StopReason,
};
