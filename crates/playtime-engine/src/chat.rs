//! Standalone chat inputs and completed-turn continuation. The same controller
//! still owns inference, tool admission, compaction, deadlines and cleanup.

use playtime_core::{CommonParameters, InvalidValue, RunMode, Sha256Digest};
pub use playtime_protocol::conversation::ConversationCheckpoint;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    RuntimeConfig, ToolPolicy,
    compaction::{CompactionSettings, PolicyConfig},
};

pub const MAX_CHECKPOINT_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_USER_BYTES: usize = 256 * 1024;
pub const HARNESS: &str = "compacting_chat_v1";

pub struct ConversationTurn {
    pub input: String,
    pub previous: Option<ConversationCheckpoint>,
}

/// `model` binds the exact backend/model/template/options; `workspace` binds
/// the workspace identity and tool access policy. Adapters verify these frozen
/// identities before execution. A display name is not an identity lock.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConversationConfig {
    pub schema_version: u32,
    pub model: Sha256Digest,
    pub workspace: Sha256Digest,
    pub instructions: String,
    pub inventory: String,
    pub mode: RunMode,
    pub tool_policy: ToolPolicy,
    pub parameters: CommonParameters,
    pub compaction: CompactionSettings,
}

pub(crate) struct Boundary {
    pub configuration: Sha256Digest,
    pub next_sequence: u64,
    pub request_offset: u64,
}

impl ConversationConfig {
    pub fn validate(&self) -> Result<(), InvalidValue> {
        self.parameters.validate()?;
        self.compaction.validate(&self.parameters)?;
        self.tool_policy.validate(&self.parameters.limits)?;
        if self.schema_version != 1
            || self.instructions.trim().is_empty()
            || self.instructions.len() > MAX_USER_BYTES
            || self.inventory.len() > MAX_USER_BYTES
            || self.compaction.allow_finish_run
            || (self.mode == RunMode::Simulation) != (self.tool_policy == ToolPolicy::Simulation)
        {
            return Err(InvalidValue(
                "invalid chat configuration; use plain final replies and a tool policy matching the execution mode",
            ));
        }
        Ok(())
    }

    pub fn fingerprint(&self) -> Result<Sha256Digest, InvalidValue> {
        self.validate()?;
        // Include the tool contract so an implementation update cannot silently
        // resume an older conversation with different capabilities.
        let bytes = serde_json::to_vec(&(HARNESS, self, crate::compaction::tools(false)))
            .map_err(|_| InvalidValue("chat configuration serialization failed"))?;
        Sha256Digest::new(
            Sha256::digest(bytes)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>(),
        )
    }

    pub fn validate_checkpoint(&self, saved: &ConversationCheckpoint) -> Result<(), InvalidValue> {
        if saved.schema_version != 1
            || saved.configuration != self.fingerprint()?
            || saved.next_sequence == 0
            || saved.next_request == 0
            || saved.next_request > saved.next_sequence
            || saved.history.compactions >= saved.next_sequence
            || saved
                .next_sequence
                .checked_add(self.parameters.limits.log_bytes.get())
                .and_then(|n| n.checked_add(65536))
                .is_none()
            || serde_json::to_vec(saved)
                .map_err(|_| InvalidValue("invalid chat checkpoint"))?
                .len()
                > MAX_CHECKPOINT_BYTES
        {
            return Err(InvalidValue("incompatible or oversized chat checkpoint"));
        }
        crate::policy::validate_chat_history(self, &saved.history)
    }

    pub(crate) fn runtime(
        &self,
        input: &str,
        saved: Option<&ConversationCheckpoint>,
    ) -> Result<RuntimeConfig, InvalidValue> {
        self.validate()?;
        if input.trim().is_empty() || input.len() > MAX_USER_BYTES {
            return Err(InvalidValue(
                "chat input must be nonempty and at most 256 KiB",
            ));
        }
        if let Some(saved) = saved {
            self.validate_checkpoint(saved)?;
        }
        Ok(RuntimeConfig {
            parameters: self.parameters.clone(),
            initial_workspace: None,
            memento: None,
            workspace: None,
            compaction: Some(PolicyConfig {
                parameters: self.parameters.clone(),
                protocol: self.instructions.clone(),
                prompt: saved
                    .map_or(input, |s| s.history.root[1]["content"].as_str().unwrap())
                    .into(),
                inventory: self.inventory.clone(),
                settings: self.compaction.clone(),
            }),
            conversation: Some(Boundary {
                configuration: self.fingerprint()?,
                next_sequence: saved.map_or(1, |s| s.next_sequence),
                request_offset: saved.map_or(0, |s| s.next_request - 1),
            }),
        })
    }
}
