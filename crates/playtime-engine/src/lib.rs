//! Bounded controller decisions and replaceable external effects.
//!
//! No model, shell, Docker, clock, or filesystem calls occur in the controller.
//! Drivers must persist observations before dispatch, honor effect deadlines,
//! and cancel abandoned effects. The simulation drives this same controller.

pub mod compaction;
mod context;
mod control;
mod controller;
pub mod evidence;
mod policy;
pub mod simulation;
mod types;
mod validation;

pub use control::*;
pub use controller::Controller;
pub use policy::SessionInput;
pub use types::*;
