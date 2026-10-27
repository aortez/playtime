//! Frozen configuration for the first continuous conversation policy.
//! Native preparation freezes this configuration before worker admission.

use std::num::NonZeroU64;

use playtime_core::{CommonParameters, InvalidValue};
use playtime_protocol::archive::ContentRef;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{FileIndex, context::encoded_size};

pub const HARNESS: &str = "continuous_compaction_v1";
pub const FINISH_SUMMARY_BYTES: usize = 8192;
pub const DEFAULT_INSTRUCTIONS: &str = playtime_protocol::api::DEFAULT_COMPACTION_INSTRUCTIONS;
/// Consecutive parse failures of work requests that are retried; one more
/// stops the run.
pub const TOOL_PARSE_RETRIES: u32 = 5;
pub(crate) const MIN_TOOL_RESULT_BYTES: u64 = 512;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CompactionSettings {
    pub schema_version: u32,
    pub trigger_percent: u8,
    pub recent_exchanges: u8,
    pub summary_tokens: NonZeroU64,
    pub instructions: String,
    /// Without finish_run, the run lasts until its time or token limit. Omitted
    /// when true, so configurations frozen before this setting keep their bytes.
    #[serde(
        default = "default_allow_finish_run",
        skip_serializing_if = "is_default_allow_finish_run"
    )]
    pub allow_finish_run: bool,
}
fn default_allow_finish_run() -> bool {
    true
}
fn is_default_allow_finish_run(allow: &bool) -> bool {
    *allow
}

impl Default for CompactionSettings {
    fn default() -> Self {
        Self {
            schema_version: 1,
            trigger_percent: 75,
            recent_exchanges: 2,
            summary_tokens: NonZeroU64::new(2048).unwrap(),
            instructions: DEFAULT_INSTRUCTIONS.into(),
            allow_finish_run: true,
        }
    }
}

/// The authored API settings. `validate` still checks them against the run.
impl TryFrom<&playtime_protocol::api::CompactionSettings> for CompactionSettings {
    type Error = InvalidValue;
    fn try_from(
        settings: &playtime_protocol::api::CompactionSettings,
    ) -> Result<Self, InvalidValue> {
        Ok(Self {
            schema_version: settings.schema_version,
            trigger_percent: settings.trigger_percent,
            recent_exchanges: settings.recent_exchanges,
            summary_tokens: NonZeroU64::new(settings.summary_tokens)
                .ok_or(InvalidValue("summary cap must be positive"))?,
            instructions: settings.instructions.clone(),
            allow_finish_run: settings.allow_finish_run,
        })
    }
}

impl CompactionSettings {
    pub fn validate(&self, parameters: &CommonParameters) -> Result<(), InvalidValue> {
        parameters.validate()?;
        if self.schema_version != 1 {
            return Err(InvalidValue("unsupported compaction settings version"));
        }
        if !(1..=99).contains(&self.trigger_percent) || self.recent_exchanges > 32 {
            return Err(InvalidValue(
                "compaction requires trigger 1..99 and retention 0..32",
            ));
        }
        if self.summary_tokens > parameters.limits.tokens_per_call {
            return Err(InvalidValue(
                "summary cap exceeds the accepted per-call allocation",
            ));
        }
        if self.instructions.trim().is_empty() || self.instructions.len() > 65536 {
            return Err(InvalidValue(
                "compaction instructions must contain 1..65536 UTF-8 bytes",
            ));
        }
        Ok(())
    }

    pub fn safety_margin(context_tokens: u64) -> u64 {
        context_tokens.div_ceil(20).max(1024)
    }

    pub(crate) fn threshold(&self, context_tokens: u64) -> u64 {
        ((u128::from(context_tokens) * u128::from(self.trigger_percent)).div_ceil(100)) as u64
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CompactionConfig {
    pub parameters: CommonParameters,
    pub protocol: String,
    pub prompt: String,
    pub inventory: String,
    pub settings: CompactionSettings,
    pub initial_workspace: ContentRef,
    pub initial_files: FileIndex,
}

impl CompactionConfig {
    pub(crate) fn work_reserve(&self) -> u64 {
        CompactionSettings::safety_margin(self.parameters.limits.context_tokens.get())
            .saturating_add(self.settings.summary_tokens.get())
            .saturating_add(encoded_size(
                &json!({"role":"user", "content":self.settings.instructions}),
            ))
            .saturating_add(self.parameters.limits.output_bytes.get())
    }
}

pub(crate) fn tools(allow_finish_run: bool) -> Vec<Value> {
    let mut tools = crate::policy::memento_contract::TOOLS[..3].to_vec();
    tools[0]["function"]["description"] = json!(
        "Execute a bounded Bash command in the owned workshop. Processes continue through conversation compaction and stop when the run ends."
    );
    if !allow_finish_run {
        return tools;
    }
    tools.push(json!({
        "type":"function",
        "function": {
            "name":"finish_run",
            "description":"Declare the experiment finished. Call this alone, with a nonempty summary of at most 8192 UTF-8 bytes. The controller then stops, captures and cleans up the workshop.",
            "parameters": {
                "type":"object",
                "properties":{"summary":{"type":"string"}},
                "required":["summary"],
                "additionalProperties":false
            }
        }
    }));
    tools
}

pub(crate) enum Batch {
    /// Every call may run. A finish declaration carries its summary.
    Valid(Option<String>),
    /// An ordinary call is invalid, so no call runs and each gets an error result.
    Rejected,
}

pub(crate) fn validate_tools(
    message: &Value,
    finish_reason: Option<&str>,
    result_allowance: u64,
    allow_finish_run: bool,
) -> Result<Batch, (playtime_core::StopReason, String)> {
    use playtime_core::StopReason;
    let Some(calls) = message["tool_calls"].as_array() else {
        return Ok(Batch::Valid(None));
    };
    // Without the tool, a finish_run call is an ordinary invalid call.
    let has_finish = allow_finish_run
        && calls
            .iter()
            .any(|call| call["function"]["name"] == "finish_run");
    if has_finish && (calls.len() != 1 || finish_reason != Some("stop")) {
        return Err((
            StopReason::InvalidCompletion,
            "finish_run requires a normally completed response containing only that call".into(),
        ));
    }
    if !has_finish && calls.len() as u64 * MIN_TOOL_RESULT_BYTES > result_allowance {
        return Err((
            StopReason::ContextLimit,
            "tool batch exceeds the aggregate result allowance".into(),
        ));
    }
    for call in calls {
        let name = call["function"]["name"].as_str().unwrap_or("");
        let arguments = &call["function"]["arguments"];
        if let Some(error) =
            crate::validation::validate_call(&tools(allow_finish_run), name, arguments)
        {
            if !has_finish {
                return Ok(Batch::Rejected);
            }
            return Err((StopReason::InvalidCompletion, error));
        }
        if name == "finish_run" {
            let summary = arguments["summary"].as_str().unwrap();
            if summary.trim().is_empty() || summary.len() > FINISH_SUMMARY_BYTES {
                return Err((
                    StopReason::InvalidCompletion,
                    "finish summary must contain non-whitespace text and fit 8192 UTF-8 bytes"
                        .into(),
                ));
            }
            return Ok(Batch::Valid(Some(summary.into())));
        }
    }
    Ok(Batch::Valid(None))
}
