use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use playtime_core::{RunId, Sha256Digest};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::ReadError;

pub const CURSOR_BYTES: usize = 2048;

/// Public position metadata, never a writer receipt or an authorization token.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Prefix {
    pub sequence: u64,
    pub byte_offset: u64,
    #[schema(value_type = String, pattern = "^[0-9a-f]{64}$")]
    pub sha256: Sha256Digest,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Frontier {
    #[schema(value_type = String)]
    pub run_id: RunId,
    #[schema(value_type = String, pattern = "^[0-9a-f]{64}$")]
    pub manifest_sha256: Sha256Digest,
    pub prefix: Prefix,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct EventCursor {
    pub schema_version: u32,
    pub frontier: Frontier,
    pub after_sequence: u64,
}
impl EventCursor {
    pub fn new(frontier: Frontier, after_sequence: u64) -> Result<Self, ReadError> {
        let value = Self {
            schema_version: 1,
            frontier,
            after_sequence,
        };
        value.validate()?;
        Ok(value)
    }
    fn validate(&self) -> Result<(), ReadError> {
        if self.schema_version != 1 {
            return Err(ReadError::UnsupportedVersion {
                format: "event cursor",
                received: self.schema_version,
                supported: 1,
            });
        }
        let prefix = &self.frontier.prefix;
        if self.after_sequence > prefix.sequence
            || (prefix.sequence == 0) != (prefix.byte_offset == 0)
            || prefix.byte_offset < prefix.sequence
        {
            return Err(ReadError::malformed("inconsistent cursor position"));
        }
        Ok(())
    }
    pub fn encode(&self) -> Result<String, ReadError> {
        self.validate()?;
        let bytes = serde_json::to_vec(self).map_err(ReadError::malformed)?;
        let result = URL_SAFE_NO_PAD.encode(bytes);
        if result.len() > CURSOR_BYTES {
            return Err(ReadError::malformed("cursor exceeds limit"));
        }
        Ok(result)
    }
    /// Only structural validation. The application must verify the historical
    /// prefix against trusted committed evidence before using this position.
    pub fn decode(encoded: &str) -> Result<Self, ReadError> {
        if encoded.is_empty() || encoded.len() > CURSOR_BYTES {
            return Err(ReadError::malformed("invalid cursor size"));
        }
        let bytes = URL_SAFE_NO_PAD
            .decode(encoded)
            .map_err(ReadError::malformed)?;
        let cursor: Self = serde_json::from_slice(&bytes).map_err(ReadError::malformed)?;
        cursor.validate()?;
        Ok(cursor)
    }
}
