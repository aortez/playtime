//! Inspectable links for continuous execution observations inside execution_v2.
//! Locations identify retained source bytes; they confer no publication authority.
use crate::{ReadError, execution::Observation, observations::ObservationLocation};
use playtime_core::{RequestId, RequestPurpose, RequestScope};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeSet, num::NonZeroU64};

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RequestSource {
    pub request_id: RequestId,
    pub purpose: RequestPurpose,
    /// None explicitly retains missing/omitted source evidence.
    pub observation: Option<ObservationLocation>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceLinks {
    pub schema_version: u32,
    pub requests: Vec<RequestSource>,
    /// A plan may be absent from the retained observation stream.
    pub plan: Option<ObservationLocation>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "availability", rename_all = "snake_case")]
pub enum EvidenceEntry {
    Recorded {
        location: ObservationLocation,
        kind: String,
        request: Option<crate::execution::RequestIdentity>,
        value: Value,
    },
    Omitted {
        archive_sequence: NonZeroU64,
        controller_sequence: Option<NonZeroU64>,
        kind: String,
        request: Option<crate::execution::RequestIdentity>,
        reason: String,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct EvidencePage {
    pub entries: Vec<EvidenceEntry>,
    /// Cursor advances over all archive events, including unrelated records.
    pub next_after_sequence: u64,
    pub has_more: bool,
}

pub fn evidence_kind(kind: &str) -> bool {
    matches!(
        kind,
        "compaction_configured"
            | "compaction_deferred"
            | "compaction_planned"
            | "compaction_applied"
            | "compaction_rejected"
            | "completion_declared"
            | "model_completion"
            | "tool_batch_rejected"
    )
}

pub fn linked_kind(kind: &str) -> bool {
    matches!(
        kind,
        "compaction_planned" | "compaction_applied" | "completion_declared"
    )
}

pub fn validate(
    observation: &Observation,
    value: &Value,
) -> Result<Option<EvidenceLinks>, ReadError> {
    let kind = observation.observation_kind.as_str();
    if !linked_kind(kind) {
        return Ok(None);
    }
    let fail = || ReadError::malformed("invalid compaction/completion evidence");
    let links: EvidenceLinks =
        serde_json::from_value(value["evidence"].clone()).map_err(ReadError::malformed)?;
    if links.schema_version != 1 || observation.scope != RequestScope::Run {
        return Err(fail());
    }
    let mut unique = BTreeSet::new();
    if links.requests.iter().any(|s| !unique.insert(&s.request_id)) {
        return Err(fail());
    }
    if kind == "compaction_planned" {
        if observation.request.is_some() || links.plan.is_some() {
            return Err(fail());
        }
        let source: Vec<RequestId> = serde_json::from_value(value["source_requests"].clone())
            .map_err(ReadError::malformed)?;
        let retained: Vec<RequestId> = serde_json::from_value(value["retained_requests"].clone())
            .map_err(ReadError::malformed)?;
        let previous: Option<RequestId> =
            serde_json::from_value(value["previous_summary_request"].clone())
                .map_err(ReadError::malformed)?;
        if source.is_empty()
            || previous.is_some() == value["previous_summary"].is_null()
            || value["before_messages"].as_array().is_none()
        {
            return Err(fail());
        }
        let expected = source
            .iter()
            .chain(&retained)
            .map(|id| (id, RequestPurpose::Work))
            .chain(previous.iter().map(|id| (id, RequestPurpose::Compaction)));
        if !links
            .requests
            .iter()
            .map(|s| (&s.request_id, s.purpose))
            .eq(expected)
        {
            return Err(fail());
        }
    } else {
        let request = observation.request.as_ref().ok_or_else(fail)?;
        let (field, purpose) = if kind == "compaction_applied" {
            if value["messages"].as_array().is_none() {
                return Err(fail());
            }
            ("summary_request_id", RequestPurpose::Compaction)
        } else {
            if links.plan.is_some() {
                return Err(fail());
            }
            ("request_id", RequestPurpose::Work)
        };
        if request.purpose != purpose
            || value[field].as_str() != Some(request.request_id.as_str())
            || links.requests.len() != 1
            || links.requests[0].request_id != request.request_id
            || links.requests[0].purpose != purpose
            || value["summary"].as_str().is_none_or(|s| {
                s.trim().is_empty() || (kind == "completion_declared" && s.len() > 8192)
            })
        {
            return Err(fail());
        }
    }
    if kind != "completion_declared" {
        if value["ordinal"]
            .as_u64()
            .and_then(NonZeroU64::new)
            .is_none()
        {
            return Err(fail());
        }
        for field in ["input_bound", "projected_tokens"] {
            if value[field].as_u64().is_none() {
                return Err(fail());
            }
        }
        if !matches!(
            value["estimate_basis"].as_str(),
            Some("input_bytes" | "calibrated_bytes" | "server_usage_plus_new_bytes")
        ) {
            return Err(fail());
        }
    }
    Ok(Some(links))
}
