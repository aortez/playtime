//! Incremental controller/archive integration. No HTTP, database, model,
//! Docker, recovery, or execution admission policy lives here.

mod clock;
mod driver;
mod error;
mod mapping;
mod prepare;
mod queries;
mod receipt;
mod writer;

pub use clock::{Cancellation, DriverClock, VirtualClock};
pub use driver::{DriverOptions, RunReport, run_prepared};
pub use error::{Error, Result};
pub use prepare::{PreparedSimulation, WorkerBundle, capture_demo_worker, prepare_simulation};
pub use queries::ArchiveQueries;
pub use receipt::LatestReceipt;
pub use writer::{CommitContext, NoWriterHooks, PendingPublication, ReceiptDelivery, WriterHooks};
