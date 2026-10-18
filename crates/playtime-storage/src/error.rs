use std::{fmt, io};

use playtime_protocol::ReadError;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorCode {
    MalformedData,
    UnsupportedVersion,
    InvalidCursor,
    CursorAhead,
    ArchiveCorrupt,
    ContentMismatch,
    WriterBusy,
    LimitExceeded,
    IoError,
}

impl ErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MalformedData => "malformed_data",
            Self::UnsupportedVersion => "unsupported_version",
            Self::InvalidCursor => "invalid_cursor",
            Self::CursorAhead => "cursor_ahead",
            Self::ArchiveCorrupt => "archive_corrupt",
            Self::ContentMismatch => "content_mismatch",
            Self::WriterBusy => "writer_busy",
            Self::LimitExceeded => "limit_exceeded",
            Self::IoError => "io_error",
        }
    }
}

#[derive(Debug)]
pub struct StorageError {
    pub code: ErrorCode,
    pub message: String,
    /// A mutation failed after entering publication. Its writer is stopped;
    /// neither the last receipt nor the filesystem proves the failed outcome.
    pub outcome_uncertain: bool,
    pub io_kind: Option<io::ErrorKind>,
}

impl StorageError {
    pub(crate) fn new(code: ErrorCode, message: impl fmt::Display) -> Self {
        Self {
            code,
            message: message.to_string(),
            outcome_uncertain: false,
            io_kind: None,
        }
    }

    pub(crate) fn corrupt(message: impl fmt::Display) -> Self {
        Self::new(ErrorCode::ArchiveCorrupt, message)
    }

    pub(crate) fn malformed(message: impl fmt::Display) -> Self {
        Self::new(ErrorCode::MalformedData, message)
    }

    pub(crate) fn limit(message: impl fmt::Display) -> Self {
        Self::new(ErrorCode::LimitExceeded, message)
    }
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code.as_str(), self.message)?;
        if self.outcome_uncertain {
            write!(f, " (publication outcome uncertain; writer stopped)")?;
        }
        Ok(())
    }
}
impl std::error::Error for StorageError {}
impl From<io::Error> for StorageError {
    fn from(error: io::Error) -> Self {
        let mut value = Self::new(ErrorCode::IoError, &error);
        value.io_kind = Some(error.kind());
        value
    }
}
impl From<rustix::io::Errno> for StorageError {
    fn from(error: rustix::io::Errno) -> Self {
        io::Error::from(error).into()
    }
}
impl From<ReadError> for StorageError {
    fn from(error: ReadError) -> Self {
        let code = match error {
            ReadError::MalformedData(_) => ErrorCode::MalformedData,
            ReadError::UnsupportedVersion { .. } => ErrorCode::UnsupportedVersion,
        };
        Self::new(code, error)
    }
}
pub type Result<T> = std::result::Result<T, StorageError>;
