//! Versioned, intentionally narrow frozen Docker profiles: scripted model
//! responses, the live locked model reached directly over plain HTTP, or a
//! reviewed catalog recipe on the live model.
use crate::{Error, Result};
use playtime_core::{RunMode, Sha256Digest};
use playtime_docker::{DaemonIdentity, Limits, capture::CaptureLimits, transport::Selection};
use playtime_engine::{ControllerConfig, simulation::Scenario};
use playtime_storage::StorageLimits;
use serde::{Deserialize, Serialize};
use std::{net::IpAddr, path::PathBuf};

mod continuous;
mod definition;
pub use continuous::{CONTINUOUS_PROFILE, CompletedResponse, ContinuousProfile, NativeProfile};
mod harness;
pub use harness::harness_catalog;
mod model_settings;
pub use model_settings::reviewed_model_capabilities;
mod recipe;
mod registered_model;
pub use definition::{CompactionDefinition, FrozenRecipe};
pub use recipe::{BOUNDS, Bounds, RECIPE_PROFILE, Recipe, Sampling};
pub use registered_model::{QWEN_MODEL_LOCK, reviewed_identity_capabilities};

pub const PROFILE: &str = "docker_scripted_no_seed_self_v1";
pub const LIVE_PROFILE: &str = "docker_live_no_seed_self_v1";
/// Reserved inside each command's effect cap for the Docker calls around
/// `docker exec` and the bridge's own kill and reap, which can wait 2 s.
pub const RETURN_OVERHEAD_MS: u64 = 3000;
pub const RECIPE: &str = "docker-two-session";
pub const LIVE_RECIPE: &str = "live-two-session";
pub const SCENARIO: &[u8] = include_bytes!("smoke.json");
pub const LIVE_SCENARIO: &[u8] = include_bytes!("live.json");
pub const IMAGE_LOCK: &[u8] = include_bytes!("image-lock.json");
pub const MODEL_LOCK: &[u8] = include_bytes!("model-lock.json");

/// Digest of this build's compiled profiles, which its bundles advertise.
pub fn digest() -> Result<Sha256Digest> {
    // Templates exclude host bindings, so these placeholders never matter.
    let daemon = DaemonIdentity {
        id: String::new(),
        name: String::new(),
        cli_version: String::new(),
        server_version: String::new(),
        api_version: String::new(),
        kernel: String::new(),
    };
    let selection = Selection {
        executable: "/".into(),
        executable_sha256: String::new(),
        socket: "/".into(),
    };
    let host = LiveHost {
        endpoint: String::new(),
        addresses: vec![],
        live_lock: PathBuf::new(),
    };
    let templates = [
        Profile::freeze(selection.clone(), daemon.clone())?.template()?,
        Profile::freeze_live(selection.clone(), daemon.clone(), host.clone())?.template()?,
        Profile::freeze_recipe(
            Recipe::reference()?,
            selection.clone(),
            daemon.clone(),
            host,
        )?
        .template()?,
        ContinuousProfile::freeze(selection, daemon, None)?.template()?,
        serde_json::to_value(&BOUNDS)?,
        // A worker that predates authoring must not accept an authored profile
        // merely because its ordinary recipe template is unchanged, nor run a
        // continuous profile without tool-call or summary recovery, the finish
        // choice, transcript summaries or tool-call summary retries.
        serde_json::json!({"immutable_recipe_definition": 1, "recipe_model_settings": 1, "registered_model_execution": 1, "harness_selection": 1, "continuous_recipe": 1, "continuous_tool_recovery": 1, "continuous_finish_choice": 1, "continuous_summary_recovery": 1, "continuous_transcript_summary": 1, "continuous_summary_tool_recovery": 1}),
        serde_json::from_slice(QWEN_MODEL_LOCK)?,
        serde_json::to_value(CompactionDefinition::new(&Recipe::reference()?))?,
    ];
    Ok(playtime_storage::sha256(&serde_json::to_vec(&templates)?))
}

/// How a static worker reaches the live model. The daemon resolves the
/// endpoint itself and freezes the addresses, because musl can't resolve mDNS.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LiveHost {
    pub endpoint: String,
    pub addresses: Vec<IpAddr>,
    /// Machine-wide flock that every checkout's live worker holds for its lifetime.
    pub live_lock: PathBuf,
}
impl LiveHost {
    fn valid(&self) -> bool {
        let mut addresses = self.addresses.clone();
        addresses.sort();
        addresses.dedup();
        self.endpoint.starts_with("http://")
            && (1..=16).contains(&self.addresses.len())
            && addresses.len() == self.addresses.len()
            && self.live_lock.is_absolute()
    }
}

/// The exact model lock and request settings, plus the host they are sent to.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LiveModel {
    pub lock: serde_json::Value,
    pub settings: serde_json::Value,
    pub host: LiveHost,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registration: Option<playtime_protocol::api::ModelRegistrationSummary>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    /// Explicit for new profiles; historical archives can lack this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    pub version: u32,
    pub profile: String,
    pub mode: RunMode,
    pub scenario: Scenario,
    pub selection: Selection,
    pub daemon: DaemonIdentity,
    pub image_lock: serde_json::Value,
    pub helpers: std::collections::BTreeMap<String, String>,
    pub resources: Limits,
    pub capture: CaptureLimits,
    pub storage: StorageLimits,
    pub command_return_ms: u64,
    /// Absent for scripted model responses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<LiveModel>,
    /// The catalog recipe this profile was frozen from, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipe: Option<Recipe>,
    /// Manifest-bound authored definition, checked against the executing recipe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition: Option<FrozenRecipe>,
}
impl Profile {
    pub fn freeze(selection: Selection, daemon: DaemonIdentity) -> Result<Self> {
        Ok(Self {
            harness: Some(playtime_protocol::api::MEMENTO_TURNS_V1.into()),
            version: 1,
            profile: PROFILE.into(),
            mode: RunMode::ScriptedSmoke,
            scenario: serde_json::from_slice(SCENARIO)?,
            selection,
            daemon,
            image_lock: serde_json::from_slice(IMAGE_LOCK)?,
            helpers: playtime_docker::capture::helper_digests(),
            resources: Limits::smoke(),
            capture: CaptureLimits {
                file_bytes: 8 << 20,
                retained_bytes: 8 << 20,
                memory_bytes: 2048,
                ..CaptureLimits::default()
            },
            storage: StorageLimits {
                event_log_bytes: 8 << 20,
                ..StorageLimits::default()
            },
            command_return_ms: RETURN_OVERHEAD_MS,
            model: None,
            recipe: None,
            definition: None,
        })
    }
    /// The same workshop and limits with the live model and its smoke prompt.
    pub fn freeze_live(
        selection: Selection,
        daemon: DaemonIdentity,
        host: LiveHost,
    ) -> Result<Self> {
        let scenario: Scenario = serde_json::from_slice(LIVE_SCENARIO)?;
        let sampling = Sampling {
            thinking: "low".into(),
            temperature: 0.8,
            top_p: 0.95,
            top_k: 40,
            repeat_penalty: 1.0,
        };
        let settings = recipe::settings(scenario.parameters.limits.context_tokens.get(), &sampling);
        Ok(Self {
            profile: LIVE_PROFILE.into(),
            mode: RunMode::LiveSmoke,
            scenario,
            model: Some(LiveModel {
                registration: None,
                lock: serde_json::from_slice(MODEL_LOCK)?,
                settings,
                host,
            }),
            ..Self::freeze(selection, daemon)?
        })
    }
    pub fn validate(&self) -> Result<()> {
        let (selection, daemon) = (self.selection.clone(), self.daemon.clone());
        let mut expected = match (&self.model, &self.recipe) {
            (None, None) => Self::freeze(selection, daemon)?,
            (Some(model), None) => Self::freeze_live(selection, daemon, model.host.clone())?,
            (Some(model), Some(recipe)) => {
                if let Some(definition) = &self.definition {
                    Self::freeze_definition(
                        definition.clone(),
                        selection,
                        daemon,
                        model.host.clone(),
                    )?
                } else {
                    Self::freeze_recipe(recipe.clone(), selection, daemon, model.host.clone())?
                }
            }
            (None, Some(_)) => {
                return Err(Error::Invalid(
                    "a recipe profile needs the live model".into(),
                ));
            }
        };
        if let Some(definition) = &self.definition {
            definition.executable()?;
            if self.recipe.as_ref() != Some(&definition.recipe) {
                return Err(Error::Invalid(
                    "definition differs from executing recipe".into(),
                ));
            }
            expected.definition = Some(definition.clone());
        }
        if serde_json::to_value(self)? != serde_json::to_value(expected)?
            || !self.selection.executable.is_absolute()
            || !self.selection.socket.is_absolute()
            || self.selection.executable_sha256.len() != 64
            || !self
                .selection
                .executable_sha256
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
            || self.model.as_ref().is_some_and(|model| !model.host.valid())
        {
            return Err(Error::Invalid(
                "unsupported frozen Docker recipe, configuration or limits".into(),
            ));
        }
        self.scenario.parameters.validate()?;
        Ok(())
    }
    /// Every build-time field: version, recipe, image and model locks, request
    /// settings, helpers and limits. Host bindings are frozen at daemon startup
    /// or admission instead, so they are excluded.
    fn template(&self) -> Result<serde_json::Value> {
        let mut value = serde_json::to_value(self)?;
        let fields = value
            .as_object_mut()
            .ok_or_else(|| Error::Invalid("profile is not an object".into()))?;
        fields.remove("selection");
        fields.remove("daemon");
        if let Some(model) = fields.get_mut("model").and_then(|m| m.as_object_mut()) {
            model.remove("host");
        }
        Ok(value)
    }
    pub fn config(&self) -> ControllerConfig {
        ControllerConfig {
            parameters: self.scenario.parameters.clone(),
            protocol: self.scenario.protocol.clone(),
            prompt: self.scenario.prompt.clone(),
            inventory: self.scenario.inventory.clone(),
            initial_workspace: crate::prepare::reference(
                "inputs/empty-workspace.tar",
                &Self::initial_bytes(),
            ),
            initial_files: Default::default(),
        }
    }
    pub fn initial_bytes() -> Vec<u8> {
        vec![0; 10240]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn selection() -> Selection {
        Selection {
            executable: "/synthetic/docker".into(),
            socket: "/synthetic/socket".into(),
            executable_sha256: "a".repeat(64),
        }
    }
    fn daemon() -> DaemonIdentity {
        DaemonIdentity {
            id: "synthetic".into(),
            name: "synthetic".into(),
            cli_version: "28".into(),
            server_version: "28".into(),
            api_version: "1.51".into(),
            kernel: "synthetic".into(),
        }
    }
    fn host() -> LiveHost {
        LiveHost {
            endpoint: "http://model-host.local:11434".into(),
            addresses: vec!["192.0.2.10".parse().unwrap()],
            live_lock: "/run/user/1000/playtime/live-model.lock".into(),
        }
    }
    fn rejects(profile: &Profile, changes: &[(&str, serde_json::Value)]) {
        for (pointer, replacement) in changes {
            let mut bytes = serde_json::to_value(profile).unwrap();
            *bytes.pointer_mut(pointer).unwrap() = replacement.clone();
            let changed: Profile = serde_json::from_value(bytes).unwrap();
            assert!(changed.validate().is_err(), "{pointer}");
            // Only host bindings leave the advertised build-time template alone.
            let host = pointer.starts_with("/selection") || pointer.starts_with("/model/host");
            assert_eq!(
                changed.template().unwrap() == profile.template().unwrap(),
                host,
                "{pointer}"
            );
        }
    }
    #[test]
    fn frozen_profile_rejects_unreviewed_recipes_limits_helpers_and_versions() {
        let profile = Profile::freeze(selection(), daemon()).unwrap();
        profile.validate().unwrap();
        rejects(
            &profile,
            &[
                ("/version", serde_json::json!(2)),
                ("/mode", serde_json::json!("simulation")),
                (
                    "/scenario/initial_files",
                    serde_json::json!({"seed.txt":"unsupported"}),
                ),
                (
                    "/scenario/parameters/memory_condition",
                    serde_json::json!("self_plus_evidence"),
                ),
                (
                    "/scenario/parameters/limits/wall_seconds",
                    serde_json::json!(121),
                ),
                ("/scenario/prompt", serde_json::json!("replacement")),
                (
                    "/storage/verified_content_bytes",
                    serde_json::json!(1073741824u64),
                ),
                ("/capture/entries", serde_json::json!(8192)),
                ("/helpers/workspace-exporter-v1", serde_json::json!("wrong")),
                ("/selection/socket", serde_json::json!("relative.sock")),
            ],
        );
    }
    #[test]
    fn live_profile_freezes_model_lock_settings_and_valid_host_bindings() {
        let live = Profile::freeze_live(selection(), daemon(), host()).unwrap();
        live.validate().unwrap();
        assert_eq!(live.mode, RunMode::LiveSmoke);
        assert!(live.scenario.models.is_empty());
        assert_eq!(live.scenario.parameters.limits.tool_parse_retries, 2);
        // A scripted profile can't pass as live, or the reverse.
        let mut scripted =
            serde_json::to_value(Profile::freeze(selection(), daemon()).unwrap()).unwrap();
        scripted["model"] = serde_json::to_value(&live.model).unwrap();
        assert!(
            serde_json::from_value::<Profile>(scripted)
                .unwrap()
                .validate()
                .is_err()
        );
        let mut unmodeled = live.clone();
        unmodeled.model = None;
        assert!(unmodeled.validate().is_err());
        rejects(
            &live,
            &[
                ("/model/lock/digest", serde_json::json!("0".repeat(64))),
                ("/model/settings/context_tokens", serde_json::json!(65536)),
                ("/model/settings/thinking", serde_json::json!("high")),
                (
                    "/scenario/parameters/limits/tool_parse_retries",
                    serde_json::json!(0),
                ),
                (
                    "/model/host/endpoint",
                    serde_json::json!("https://model-host.local:11434"),
                ),
                ("/model/host/addresses", serde_json::json!([])),
                (
                    "/model/host/addresses",
                    serde_json::json!(["192.0.2.10", "192.0.2.10"]),
                ),
                ("/model/host/live_lock", serde_json::json!("relative.lock")),
            ],
        );
        // Any host binding produces the same advertised digest input.
        let elsewhere = LiveHost {
            endpoint: "http://other.local:11434".into(),
            addresses: vec![
                "192.0.2.11".parse().unwrap(),
                "2001:db8::1".parse().unwrap(),
            ],
            live_lock: "/run/user/1001/playtime/live-model.lock".into(),
        };
        let moved = Profile::freeze_live(selection(), daemon(), elsewhere).unwrap();
        moved.validate().unwrap();
        assert_eq!(moved.template().unwrap(), live.template().unwrap());
        assert_eq!(digest().unwrap(), digest().unwrap());
    }
    #[test]
    fn frozen_lock_copies_match_the_repository_locks() {
        // The worker build snapshot captures only crates/, so the locks are
        // copied here. Refreshing either lock needs a new profile version.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        for (copy, path) in [
            (IMAGE_LOCK, "environments/general.lock.json"),
            (MODEL_LOCK, "models/gpt-oss-20b.lock.json"),
        ] {
            assert!(
                copy == std::fs::read(root.join(path)).unwrap(),
                "{path} changed; freeze a new Docker profile version for it"
            );
        }
    }
}
