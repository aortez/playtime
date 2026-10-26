//! Validated effect inputs shared by the two explicit native profile families.
use playtime_runner::docker::{CompletedResponse, NativeProfile, Profile};

#[derive(Clone)]
pub(crate) struct EffectProfile {
    pub parameters: playtime_core::CommonParameters,
    pub selection: playtime_docker::transport::Selection,
    pub daemon: playtime_docker::DaemonIdentity,
    pub image_lock: serde_json::Value,
    pub resources: playtime_docker::Limits,
    pub capture: playtime_docker::capture::CaptureLimits,
    pub storage: playtime_storage::StorageLimits,
    pub command_return_ms: u64,
    pub model: Option<playtime_runner::docker::LiveModel>,
    pub seeds: std::collections::BTreeMap<String, String>,
    pub models: Vec<playtime_engine::simulation::ModelScript>,
    pub continuous: bool,
    pub work_responses: Vec<CompletedResponse>,
    pub summary_responses: Vec<CompletedResponse>,
}
impl From<Profile> for EffectProfile {
    fn from(p: Profile) -> Self {
        Self {
            parameters: p.scenario.parameters.common(),
            selection: p.selection,
            daemon: p.daemon,
            image_lock: p.image_lock,
            resources: p.resources,
            capture: p.capture,
            storage: p.storage,
            command_return_ms: p.command_return_ms,
            model: p.model,
            seeds: p.recipe.map(|r| r.seeds).unwrap_or_default(),
            models: p.scenario.models,
            continuous: false,
            work_responses: vec![],
            summary_responses: vec![],
        }
    }
}
impl From<NativeProfile> for EffectProfile {
    fn from(p: NativeProfile) -> Self {
        match p {
            NativeProfile::Memento(p) => (*p).into(),
            NativeProfile::Continuous(p) => Self {
                parameters: p.config.parameters,
                selection: p.selection,
                daemon: p.daemon,
                image_lock: p.image_lock,
                resources: p.resources,
                capture: p.capture,
                storage: p.storage,
                command_return_ms: p.command_return_ms,
                model: p.model,
                seeds: p.seeds,
                models: vec![],
                continuous: true,
                work_responses: p.work_responses,
                summary_responses: p.summary_responses,
            },
        }
    }
}
