use playtime_core::{InvalidValue, RequestId, RunParameters, UsageTotals};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use utoipa::ToSchema;

use crate::ReadError;

pub const API_PREFIX: &str = "/api/v1";
pub const MALFORMED_DATA: &str = "malformed_data";
pub const UNSUPPORTED_VERSION: &str = "unsupported_version";

/// A read model of accounting outcomes, never an admission/config input.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct UsageSummary {
    pub known_generated_tokens: u64,
    pub unknown_reserved_tokens: u64,
    pub unknown_requests: u64,
    pub budget_charged_tokens: u64,
    pub usage_complete: bool,
}

impl UsageSummary {
    pub fn validate(&self) -> Result<(), InvalidValue> {
        if self
            .known_generated_tokens
            .checked_add(self.unknown_reserved_tokens)
            != Some(self.budget_charged_tokens)
            || (self.unknown_requests == 0) != (self.unknown_reserved_tokens == 0)
            || self.unknown_requests > self.unknown_reserved_tokens
            || self.usage_complete != (self.unknown_requests == 0)
        {
            return Err(InvalidValue("inconsistent generation usage summary"));
        }
        Ok(())
    }
}

pub fn decode_run_parameters(json: &str) -> Result<RunParameters, ReadError> {
    let parameters: RunParameters = serde_json::from_str(json).map_err(ReadError::malformed)?;
    parameters.validate().map_err(ReadError::malformed)?;
    Ok(parameters)
}

impl From<UsageTotals> for UsageSummary {
    fn from(totals: UsageTotals) -> Self {
        Self {
            known_generated_tokens: totals.known_generated_tokens(),
            unknown_reserved_tokens: totals.unknown_reserved_tokens(),
            unknown_requests: totals.unknown_requests(),
            budget_charged_tokens: totals.budget_charged_tokens(),
            usage_complete: totals.usage_complete(),
        }
    }
}

/// Clients may display errors with new codes and compatible additional fields.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, ToSchema)]
pub struct ErrorResponse {
    /// Stable machine-readable code; do not branch on the readable message.
    pub code: String,
    pub message: String,
    /// Optional opaque correlation ID; an empty ID is invalid.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>, pattern = "^[A-Za-z0-9][A-Za-z0-9_-]{0,127}$")]
    pub request_id: Option<RequestId>,
    #[schema(additional_properties = true)]
    pub details: Map<String, Value>,
}
