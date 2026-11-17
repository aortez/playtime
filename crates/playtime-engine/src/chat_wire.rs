//! Version-one bytes remain stable for saved session fingerprints. New host
//! sessions use version two, which records execution separately from compaction.
use crate::{
    ToolPolicy,
    chat::ConversationConfig,
    compaction::CompactionSettings,
    execution::{ExecutionConfig, ToolProfile},
};
use playtime_core::{CommonParameters, RunMode, Sha256Digest};
use serde::{Deserialize, Serialize};
use std::num::NonZeroU64;

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LegacyCompaction {
    schema_version: u32,
    trigger_percent: u8,
    recent_exchanges: u8,
    summary_tokens: NonZeroU64,
    instructions: String,
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    allow_finish_run: bool,
    #[serde(default, skip_serializing_if = "ToolProfile::is_workshop")]
    tool_profile: ToolProfile,
}
fn yes() -> bool {
    true
}
fn is_true(value: &bool) -> bool {
    *value
}
impl LegacyCompaction {
    pub(crate) fn new(settings: &CompactionSettings, tool_profile: ToolProfile) -> Self {
        Self {
            schema_version: settings.schema_version,
            trigger_percent: settings.trigger_percent,
            recent_exchanges: settings.recent_exchanges,
            summary_tokens: settings.summary_tokens,
            instructions: settings.instructions.clone(),
            allow_finish_run: settings.allow_finish_run,
            tool_profile,
        }
    }
    fn settings(&self) -> CompactionSettings {
        CompactionSettings {
            schema_version: self.schema_version,
            trigger_percent: self.trigger_percent,
            recent_exchanges: self.recent_exchanges,
            summary_tokens: self.summary_tokens,
            instructions: self.instructions.clone(),
            allow_finish_run: self.allow_finish_run,
        }
    }
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Legacy {
    schema_version: u32,
    model: Sha256Digest,
    workspace: Sha256Digest,
    instructions: String,
    inventory: String,
    mode: RunMode,
    tool_policy: ToolPolicy,
    parameters: CommonParameters,
    compaction: LegacyCompaction,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Current {
    schema_version: u32,
    model: Sha256Digest,
    workspace: Sha256Digest,
    instructions: String,
    inventory: String,
    mode: RunMode,
    execution: ExecutionConfig,
    compaction: CompactionSettings,
}
#[derive(Deserialize, Serialize)]
#[serde(untagged)]
pub(crate) enum Wire {
    Legacy(Legacy),
    Current(Current),
}
impl From<ConversationConfig> for Wire {
    fn from(c: ConversationConfig) -> Self {
        if c.schema_version == 1 {
            Self::Legacy(Legacy {
                schema_version: c.schema_version,
                model: c.model,
                workspace: c.workspace,
                instructions: c.instructions,
                inventory: c.inventory,
                mode: c.mode,
                tool_policy: c.execution.tool_policy,
                parameters: c.execution.controller_parameters(),
                compaction: LegacyCompaction::new(&c.compaction, c.execution.tool_profile),
            })
        } else {
            Self::Current(Current {
                schema_version: c.schema_version,
                model: c.model,
                workspace: c.workspace,
                instructions: c.instructions,
                inventory: c.inventory,
                mode: c.mode,
                execution: c.execution,
                compaction: c.compaction,
            })
        }
    }
}
impl TryFrom<Wire> for ConversationConfig {
    type Error = &'static str;
    fn try_from(wire: Wire) -> Result<Self, Self::Error> {
        match wire {
            Wire::Legacy(c) if c.schema_version == 1 => {
                let mut execution = ExecutionConfig::workshop(c.parameters, c.tool_policy);
                execution.tool_profile = c.compaction.tool_profile;
                Ok(Self {
                    schema_version: 1,
                    model: c.model,
                    workspace: c.workspace,
                    instructions: c.instructions,
                    inventory: c.inventory,
                    mode: c.mode,
                    execution,
                    compaction: c.compaction.settings(),
                })
            }
            Wire::Current(c) if c.schema_version == 2 => Ok(Self {
                schema_version: 2,
                model: c.model,
                workspace: c.workspace,
                instructions: c.instructions,
                inventory: c.inventory,
                mode: c.mode,
                execution: c.execution,
                compaction: c.compaction,
            }),
            _ => Err("conversation fields do not match their schema version"),
        }
    }
}
