use std::fmt;

#[derive(Debug)]
pub enum Error {
    Storage(playtime_storage::StorageError),
    Protocol(playtime_protocol::ReadError),
    Controller(playtime_core::InvalidValue),
    Json(serde_json::Error),
    Io(std::io::Error),
    Invalid(String),
    Capacity(String),
    LogLimit(String),
    Uncertain(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Storage(error) => error.fmt(f),
            Self::Protocol(error) => error.fmt(f),
            Self::Controller(error) => error.fmt(f),
            Self::Json(error) => error.fmt(f),
            Self::Io(error) => error.fmt(f),
            Self::Invalid(error)
            | Self::Capacity(error)
            | Self::LogLimit(error)
            | Self::Uncertain(error) => f.write_str(error),
        }
    }
}
impl std::error::Error for Error {}
macro_rules! from {
    ($source:ty, $variant:ident) => {
        impl From<$source> for Error {
            fn from(error: $source) -> Self {
                Self::$variant(error)
            }
        }
    };
}
from!(playtime_storage::StorageError, Storage);
from!(playtime_protocol::ReadError, Protocol);
from!(playtime_core::InvalidValue, Controller);
from!(serde_json::Error, Json);
from!(std::io::Error, Io);
pub type Result<T> = std::result::Result<T, Error>;
