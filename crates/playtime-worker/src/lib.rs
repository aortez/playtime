//! Production worker composition and frozen package validation. No HTTP/SQLite.
pub mod bundle;
mod elf;
pub mod execution;
mod files;

pub use elf::verify_static_elf;
pub use files::PrivateDirectory;

pub type Result<T> = std::result::Result<T, Error>;
#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Json(serde_json::Error),
    Storage(playtime_storage::StorageError),
    Runner(playtime_runner::Error),
    Control(playtime_worker_control::Error),
    Invalid(&'static str),
    Capacity,
    UnsupportedPlatform,
    UnsupportedCapability,
    DynamicExecutable,
    Unbundled,
    ExecutionUnavailable,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "worker package: {self:?}")
    }
}
impl std::error::Error for Error {}
macro_rules! from_error {
    ($kind:ty, $variant:ident) => {
        impl From<$kind> for Error {
            fn from(error: $kind) -> Self {
                Self::$variant(error)
            }
        }
    };
}
from_error!(std::io::Error, Io);
from_error!(serde_json::Error, Json);
from_error!(playtime_storage::StorageError, Storage);
from_error!(playtime_runner::Error, Runner);
from_error!(playtime_worker_control::Error, Control);

pub const EMBEDDED_BUILD: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/build.json"));
