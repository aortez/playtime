//! Incremental controller/archive integration. No HTTP, database, model,
//! Docker, recovery mutation, or execution admission policy lives here.

mod clock;
mod driver;
mod error;
mod mapping;
mod prepare;
mod queries;
mod receipt;
pub mod recovery;
mod writer;

pub use clock::{Cancellation, DriverClock, VirtualClock};
pub use driver::{DriverOptions, PreparedExecution, RunReport, run_prepared};
pub use error::{Error, Result};
pub use prepare::{
    PreparedSimulation, WorkerBundle, capture_demo_worker,
    prepare_recoverable_simulation_with_inputs, prepare_simulation, prepare_simulation_with_inputs,
};
pub use queries::ArchiveQueries;
pub use receipt::LatestReceipt;
pub use writer::{CommitContext, NoWriterHooks, PendingPublication, ReceiptDelivery, WriterHooks};
