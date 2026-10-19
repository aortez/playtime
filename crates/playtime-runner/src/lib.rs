//! Incremental controller/archive integration. The worker supplies native effects;
//! no HTTP, database, recovery mutation or execution admission policy lives here.

mod clock;
pub mod docker;
mod driver;
mod effects;
pub use effects::{CheckpointBytes, Effects};
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
    PreparedSimulation, WorkerBundle, capture_demo_worker, prepare_docker_with_inputs,
    prepare_recoverable_simulation_with_inputs, prepare_simulation, prepare_simulation_with_inputs,
};
pub use queries::ArchiveQueries;
pub use receipt::LatestReceipt;
pub use writer::{CommitContext, NoWriterHooks, PendingPublication, ReceiptDelivery, WriterHooks};
