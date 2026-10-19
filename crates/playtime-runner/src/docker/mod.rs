//! Versioned, intentionally narrow frozen Docker/scripted-model profile.
use crate::{Error, Result};
use playtime_core::{RunMode, Sha256Digest};
use playtime_docker::{DaemonIdentity, Limits, capture::CaptureLimits, transport::Selection};
use playtime_engine::{ControllerConfig, simulation::Scenario};
use playtime_storage::StorageLimits;
use serde::{Deserialize, Serialize};

pub const PROFILE: &str = "docker_scripted_no_seed_self_v1";
pub const RETURN_OVERHEAD_MS: u64 = 1000;
pub const RECIPE: &str = "docker-two-session";
pub const SCENARIO: &[u8] = include_bytes!("smoke.json");
pub const IMAGE_LOCK: &[u8] = include_bytes!("image-lock.json");

/// Digest of this build's compiled profile, which its bundles advertise.
pub fn digest() -> Result<Sha256Digest> {
    // The digest excludes host bindings, so these placeholders never matter.
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
    Profile::freeze(selection, daemon)?.digest()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
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
}
impl Profile {
    pub fn freeze(selection: Selection, daemon: DaemonIdentity) -> Result<Self> {
        Ok(Self {
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
        })
    }
    pub fn validate(&self) -> Result<()> {
        let expected = Self::freeze(self.selection.clone(), self.daemon.clone())?;
        if serde_json::to_value(self)? != serde_json::to_value(expected)?
            || !self.selection.executable.is_absolute()
            || !self.selection.socket.is_absolute()
            || self.selection.executable_sha256.len() != 64
            || !self
                .selection
                .executable_sha256
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        {
            return Err(Error::Invalid(
                "unsupported frozen Docker recipe, configuration or limits".into(),
            ));
        }
        self.scenario.parameters.validate()?;
        Ok(())
    }
    /// SHA-256 of every build-time field: version, recipe, image lock, helpers
    /// and limits. The host's Docker selection and daemon identity are frozen
    /// at daemon startup instead, so they are excluded.
    pub fn digest(&self) -> Result<Sha256Digest> {
        let mut value = serde_json::to_value(self)?;
        let fields = value
            .as_object_mut()
            .ok_or_else(|| Error::Invalid("profile is not an object".into()))?;
        fields.remove("selection");
        fields.remove("daemon");
        Ok(playtime_storage::sha256(&serde_json::to_vec(&value)?))
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
    #[test]
    fn frozen_profile_rejects_unreviewed_recipes_limits_helpers_and_versions() {
        let profile = Profile::freeze(
            Selection {
                executable: "/synthetic/docker".into(),
                socket: "/synthetic/socket".into(),
                executable_sha256: "a".repeat(64),
            },
            DaemonIdentity {
                id: "synthetic".into(),
                name: "synthetic".into(),
                cli_version: "28".into(),
                server_version: "28".into(),
                api_version: "1.51".into(),
                kernel: "synthetic".into(),
            },
        )
        .unwrap();
        profile.validate().unwrap();
        // Host bindings never change the advertised build-time digest.
        assert_eq!(profile.digest().unwrap(), digest().unwrap());
        for (pointer, replacement) in [
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
        ] {
            let mut bytes = serde_json::to_value(&profile).unwrap();
            *bytes.pointer_mut(pointer).unwrap() = replacement;
            let changed: Profile = serde_json::from_value(bytes).unwrap();
            assert!(changed.validate().is_err(), "{pointer}");
            assert_eq!(
                changed.digest().unwrap() == profile.digest().unwrap(),
                pointer.starts_with("/selection"),
                "{pointer}"
            );
        }
    }
}
