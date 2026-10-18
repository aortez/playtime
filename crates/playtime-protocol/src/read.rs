use std::fmt;

use playtime_core::RequestId;
use serde::Deserialize;
use serde_json::{Map, json};

use crate::v1::{ErrorResponse, MALFORMED_DATA, UNSUPPORTED_VERSION};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReadError {
    MalformedData(String),
    UnsupportedVersion {
        format: &'static str,
        received: u32,
        supported: u32,
    },
}

impl ReadError {
    pub(crate) fn malformed(error: impl fmt::Display) -> Self {
        Self::MalformedData(error.to_string())
    }

    pub fn into_response(self, request_id: Option<RequestId>) -> ErrorResponse {
        let (code, details) = match &self {
            Self::MalformedData(_) => (MALFORMED_DATA, Map::new()),
            Self::UnsupportedVersion {
                format,
                received,
                supported,
            } => (
                UNSUPPORTED_VERSION,
                Map::from_iter([
                    ("format".into(), json!(format)),
                    ("received".into(), json!(received)),
                    ("supported".into(), json!([supported])),
                ]),
            ),
        };
        ErrorResponse {
            code: code.into(),
            message: self.to_string(),
            request_id,
            details,
        }
    }
}

impl fmt::Display for ReadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MalformedData(message) => write!(formatter, "Malformed data: {message}"),
            Self::UnsupportedVersion {
                format,
                received,
                supported,
            } => {
                write!(
                    formatter,
                    "Unsupported {format} schema version {received}; supported: {supported}"
                )
            }
        }
    }
}

impl std::error::Error for ReadError {}

// Check only the version header before attempting to interpret the body. A new
// major may have a completely different shape; that is not malformed v1 data.
pub(crate) fn require_version(
    json: &str,
    format: &'static str,
    supported: u32,
) -> Result<(), ReadError> {
    #[derive(Deserialize)]
    struct Header {
        schema_version: u32,
    }

    let header: Header = serde_json::from_str(json).map_err(ReadError::malformed)?;
    if header.schema_version != supported {
        return Err(ReadError::UnsupportedVersion {
            format,
            received: header.schema_version,
            supported,
        });
    }
    Ok(())
}
