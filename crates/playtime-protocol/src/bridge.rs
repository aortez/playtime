//! Durable control facts and bounded checkpoint manifests for the controller
//! driver. Raw observations are separate inspectable copies, never settlements.

use std::{
    collections::BTreeSet,
    num::{NonZeroU32, NonZeroU64},
};

use playtime_core::{RequestId, RunId, SessionId, Sha256Digest, StopReason};
use serde::{Deserialize, Serialize};

use crate::{
    ReadError,
    archive::ContentRef,
    observations::{ObservationIdentity, ObservationLocation},
    read::require_version,
};

pub const BRIDGE_SCHEMA_VERSION: u32 = 1;
pub const CHECKPOINT_INDEX_VERSION: u32 = 1;
pub const SOURCE_MAP_VERSION: u32 = 1;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct EffectIntent {
    pub schema_version: u32,
    pub effect_id: NonZeroU64,
    /// Open vocabulary for inspection. Execution is exclusively controller-owned.
    pub effect_kind: String,
    pub session_id: Option<SessionId>,
    pub request_id: Option<RequestId>,
    pub tool_request_sequence: Option<NonZeroU64>,
    pub maximum_tokens: Option<NonZeroU64>,
    pub deadline_ms: u64,
    pub sources: Vec<ObservationLocation>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct EffectNotDispatched {
    pub schema_version: u32,
    pub effect_id: NonZeroU64,
    pub session_id: Option<SessionId>,
    pub identity: ObservationIdentity,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct RequestNotDispatched {
    pub schema_version: u32,
    pub effect_id: NonZeroU64,
    pub session_id: SessionId,
    pub request_id: RequestId,
    pub released_tokens: NonZeroU64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct StopDecided {
    pub schema_version: u32,
    pub reason: StopReason,
    pub elapsed_ms: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ObservationOmitted {
    pub schema_version: u32,
    /// None when the engine rejected a raw record before allocating a sequence.
    pub controller_sequence: Option<NonZeroU64>,
    pub observation_kind: String,
    pub elapsed_ms: u64,
    pub session_id: Option<SessionId>,
    pub identity: ObservationIdentity,
    pub size_bytes: u64,
    pub reason: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum BridgeEvent {
    EffectIntent(EffectIntent),
    EffectNotDispatched(EffectNotDispatched),
    RequestNotDispatched(RequestNotDispatched),
    StopDecided(StopDecided),
    ObservationOmitted(ObservationOmitted),
}

pub fn decode_bridge_event(kind: &str, json: &str) -> Result<BridgeEvent, ReadError> {
    require_version(json, "bridge control", BRIDGE_SCHEMA_VERSION)?;
    let value: serde_json::Value = serde_json::from_str(json).map_err(ReadError::malformed)?;
    let event: BridgeEvent =
        serde_json::from_value(serde_json::json!({"kind": kind, "data": value}))
            .map_err(ReadError::malformed)?;
    match &event {
        BridgeEvent::EffectIntent(intent) => {
            if intent.effect_kind.is_empty()
                || (intent.effect_kind == "model"
                    && (intent.maximum_tokens.is_none()
                        || intent.request_id.is_none()
                        || intent.session_id.is_none()))
            {
                return Err(ReadError::malformed(
                    "effect intent is missing required identity/allowance",
                ));
            }
            if intent.effect_kind != "model" && intent.maximum_tokens.is_some() {
                return Err(ReadError::malformed(
                    "only model intents reserve generation",
                ));
            }
        }
        BridgeEvent::EffectNotDispatched(fact)
            if fact.identity.effect_id != Some(fact.effect_id) =>
        {
            return Err(ReadError::malformed(
                "non-dispatch identity disagrees with effect",
            ));
        }
        BridgeEvent::ObservationOmitted(fact)
            if fact.observation_kind.is_empty() || fact.reason.is_empty() =>
        {
            return Err(ReadError::malformed(
                "omission must identify its source kind and cause",
            ));
        }
        _ => {}
    }
    Ok(event)
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ObservationSourceMap {
    pub schema_version: u32,
    pub run_id: RunId,
    pub entries: Vec<ObservationLocation>,
}

pub fn decode_source_map(json: &str) -> Result<ObservationSourceMap, ReadError> {
    require_version(json, "observation source map", SOURCE_MAP_VERSION)?;
    let map: ObservationSourceMap = serde_json::from_str(json).map_err(ReadError::malformed)?;
    let mut previous = (0, 0);
    for entry in &map.entries {
        if entry.controller_sequence.get() <= previous.0
            || entry.archive_sequence.get() <= previous.1
        {
            return Err(ReadError::malformed(
                "source map entries must increase in both sequence domains",
            ));
        }
        previous = (
            entry.controller_sequence.get(),
            entry.archive_sequence.get(),
        );
    }
    Ok(map)
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "availability", rename_all = "snake_case")]
pub enum FileContent {
    Retained { content: ContentRef },
    Unavailable { reason: String },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FileVersion {
    /// A logical workspace name, never a host path to open.
    pub path: String,
    pub size_bytes: u64,
    pub mode: u32,
    pub entry_type: String,
    pub sha256: Option<Sha256Digest>,
    pub link: Option<String>,
    pub content: FileContent,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FileChange {
    pub path: String,
    pub change: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MemoryContent {
    pub status: String,
    /// Exact delivered text, even when empty because the source was missing.
    pub content: ContentRef,
}

impl MemoryContent {
    pub fn memory_ref(&self) -> crate::events::MemoryRef {
        match self.status.as_str() {
            "loaded" => crate::events::MemoryRef::Available {
                content: self.content.clone(),
            },
            "missing" => crate::events::MemoryRef::Missing,
            _ => crate::events::MemoryRef::Unavailable {
                reason: self.status.clone(),
            },
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EvidenceContent {
    pub text: ContentRef,
    pub provenance: ContentRef,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionDelivery {
    pub memento: MemoryContent,
    pub evidence: Option<EvidenceContent>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CheckpointIndex {
    pub schema_version: u32,
    pub run_id: RunId,
    pub session_id: SessionId,
    pub ordinal: NonZeroU32,
    pub workspace: ContentRef,
    pub files: Vec<FileVersion>,
    pub changes: Vec<FileChange>,
    pub incoming_memory: MemoryContent,
    pub outgoing_memory: MemoryContent,
    pub incoming_evidence: Option<EvidenceContent>,
    pub outgoing_evidence: Option<EvidenceContent>,
    pub source_map: ContentRef,
}

impl CheckpointIndex {
    /// Only these defined leaves are reachable. JSON inside a leaf is never
    /// recursively searched for paths or content-like objects.
    pub fn content_leaves(&self) -> Vec<&ContentRef> {
        let mut references = vec![
            &self.workspace,
            &self.incoming_memory.content,
            &self.outgoing_memory.content,
        ];
        for evidence in [&self.incoming_evidence, &self.outgoing_evidence]
            .into_iter()
            .flatten()
        {
            references.extend([&evidence.text, &evidence.provenance]);
        }
        for file in &self.files {
            if let FileContent::Retained { content } = &file.content {
                references.push(content);
            }
        }
        references
    }
}

pub fn decode_checkpoint_index(json: &str) -> Result<CheckpointIndex, ReadError> {
    require_version(json, "checkpoint index", CHECKPOINT_INDEX_VERSION)?;
    let index: CheckpointIndex = serde_json::from_str(json).map_err(ReadError::malformed)?;
    let mut paths = BTreeSet::new();
    for file in &index.files {
        if file.path.is_empty() || !paths.insert(&file.path) {
            return Err(ReadError::malformed(
                "checkpoint file names must be nonempty and unique",
            ));
        }
        if let FileContent::Retained { content } = &file.content
            && (file.link.is_some()
                || !matches!(file.entry_type.as_str(), "0" | "file")
                || file.sha256.as_ref() != Some(&content.sha256)
                || file.size_bytes != content.size_bytes)
        {
            return Err(ReadError::malformed(
                "only regular files with matching size/digest can retain content",
            ));
        }
    }
    Ok(index)
}
