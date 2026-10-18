//! Bounded, unextracted workspace candidates. None of these types certify a
//! storage commit. Production publication belongs to the runner's writer.
mod tar;

use crate::{CancellationToken, Error, Failure, Result};
use playtime_protocol::storage::StorageLimits;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const EXPORTER: &str = include_str!("exporter.py");
pub const MEMORY_READER: &str = include_str!("memory.py");
pub fn helper_digests() -> BTreeMap<String, String> {
    [
        ("workspace-exporter-v1", EXPORTER),
        ("outgoing-memory-v1", MEMORY_READER),
    ]
    .into_iter()
    .map(|(name, source)| (name.into(), crate::types::hash(source.as_bytes())))
    .collect()
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CaptureLimits {
    pub entries: u64,
    pub tar_bytes: u64,
    pub file_bytes: u64,
    pub retained_bytes: u64,
    pub index_bytes: u64,
    pub memory_bytes: u64,
}
impl Default for CaptureLimits {
    fn default() -> Self {
        Self {
            entries: 4096,
            tar_bytes: 64 << 20,
            file_bytes: 64 << 20,
            retained_bytes: 64 << 20,
            index_bytes: 4 << 20,
            memory_bytes: 65536,
        }
    }
}
/// Cumulative already-retained inputs/checkpoints PLUS reserved publication
/// overhead (source map, incoming memory/evidence, canonical checkpoint index,
/// event envelopes, etc.). The integrating owner must include both. No credit
/// for deduplication is assumed. Refresh only from authoritative writer state.
#[derive(Clone, Copy, Debug, Default)]
pub struct ArchiveUsage {
    pub content_bytes: u64,
    pub references: u64,
    pub events: u64,
    pub log_bytes: u64,
}
/// Frozen preparation, with lower workspace and archive capacities applied.
/// This is not a reservation or a promise every allowed tree will fit.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct CaptureProfile {
    limits: CaptureLimits,
    remaining_bytes: u64,
    remaining_references: u64,
}
impl CaptureProfile {
    pub fn prepare(
        mut limits: CaptureLimits,
        workspace_bytes: u64,
        storage: &StorageLimits,
        usage: ArchiveUsage,
    ) -> Result<Self> {
        let max = CaptureLimits::default();
        if workspace_bytes == 0
            || limits.entries == 0
            || limits.entries > max.entries
            || limits.tar_bytes < 10240
            || limits.tar_bytes > max.tar_bytes
            || limits.file_bytes == 0
            || limits.file_bytes > max.file_bytes
            || limits.retained_bytes == 0
            || limits.retained_bytes > max.retained_bytes
            || limits.index_bytes == 0
            || limits.index_bytes > max.index_bytes
            || limits.memory_bytes == 0
            || limits.memory_bytes > max.memory_bytes
            || storage.record_bytes == 0
            || storage.record_bytes > storage.batch_bytes
            || storage.record_bytes > storage.page_bytes
            || storage.manifest_bytes > storage.content_bytes
            || usage.events >= storage.events
            || usage.log_bytes >= storage.event_log_bytes
        {
            return Err(invalid("invalid capture/storage preparation"));
        }
        limits.entries = limits.entries.min(storage.references);
        limits.tar_bytes = limits.tar_bytes.min(storage.content_bytes);
        limits.file_bytes = limits
            .file_bytes
            .min(workspace_bytes)
            .min(storage.content_bytes);
        limits.retained_bytes = limits.retained_bytes.min(workspace_bytes);
        limits.index_bytes = limits
            .index_bytes
            .min(storage.manifest_bytes)
            .min(storage.content_bytes);
        limits.memory_bytes = limits.memory_bytes.min(storage.content_bytes);
        let remaining_bytes = storage
            .verified_content_bytes
            .checked_sub(usage.content_bytes)
            .ok_or_else(capacity)?;
        let remaining_references = storage
            .references
            .checked_sub(usage.references)
            .ok_or_else(capacity)?;
        if limits.tar_bytes < 10240
            || limits.file_bytes == 0
            || limits.index_bytes == 0
            || limits.memory_bytes == 0
            || remaining_bytes < 10240
            || remaining_references < 3
        {
            return Err(capacity());
        }
        Ok(Self {
            limits,
            remaining_bytes,
            remaining_references,
        })
    }
    pub fn limits(&self) -> &CaptureLimits {
        &self.limits
    }
    pub(crate) fn payload(&self) -> Vec<u8> {
        serde_json::to_vec(
            &serde_json::json!({"entries":self.limits.entries,"tar_bytes":self.limits.tar_bytes,
            "file_bytes":self.limits.file_bytes,"retained_bytes":self.limits.retained_bytes}),
        )
        .expect("capture limits")
    }
    pub(crate) fn capacity(&self, bytes: u64, references: u64) -> Result<()> {
        if bytes > self.remaining_bytes || references > self.remaining_references {
            Err(capacity())
        } else {
            Ok(())
        }
    }
}
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct CapturedEntry {
    pub path: String,
    pub entry_type: String,
    pub mode: u32,
    pub size_bytes: u64,
    pub sha256: Option<String>,
    pub link: Option<String>,
    /// Controller-generated in-memory staging slot; never a workspace path.
    pub content_slot: Option<usize>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OutgoingMemory {
    pub status: String,
    pub content: String,
}
impl OutgoingMemory {
    pub(crate) fn decode(bytes: &[u8], limit: u64) -> Result<Self> {
        if bytes.len() > crate::bridge::RESPONSE_BYTES {
            return Err(capacity());
        }
        let memory: Self =
            serde_json::from_slice(bytes).map_err(|_| malformed("invalid memory reply"))?;
        if memory.content.len() as u64 > limit
            || match memory.status.as_str() {
                "loaded" => memory
                    .content
                    .bytes()
                    .any(|b| (b < 32 && !b"\t\n\r\x0c".contains(&b)) || b == 127),
                "missing" | "unreadable" | "oversized" | "binary" => !memory.content.is_empty(),
                _ => true,
            }
        {
            return Err(malformed("invalid memory status/content"));
        }
        Ok(memory)
    }
}
/// Pure validation result. The tar and bodies are bounded owned staging bytes;
/// no extraction, filesystem names, or live/committed frontier are involved.
#[derive(Debug)]
pub struct ValidatedWorkspace {
    tar: Vec<u8>,
    sha256: String,
    entries: Vec<CapturedEntry>,
    bodies: Vec<Vec<u8>>,
    index: Vec<u8>,
    retained_bytes: u64,
}
impl ValidatedWorkspace {
    pub fn tar(&self) -> &[u8] {
        &self.tar
    }
    pub fn sha256(&self) -> &str {
        &self.sha256
    }
    pub fn entries(&self) -> &[CapturedEntry] {
        &self.entries
    }
    pub fn content(&self, slot: usize) -> Option<&[u8]> {
        self.bodies.get(slot).map(Vec::as_slice)
    }
    pub fn index(&self) -> &[u8] {
        &self.index
    }
}
/// Created only after complete zero-exit transport and holder revalidation.
/// Still a candidate, never a writer receipt or permission to advertise results.
#[derive(Debug)]
pub struct SnapshotCandidate {
    pub(crate) workspace: ValidatedWorkspace,
    pub(crate) memory: OutgoingMemory,
    pub(crate) helpers: BTreeMap<String, String>,
}
impl SnapshotCandidate {
    pub fn workspace(&self) -> &ValidatedWorkspace {
        &self.workspace
    }
    pub fn memory(&self) -> &OutgoingMemory {
        &self.memory
    }
    pub fn helpers(&self) -> &BTreeMap<String, String> {
        &self.helpers
    }
    pub(crate) fn finish(
        workspace: ValidatedWorkspace,
        memory: OutgoingMemory,
        profile: &CaptureProfile,
    ) -> Result<Self> {
        // The separate memory read must agree with the captured file, including
        // missing versus empty and exact bytes. A changed memento cannot turn
        // two independently valid reads into a successful candidate.
        let entry = workspace.entries.iter().find(|e| e.path == "MEMENTO.md");
        let content_entry = match entry {
            Some(e) if e.entry_type == "1" => e
                .link
                .as_ref()
                .and_then(|path| workspace.entries.iter().find(|e| &e.path == path)),
            _ => entry,
        };
        let body = content_entry
            .and_then(|e| e.content_slot)
            .and_then(|slot| workspace.content(slot));
        let expected = match body {
            Some(bytes) => memory_status(bytes, profile.limits.memory_bytes as usize),
            None if entry.is_none() => "missing",
            None => "unreadable",
        };
        if memory.status != expected
            || (expected == "loaded" && Some(memory.content.as_bytes()) != body)
        {
            return Err(malformed("memory changed between capture and read"));
        }
        let bytes = (workspace.tar.len() as u64)
            .checked_add(workspace.retained_bytes)
            .and_then(|n| n.checked_add(workspace.index.len() as u64))
            .and_then(|n| n.checked_add(memory.content.len() as u64))
            .ok_or_else(capacity)?;
        profile.capacity(
            bytes,
            (workspace.bodies.len() as u64)
                .checked_add(3)
                .ok_or_else(capacity)?,
        )?;
        Ok(Self {
            workspace,
            memory,
            helpers: helper_digests(),
        })
    }
}
/// Streaming strict-subset parser. A valid archive alone is not a successful
/// Docker capture; only Quiescent::capture can supply a SnapshotCandidate.
pub fn validate_workspace(
    reader: impl std::io::Read,
    profile: &CaptureProfile,
    cancel: &CancellationToken,
) -> Result<ValidatedWorkspace> {
    tar::validate(reader, profile, cancel)
}
pub(crate) fn invalid(detail: &'static str) -> Error {
    Error::new(Failure::InvalidInput, "capture", detail)
}
pub(crate) fn malformed(detail: &'static str) -> Error {
    Error::new(Failure::Malformed, "capture", detail)
}
pub(crate) fn capacity() -> Error {
    Error::new(
        Failure::Capacity,
        "capture",
        "capture or remaining archive capacity exceeded",
    )
}

fn memory_status(bytes: &[u8], limit: usize) -> &'static str {
    let text_sample = |bound: usize| {
        let sample = &bytes[..bytes.len().min(bound)];
        !sample
            .iter()
            .any(|b| (*b < 32 && !b"\t\n\r\x0c".contains(b)) || *b == 127)
            && match std::str::from_utf8(sample) {
                Ok(_) => true,
                Err(e) => e.error_len().is_none() && sample.len() < bytes.len(),
            }
    };
    if !text_sample(4096) || !text_sample(limit + 1) {
        "binary"
    } else if bytes.len() > limit {
        "oversized"
    } else {
        "loaded"
    }
}
