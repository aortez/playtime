use std::num::{NonZeroU32, NonZeroU64};

use serde::{Deserialize, Serialize};

use crate::{InvalidValue, MemoryCondition};

/// Shared allocations without memento session, memory, or handoff settings.
/// The legacy ResourceLimits representation below keeps its stored layout.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CommonLimits {
    pub wall_seconds: NonZeroU64,
    pub generated_tokens: NonZeroU64,
    pub tokens_per_call: NonZeroU64,
    pub context_tokens: NonZeroU64,
    pub model_seconds: NonZeroU64,
    pub command_seconds: NonZeroU64,
    pub output_bytes: NonZeroU64,
    pub log_bytes: NonZeroU64,
}

impl CommonLimits {
    pub fn validate(&self) -> Result<(), InvalidValue> {
        if !(1024..=65536).contains(&self.output_bytes.get()) {
            return Err(InvalidValue("output_bytes must be 1024..65536"));
        }
        if self.context_tokens <= self.tokens_per_call {
            return Err(InvalidValue(
                "context_tokens must leave room beyond tokens_per_call",
            ));
        }
        for seconds in [self.wall_seconds, self.model_seconds, self.command_seconds] {
            seconds
                .get()
                .checked_mul(1000)
                .ok_or(InvalidValue("timeout overflows milliseconds"))?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CommonParameters {
    pub sampling_seed: u32,
    pub limits: CommonLimits,
    pub workshop: WorkshopLimits,
}

impl CommonParameters {
    pub fn validate(&self) -> Result<(), InvalidValue> {
        if self.sampling_seed > i32::MAX as u32 {
            return Err(InvalidValue("sampling_seed must be at most 2147483647"));
        }
        self.limits.validate()?;
        self.workshop.validate()
    }
}

/// Requested configuration. Deserialization rejects misspelled settings;
/// validate() checks relationships before the values may be used for admission.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RunParameters {
    pub sampling_seed: u32,
    #[serde(default)]
    pub memory_condition: MemoryCondition,
    pub limits: ResourceLimits,
    pub workshop: WorkshopLimits,
}

impl RunParameters {
    pub fn common(&self) -> CommonParameters {
        CommonParameters {
            sampling_seed: self.sampling_seed,
            limits: self.limits.common(),
            workshop: self.workshop.clone(),
        }
    }
    pub fn validate(&self) -> Result<(), InvalidValue> {
        if self.sampling_seed > i32::MAX as u32 {
            return Err(InvalidValue("sampling_seed must be at most 2147483647"));
        }
        self.limits.validate()?;
        self.workshop.validate()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceLimits {
    pub sessions: NonZeroU32,
    pub work_turns_per_session: NonZeroU32,
    pub wall_seconds: NonZeroU64,
    pub generated_tokens: NonZeroU64,
    pub tokens_per_call: NonZeroU64,
    pub handoff_tokens: NonZeroU64,
    pub context_tokens: NonZeroU64,
    pub model_seconds: NonZeroU64,
    pub command_seconds: NonZeroU64,
    pub output_bytes: NonZeroU64,
    pub memento_bytes: NonZeroU64,
    pub log_bytes: NonZeroU64,
    /// Consecutive parse errors allowed; one more stops the run.
    #[serde(default)]
    pub tool_parse_retries: u32,
    /// Parse errors allowed in one session; reaching it ends that session
    /// through the normal handoff. Omitted means `tool_parse_retries + 1`.
    #[serde(
        default,
        deserialize_with = "present_error_limit",
        skip_serializing_if = "Option::is_none"
    )]
    pub tool_parse_error_limit: Option<NonZeroU32>,
}

fn present_error_limit<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<NonZeroU32>, D::Error> {
    // Omission selects the legacy default; an explicitly supplied null is a bad setting.
    NonZeroU32::deserialize(deserializer).map(Some)
}

impl ResourceLimits {
    pub fn common(&self) -> CommonLimits {
        CommonLimits {
            wall_seconds: self.wall_seconds,
            generated_tokens: self.generated_tokens,
            tokens_per_call: self.tokens_per_call,
            context_tokens: self.context_tokens,
            model_seconds: self.model_seconds,
            command_seconds: self.command_seconds,
            output_bytes: self.output_bytes,
            log_bytes: self.log_bytes,
        }
    }
    pub fn validate(&self) -> Result<(), InvalidValue> {
        if self.sessions.get() > 1000 || self.work_turns_per_session.get() > 1000 {
            return Err(InvalidValue(
                "sessions and work_turns_per_session must be at most 1000",
            ));
        }
        if self.generated_tokens <= self.handoff_tokens {
            return Err(InvalidValue(
                "generated_tokens must leave room beyond handoff_tokens",
            ));
        }
        if !(1024..=65536).contains(&self.output_bytes.get()) || self.memento_bytes.get() > 32768 {
            return Err(InvalidValue(
                "output_bytes must be 1024..65536 and memento_bytes at most 32768",
            ));
        }
        let reserve = self
            .tokens_per_call
            .get()
            .checked_add(self.memento_bytes.get())
            .and_then(|value| value.checked_add(4096))
            .ok_or(InvalidValue("configured input/output reserves overflow"))?;
        if self.context_tokens.get() < reserve {
            return Err(InvalidValue(
                "context_tokens is too small for configured input/output reserves",
            ));
        }
        if self.tool_parse_retries > 10
            || self
                .tool_parse_error_limit
                .is_some_and(|value| value.get() > 100)
        {
            return Err(InvalidValue(
                "tool_parse_retries must be at most 10 and tool_parse_error_limit at most 100",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkshopLimits {
    pub cpu_cores: f64,
    pub memory_bytes: NonZeroU64,
    pub workspace_bytes: NonZeroU64,
    pub tmp_bytes: NonZeroU64,
    pub pids: NonZeroU32,
}

impl WorkshopLimits {
    pub fn validate(&self) -> Result<(), InvalidValue> {
        if !self.cpu_cores.is_finite()
            || !(0.1..=64.0).contains(&self.cpu_cores)
            || self.pids.get() > 65536
        {
            return Err(InvalidValue(
                "cpu_cores must be finite and 0.1..64; pids must be at most 65536",
            ));
        }
        if [self.memory_bytes, self.workspace_bytes, self.tmp_bytes]
            .iter()
            .any(|value| !(1048576..=68719476736).contains(&value.get()))
        {
            return Err(InvalidValue("workshop byte limits must be 1 MiB..64 GiB"));
        }
        let storage = self
            .workspace_bytes
            .get()
            .checked_add(self.tmp_bytes.get())
            .ok_or(InvalidValue("workshop storage limits overflow"))?;
        if self.memory_bytes.get() <= storage {
            return Err(InvalidValue(
                "memory_bytes must leave process memory beyond workspace_bytes and tmp_bytes",
            ));
        }
        Ok(())
    }
}
