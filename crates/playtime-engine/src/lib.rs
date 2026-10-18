//! Bounded controller decisions and replaceable external effects.
//!
//! No model, shell, Docker, clock, or filesystem calls occur in the controller.
//! Drivers must persist observations before dispatch, honor effect deadlines,
//! and cancel abandoned effects. The simulation drives this same controller.

mod context;
mod controller;
pub mod evidence;
pub mod simulation;
mod types;

pub use controller::Controller;
pub use types::*;
