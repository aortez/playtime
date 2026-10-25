//! Versioned session-neutral execution evidence. These records extend the
//! archive's v1 envelope without changing any v1 session event or observation.

use std::{collections::BTreeSet, num::NonZeroU64};

use playtime_core::{
    CommandOutcome, GenerationUsage, RequestId, RequestPurpose, RequestScope, RunId,
};
use serde::{Deserialize, Serialize};

use crate::{
    ReadError,
    archive::ContentRef,
    bridge::{FileContent, FileVersion},
    observations::{ObservationData, ObservationIdentity, ObservationLocation},
    read::require_version,
};

pub const KIND: &str = "execution_v2";
pub const VERSION: u32 = 2;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RequestIdentity {
    pub effect_id: NonZeroU64,
    pub request_id: RequestId,
    pub call: NonZeroU64,
    pub scope: RequestScope,
    pub purpose: RequestPurpose,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct EffectIntent {
    pub effect_id: NonZeroU64,
    pub effect_kind: String,
    pub request: Option<RequestIdentity>,
    pub tool_request_sequence: Option<NonZeroU64>,
    pub maximum_tokens: Option<NonZeroU64>,
    pub deadline_ms: u64,
    pub sources: Vec<ObservationLocation>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CaptureRef {
    pub workspace: ContentRef,
    pub index: ContentRef,
}

/// A final stopped workspace has files and provenance, but no synthetic
/// session, handoff, memory file or compaction checkpoint.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CaptureIndex {
    pub schema_version: u32,
    pub run_id: RunId,
    pub workspace: ContentRef,
    pub files: Vec<FileVersion>,
    pub source_map: ContentRef,
}

impl CaptureIndex {
    pub fn content_leaves(&self) -> Vec<&ContentRef> {
        let mut leaves = vec![&self.workspace];
        for file in &self.files {
            if let FileContent::Retained { content } = &file.content {
                leaves.push(content);
            }
        }
        leaves
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Observation {
    pub controller_sequence: NonZeroU64,
    pub elapsed_ms: u64,
    pub scope: RequestScope,
    pub observation_kind: String,
    pub identity: ObservationIdentity,
    pub request: Option<RequestIdentity>,
    pub data: ObservationData,
}

impl Observation {
    pub fn validate_value(&self, value: &serde_json::Value) -> Result<(), ReadError> {
        if self.observation_kind == "model_request" {
            let request = self.request.as_ref().ok_or_else(|| {
                ReadError::malformed("model observation lacks request purpose/identity")
            })?;
            if value.get("purpose")
                != Some(&serde_json::to_value(request.purpose).map_err(ReadError::malformed)?)
                || value.get("request_id")
                    != Some(
                        &serde_json::to_value(&request.request_id).map_err(ReadError::malformed)?,
                    )
                || value.get("call").and_then(serde_json::Value::as_u64) != Some(request.call.get())
                || value.get("handoff").is_some()
            {
                return Err(ReadError::malformed(
                    "v2 model request disagrees with its explicit purpose/identity",
                ));
            }
        }
        Ok(())
    }

    pub fn resolve(&self, bytes: &[u8]) -> Result<serde_json::Value, ReadError> {
        use sha2::{Digest, Sha256};
        let ObservationData::Artifact { content } = &self.data else {
            return Err(ReadError::malformed("observation is already inline"));
        };
        let digest: String = Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        if bytes.len() as u64 != content.size_bytes || digest != content.sha256.as_str() {
            return Err(ReadError::malformed(
                "observation content identity mismatch",
            ));
        }
        let value = serde_json::from_slice(bytes).map_err(ReadError::malformed)?;
        self.validate_value(&value)?;
        Ok(value)
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum Fact {
    RunFinished {
        reason: playtime_core::StopReason,
        usage: crate::v1::UsageSummary,
        cleanup: crate::events::CleanupOutcome,
        last_capture: Option<CaptureRef>,
    },
    WorkspaceStarted {
        workspace: ContentRef,
    },
    EffectIntent(EffectIntent),
    EffectNotDispatched {
        effect_id: NonZeroU64,
        request: Option<RequestIdentity>,
        tool_request_sequence: Option<NonZeroU64>,
    },
    RequestSettled {
        request: RequestIdentity,
        usage: GenerationUsage,
    },
    RequestNotDispatched {
        request: RequestIdentity,
        released_tokens: NonZeroU64,
    },
    CommandFinished {
        request: RequestIdentity,
        effect_id: NonZeroU64,
        tool_request_sequence: NonZeroU64,
        outcome: CommandOutcome,
    },
    CapturePublished {
        capture: CaptureRef,
    },
    Observation(Observation),
    ObservationOmitted {
        controller_sequence: Option<NonZeroU64>,
        observation_kind: String,
        elapsed_ms: u64,
        scope: RequestScope,
        identity: ObservationIdentity,
        request: Option<RequestIdentity>,
        size_bytes: u64,
        reason: String,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Record {
    pub schema_version: u32,
    pub fact: Fact,
}

impl Record {
    pub fn new(fact: Fact) -> Self {
        Self {
            schema_version: VERSION,
            fact,
        }
    }
}

pub fn decode_record(json: &str) -> Result<Record, ReadError> {
    require_version(json, "execution record", VERSION)?;
    let record: Record = serde_json::from_str(json).map_err(ReadError::malformed)?;
    match &record.fact {
        Fact::RunFinished { usage, cleanup, .. } => {
            usage.validate().map_err(ReadError::malformed)?;
            if matches!(cleanup, crate::events::CleanupOutcome::Failed { errors } if errors.is_empty())
            {
                return Err(ReadError::malformed("failed cleanup must include errors"));
            }
        }
        Fact::EffectIntent(intent) => {
            if intent.effect_kind.is_empty()
                || (intent.effect_kind == "model"
                    && (intent.maximum_tokens.is_none()
                        || intent
                            .request
                            .as_ref()
                            .is_none_or(|request| request.effect_id != intent.effect_id)))
                || (intent.effect_kind != "model" && intent.maximum_tokens.is_some())
            {
                return Err(ReadError::malformed(
                    "execution intent lacks its exact request identity/allowance",
                ));
            }
        }
        Fact::Observation(observation) => {
            validate_association(
                &observation.scope,
                &observation.identity,
                observation.request.as_ref(),
            )?;
            if observation.observation_kind.is_empty() {
                return Err(ReadError::malformed("observation kind is empty"));
            }
            match &observation.data {
                ObservationData::Inline { value } => {
                    if serde_json::to_vec(value)
                        .map_err(ReadError::malformed)?
                        .len()
                        > crate::observations::INLINE_OBSERVATION_BYTES
                    {
                        return Err(ReadError::malformed(
                            "large observation values must be externalized",
                        ));
                    }
                    observation.validate_value(value)?;
                }
                ObservationData::Artifact { content }
                    if content.size_bytes
                        <= crate::observations::INLINE_OBSERVATION_BYTES as u64 =>
                {
                    return Err(ReadError::malformed(
                        "small observation values must be inline",
                    ));
                }
                _ => {}
            }
        }
        Fact::ObservationOmitted {
            scope,
            identity,
            request,
            observation_kind,
            reason,
            ..
        } => {
            validate_association(scope, identity, request.as_ref())?;
            if observation_kind.is_empty() || reason.is_empty() {
                return Err(ReadError::malformed(
                    "omission must identify its kind and reason",
                ));
            }
        }
        _ => {}
    }
    Ok(record)
}

fn validate_association(
    scope: &RequestScope,
    identity: &ObservationIdentity,
    request: Option<&RequestIdentity>,
) -> Result<(), ReadError> {
    if let Some(request) = request {
        if &request.scope != scope || identity.request_id.as_ref() != Some(&request.request_id) {
            return Err(ReadError::malformed(
                "observation request scope/identity mismatch",
            ));
        }
    } else if identity.request_id.is_some() {
        return Err(ReadError::malformed(
            "request observation lacks purpose attribution",
        ));
    }
    Ok(())
}

pub fn decode_capture_index(json: &str) -> Result<CaptureIndex, ReadError> {
    require_version(json, "workspace capture index", VERSION)?;
    let index: CaptureIndex = serde_json::from_str(json).map_err(ReadError::malformed)?;
    let mut paths = BTreeSet::new();
    for file in &index.files {
        if file.path.is_empty() || !paths.insert(&file.path) {
            return Err(ReadError::malformed(
                "capture file names must be nonempty and unique",
            ));
        }
        if let FileContent::Retained { content } = &file.content
            && (file.link.is_some()
                || !matches!(file.entry_type.as_str(), "0" | "file")
                || file.sha256.as_ref() != Some(&content.sha256)
                || file.size_bytes != content.size_bytes)
        {
            return Err(ReadError::malformed(
                "capture regular file identity mismatch",
            ));
        }
    }
    Ok(index)
}
