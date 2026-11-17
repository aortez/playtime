use playtime_core::Sha256Digest;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{Failure, FailureKind, Result, Settings};

pub(crate) fn digest(value: &[u8]) -> Sha256Digest {
    Sha256Digest::new(
        Sha256::digest(value)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>(),
    )
    .unwrap()
}

/// Verified by the host owner before lending its endpoint. HTTP metadata cannot
/// establish weights, engine executable or launch-environment byte identity.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostIdentity {
    pub backend: String,
    pub quantization: String,
    pub engine_sha256: Sha256Digest,
    pub artifacts_manifest_sha256: Sha256Digest,
    pub launch_configuration_sha256: Sha256Digest,
    pub responses_source_sha256: Sha256Digest,
}

/// Stable fields of /props and /v1/status, excluding activity and process age.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    /// Omitted for historical Strata locks so their saved fingerprints stay stable.
    #[serde(default, skip_serializing_if = "Service::is_strata")]
    pub service: Service,
    #[serde(default = "one_slot", skip_serializing_if = "is_one_slot")]
    pub parallel: u64,
    pub model: String,
    pub engine: String,
    pub context_tokens: u64,
    pub model_path: String,
    pub template_sha256: Sha256Digest,
    pub defaults: Value,
    pub template_capabilities: Value,
    pub shared_settings: Value,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Service {
    #[default]
    Strata,
    Basalt,
}
impl Service {
    fn is_strata(&self) -> bool {
        *self == Self::Strata
    }
}
fn one_slot() -> u64 {
    1
}
fn is_one_slot(value: &u64) -> bool {
    *value == 1
}

impl Identity {
    pub fn inspect(props: &Value, status: &Value, settings: &Value) -> Result<Self> {
        let error = || {
            Failure::new(
                FailureKind::Identity,
                "unsupported or unloaded Responses server metadata",
            )
        };
        let text = |v: &Value| {
            v.as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .ok_or_else(error)
        };
        let model = text(&status["model"])?;
        let engine = text(&status["engine"])?;
        let (service, build) = match status["service"].as_str() {
            Some("strata") => (Service::Strata, format!("Strata {engine}")),
            Some("basalt") => (Service::Basalt, format!("basalt {engine}")),
            _ => return Err(error()),
        };
        let parallel = status["concurrency"]["serving"]
            .as_u64()
            .ok_or_else(error)?;
        let context_tokens = status["context"]["native"]
            .as_u64()
            .filter(|n| *n > 0)
            .ok_or_else(error)?;
        if status["loaded"] != true
            || props["is_sleeping"] != false
            || props["model_alias"] != model
            || props["build_info"] != build
            || props["default_generation_settings"]["n_ctx"] != context_tokens
            || status["context"]["max_positions"] != context_tokens
            || props["total_slots"] != parallel
            || !(1..=2).contains(&parallel)
            || (service == Service::Strata && parallel != 1)
            || !status["dialects"]
                .as_array()
                .is_some_and(|a| a.contains(&json!("/v1/responses")))
        {
            return Err(error());
        }
        let defaults = props["default_generation_settings"]["params"].clone();
        if !defaults.is_object()
            || !props["chat_template_caps"].is_object()
            || (service == Service::Strata
                && (!settings["defaults"].is_object() || !settings["shared"].is_boolean()))
            || (service == Service::Basalt && !settings.is_null())
        {
            return Err(error());
        }
        Ok(Self {
            service,
            parallel,
            model,
            engine,
            context_tokens,
            model_path: text(&props["model_path"])?,
            template_sha256: digest(text(&props["chat_template"])?.as_bytes()),
            defaults,
            template_capabilities: props["chat_template_caps"].clone(),
            shared_settings: if service == Service::Basalt {
                Value::Null
            } else {
                json!({"shared":settings["shared"],"defaults":settings["defaults"]})
            },
        })
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelLock {
    pub schema_version: u32,
    pub identity: Identity,
    pub host: HostIdentity,
    pub settings: Settings,
}
impl ModelLock {
    pub fn validate(&self) -> Result<()> {
        self.settings.validate()?;
        let reviewed_runtime = match self.identity.service {
            Service::Strata => {
                self.schema_version == 1
                    && self.identity.parallel == 1
                    && matches!(self.identity.engine.as_str(), "0.1.57" | "0.1.58")
                    && self.host.responses_source_sha256.as_str()
                        == "aba98b4aba933fe55cf699985add2e1014a66dacb55ed768ca9aa404c3fcb29b"
            }
            Service::Basalt => {
                self.schema_version == 2
                    && self.identity.engine == "0.4.1"
                    && (1..=2).contains(&self.identity.parallel)
                    && self.identity.shared_settings.is_null()
                    && self.host.backend == "cuda"
                    && self.host.engine_sha256.as_str()
                        == "5a15f95f8bf41fbd61c8e7299910d08049243eb32b26699b0b7a6f7976ad92e8"
                    && self.host.responses_source_sha256.as_str()
                        == "c1ba29dd88ac98f052b34e75de46106737a559871c75eb565fb852930e7bcf23"
            }
        };
        if !reviewed_runtime
            || !matches!(self.host.backend.as_str(), "cuda" | "hip")
            || self.host.quantization.is_empty()
            || self.host.quantization.len() > 128
            || self.identity.model.is_empty()
            || self.identity.model.len() > 256
            || self.identity.model_path.is_empty()
            || self.identity.context_tokens == 0
        {
            return Err(Failure::new(
                FailureKind::Identity,
                "unsupported model lock or unreviewed Responses implementation",
            ));
        }
        Ok(())
    }

    pub fn fingerprint(&self) -> Result<Sha256Digest> {
        self.validate()?;
        Ok(digest(
            &serde_json::to_vec(self).map_err(|e| Failure::new(FailureKind::Identity, e))?,
        ))
    }
}
