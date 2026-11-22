//! Bounded controller decisions and replaceable external effects.
//!
//! No model, shell, Docker, clock, or filesystem calls occur in the controller.
//! Drivers must persist observations before dispatch, honor effect deadlines,
//! and cancel abandoned effects. The simulation drives this same controller.

pub mod chat;
mod chat_wire;
pub mod compaction;
mod context;
mod control;
mod controller;
pub mod conversation;
pub mod evidence;
pub mod execution;
mod policy;
pub mod simulation;
mod types;
mod validation;

pub use control::*;
pub use controller::{Controller, LIFECYCLE_MS};
pub use policy::SessionInput;
pub use types::*;
