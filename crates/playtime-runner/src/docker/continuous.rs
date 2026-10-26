//! Explicit native smoke capability. Recipe/API selection is a separate slice.
use super::*;
use playtime_engine::{
    ModelResponse,
    compaction::{CompactionConfig, CompactionSettings},
};
use serde_json::{Value, json};

pub const CONTINUOUS_PROFILE: &str = "docker_continuous_compaction_v1";
pub const PROBE: &str = include_str!("continuous-probe.sh");

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompletedResponse {
    pub response: ModelResponse,
    pub finish_reason: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContinuousProfile {
    pub version: u32,
    pub profile: String,
    pub harness: String,
    pub mode: RunMode,
    pub config: CompactionConfig,
    pub selection: Selection,
    pub daemon: DaemonIdentity,
    pub image_lock: Value,
    pub helpers: std::collections::BTreeMap<String, String>,
    pub resources: Limits,
    pub capture: CaptureLimits,
    pub storage: StorageLimits,
    pub command_return_ms: u64,
    pub model: Option<LiveModel>,
    pub seeds: std::collections::BTreeMap<String, String>,
    pub work_responses: Vec<CompletedResponse>,
    pub summary_responses: Vec<CompletedResponse>,
}

impl ContinuousProfile {
    pub fn freeze(
        selection: Selection,
        daemon: DaemonIdentity,
        model: Option<LiveModel>,
    ) -> Result<Self> {
        let base = Profile::freeze(selection, daemon)?;
        let mut parameters = base.scenario.parameters.common();
        parameters.limits.wall_seconds = 300.try_into().unwrap();
        parameters.limits.generated_tokens = 16384.try_into().unwrap();
        parameters.limits.context_tokens = 16384.try_into().unwrap();
        parameters.limits.model_seconds = 45.try_into().unwrap();
        parameters.limits.output_bytes = 2048.try_into().unwrap();
        parameters.limits.log_bytes = (16 << 20).try_into().unwrap();
        let config = CompactionConfig {
            parameters,
            protocol: "Work in the owned workshop using tools. Use exactly one tool call per response. Never finish before the probe reports stage 48 and continuity-ok. Only finish_run ends this run. Compaction preserves the workshop and running processes.".into(),
            prompt: "Repeatedly call exec with exactly these arguments: {\"command\":\"bash /work/seeds/continuous-probe.sh\",\"cwd\":\"/work\",\"timeout_seconds\":5}. The working directory is /work, never the script filename. This task requires 48 separate successful exec calls, one per response. Each call advances one stage. Continue until its output contains continuity-ok, then call finish_run alone with a short summary. Do not inspect or modify the script or other files. After compaction continue calling exec with those same arguments.".into(),
            inventory: "Bash and Python 3 are available. The probe is already installed.".into(),
            settings: CompactionSettings {
                summary_tokens: 512.try_into().unwrap(),
                instructions: "Summarize the checked progress in at most 300 characters. Preserve the task, next command and any uncertainty. Return only the summary; no tools.".into(),
                ..CompactionSettings::default()
            },
            initial_workspace: base.config().initial_workspace,
            initial_files: Default::default(),
        };
        let completed = |message| CompletedResponse {
            response: ModelResponse {
                message,
                done: true,
                generated_tokens: Some(32),
                prompt_tokens: None,
            },
            finish_reason: "stop".into(),
        };
        let work_responses = if model.is_none() {
            (0..48).map(|_| completed(json!({"role":"assistant","tool_calls":[{"function":{"name":"exec","arguments":{"command":"bash /work/seeds/continuous-probe.sh"}}}]})))
                .chain(std::iter::once(completed(json!({"role":"assistant","tool_calls":[{"function":{"name":"finish_run","arguments":{"summary":"The counter survived compaction and continuity-ok was recorded."}}}]}))))
                .collect()
        } else {
            Vec::new()
        };
        let summary_responses = if model.is_none() {
            (0..48).map(|_| completed(json!({"role":"assistant","content":"Continue calling bash /work/seeds/continuous-probe.sh until continuity-ok, then finish_run. The owned workshop and counter must persist."}))).collect()
        } else {
            Vec::new()
        };
        Ok(Self {
            version: 1,
            profile: CONTINUOUS_PROFILE.into(),
            harness: playtime_engine::compaction::HARNESS.into(),
            mode: if model.is_some() {
                RunMode::LiveSmoke
            } else {
                RunMode::ScriptedSmoke
            },
            config,
            selection: base.selection,
            daemon: base.daemon,
            image_lock: base.image_lock,
            helpers: base.helpers,
            resources: base.resources,
            capture: base.capture,
            storage: StorageLimits {
                event_log_bytes: 16 << 20,
                ..base.storage
            },
            command_return_ms: base.command_return_ms,
            model,
            seeds: [("continuous-probe.sh".into(), PROBE.into())].into(),
            work_responses,
            summary_responses,
        })
    }

    pub fn validate(&self) -> Result<()> {
        let expected = Self::freeze(
            self.selection.clone(),
            self.daemon.clone(),
            self.model.clone(),
        )?;
        if serde_json::to_value(self)? != serde_json::to_value(expected)?
            || !self.selection.executable.is_absolute()
            || !self.selection.socket.is_absolute()
            || self.selection.executable_sha256.len() != 64
            || !self
                .selection
                .executable_sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(Error::Invalid(
                "unsupported continuous native profile or bounds".into(),
            ));
        }
        self.config.settings.validate(&self.config.parameters)?;
        if let Some(model) = &self.model {
            if !model.host.valid() {
                return Err(Error::Invalid("invalid continuous model host".into()));
            }
            let identity = model.execution_model()?;
            let settings: playtime_ollama::EffectiveSettings =
                serde_json::from_value(model.settings.clone())?;
            let expected = playtime_ollama::EffectiveSettings {
                context_tokens: Some(self.config.parameters.limits.context_tokens.get()),
                thinking: settings.thinking.clone(),
                ..Default::default()
            };
            if settings != expected || !identity.reasoning.contains(&settings.thinking) {
                return Err(Error::Invalid(
                    "unsupported continuous smoke model settings".into(),
                ));
            }
        }
        Ok(())
    }

    pub(super) fn template(&self) -> Result<Value> {
        let mut value = serde_json::to_value(self)?;
        let fields = value.as_object_mut().unwrap();
        fields.remove("selection");
        fields.remove("daemon");
        if let Some(model) = fields.get_mut("model").and_then(Value::as_object_mut) {
            model.remove("host");
        }
        Ok(value)
    }
}

/// Decoding selects an explicit profile family; a v2 test probe is never native.
pub enum NativeProfile {
    Memento(Box<Profile>),
    Continuous(Box<ContinuousProfile>),
}
impl NativeProfile {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let value: Value = serde_json::from_slice(bytes)?;
        let profile = if value["profile"] == CONTINUOUS_PROFILE {
            Self::Continuous(Box::new(serde_json::from_value(value)?))
        } else {
            Self::Memento(Box::new(serde_json::from_value(value)?))
        };
        profile.validate()?;
        Ok(profile)
    }
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Memento(p) => p.validate(),
            Self::Continuous(p) => p.validate(),
        }
    }
    pub fn model(&self) -> Option<&LiveModel> {
        match self {
            Self::Memento(p) => p.model.as_ref(),
            Self::Continuous(p) => p.model.as_ref(),
        }
    }
    pub fn wall_seconds(&self) -> u64 {
        match self {
            Self::Memento(p) => p.scenario.parameters.limits.wall_seconds.get(),
            Self::Continuous(p) => p.config.parameters.limits.wall_seconds.get(),
        }
    }
    pub fn storage(&self) -> &StorageLimits {
        match self {
            Self::Memento(p) => &p.storage,
            Self::Continuous(p) => &p.storage,
        }
    }
    pub fn mode(&self) -> RunMode {
        match self {
            Self::Memento(p) => p.mode,
            Self::Continuous(p) => p.mode,
        }
    }
    pub fn manifest_version(&self) -> u32 {
        match self {
            Self::Memento(_) => 1,
            Self::Continuous(_) => 2,
        }
    }
}
