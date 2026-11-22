//! Tool vocabulary and execution budgets, independent of conversation compaction.
use crate::ToolPolicy;
use playtime_core::{CommonLimits, CommonParameters, InvalidValue, WorkshopLimits};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolProfile {
    #[default]
    Workshop,
    HostV1,
    /// The workshop vocabulary, described for the per-command native sandbox at
    /// the checkout's host path.
    NativeV1,
}
impl ToolProfile {
    pub(crate) fn is_workshop(&self) -> bool {
        *self == Self::Workshop
    }
    pub fn tools(self, allow_finish_run: bool) -> Vec<Value> {
        match self {
            Self::Workshop => crate::compaction::tools(allow_finish_run),
            Self::HostV1 => serde_json::from_str(include_str!("host-tools-v1.json"))
                .expect("embedded host tool contract"),
            Self::NativeV1 => serde_json::from_str(include_str!("native-tools-v1.json"))
                .expect("embedded native tool contract"),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionConfig {
    pub tool_profile: ToolProfile,
    pub tool_policy: ToolPolicy,
    pub sampling_seed: u32,
    pub limits: CommonLimits,
    /// None for unrestricted host coding. Present for workshop/native worker
    /// allocations and for exact round trips of legacy conversation records.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub workshop: Option<WorkshopLimits>,
}
impl ExecutionConfig {
    pub fn workshop(parameters: CommonParameters, tool_policy: ToolPolicy) -> Self {
        Self {
            tool_profile: ToolProfile::Workshop,
            tool_policy,
            sampling_seed: parameters.sampling_seed,
            limits: parameters.limits,
            workshop: Some(parameters.workshop),
        }
    }
    pub fn validate(&self) -> Result<(), InvalidValue> {
        self.limits.validate()?;
        self.tool_policy.validate(&self.limits)?;
        if self.tool_profile != ToolProfile::HostV1 && self.workshop.is_none() {
            return Err(InvalidValue("workshop execution needs resource limits"));
        }
        self.controller_parameters().validate()
    }
    /// The shared archive controller still carries a workshop allocation. Host
    /// adapters do not consume it: only the budgets above govern their commands.
    /// Keep this compatibility value at the controller boundary, outside host
    /// configuration, admission and capability descriptions.
    pub fn controller_parameters(&self) -> CommonParameters {
        CommonParameters {
            sampling_seed: self.sampling_seed,
            limits: self.limits.clone(),
            workshop: self.workshop.clone().unwrap_or_else(|| WorkshopLimits {
                cpu_cores: 1.0,
                memory_bytes: (4 * 1024 * 1024).try_into().unwrap(),
                workspace_bytes: (1024 * 1024).try_into().unwrap(),
                tmp_bytes: (1024 * 1024).try_into().unwrap(),
                pids: 1.try_into().unwrap(),
            }),
        }
    }
}
