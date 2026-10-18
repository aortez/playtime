//! Native workbench. Background clients prepare bounded data; drawing has no I/O.
pub mod backend;
pub mod media;
pub mod model;
mod read_retry;
pub mod transcript;
pub mod ui;
