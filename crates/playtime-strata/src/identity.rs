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
    /// Digest of the Python Responses module that Strata and Basalt serve from.
    /// llama-server has none; its API is part of the engine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub responses_source_sha256: Option<Sha256Digest>,
    /// Digest of the manifest of every file in the Strata server release the API
    /// server runs from. Locks made before the verifier checked the release omit
    /// it, so their bytes are unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_release_manifest_sha256: Option<Sha256Digest>,
    /// The llama.cpp commit that a llama-server engine was built from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_commit: Option<String>,
    /// Digest of a llama-server engine's build record: its source commit, build
    /// options, toolchain, linked system libraries and binary digest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_manifest_sha256: Option<Sha256Digest>,
}

/// Each reviewed Strata server release, by the engine version its server reports,
/// with the digest of its complete file manifest.
const STRATA_RELEASES: [(&str, &str); 2] = [
    (
        "0.1.57",
        "0e18282520906d8c01153ea0f6cf14fc2746847d06bf9ed5b96ae63f1a294797",
    ),
    (
        "0.1.58",
        "f49826e2c75d33790f0bce86651e740c0b40fe34c9e63e31b1134155e79466b6",
    ),
];

/// Each reviewed llama.cpp release, by its tag and the commit it names.
const LLAMA_SERVER_RELEASES: [(&str, &str); 1] =
    [("v1.0.0", "b1b770d90d94cad9e9ea5b993bdbc383a522fb28")];

/// Stable fields of the server's metadata, excluding activity and process age.
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
    /// llama-server's other stable /props fields and its model's /v1/models
    /// metadata. Strata and Basalt locks omit it, so their bytes are unchanged.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub server: Value,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Service {
    #[default]
    Strata,
    Basalt,
    /// llama.cpp's llama-server, which serves Chat Completions.
    LlamaServer,
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
            server: Value::Null,
        })
    }

    /// llama-server's identity from its /props, /slots and /v1/models. Only a
    /// loaded, awake, single-model, text-only server with read-only properties
    /// qualifies.
    pub fn inspect_llama(props: &Value, slots: &Value, models: &Value) -> Result<Self> {
        let error = || {
            Failure::new(
                FailureKind::Identity,
                "unsupported or unloaded llama-server metadata",
            )
        };
        let text = |v: &Value| {
            v.as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .ok_or_else(error)
        };
        let model = text(&props["model_alias"])?;
        let engine = text(&props["build_info"])?;
        let parallel = props["total_slots"].as_u64().ok_or_else(error)?;
        let context_tokens = props["default_generation_settings"]["n_ctx"]
            .as_u64()
            .filter(|n| *n > 0)
            .ok_or_else(error)?;
        let defaults = props["default_generation_settings"]["params"].clone();
        let listed = models["data"].as_array().ok_or_else(error)?;
        if props["is_sleeping"] != false
            || parallel != 1
            || !defaults.is_object()
            || !props["chat_template_caps"].is_object()
            || props["modalities"] != json!({"vision":false, "video":false, "audio":false})
            || props["endpoint_props"] != false
            || props["endpoint_slots"] != true
            || slots.as_array().is_none_or(|s| s.len() as u64 != parallel)
            || listed.len() != 1
            || listed[0]["id"] != model
            || !listed[0]["meta"].is_object()
            || listed[0]["meta"]["n_ctx"] != context_tokens
        {
            return Err(error());
        }
        let mut server = serde_json::Map::new();
        for key in [
            "model_ftype",
            "bos_token",
            "eos_token",
            "endpoint_metrics",
            "ui",
            "ui_settings",
            "cors_proxy_enabled",
        ] {
            server.insert(key.into(), props.get(key).ok_or_else(error)?.clone());
        }
        server.insert("model_meta".into(), listed[0]["meta"].clone());
        Ok(Self {
            service: Service::LlamaServer,
            parallel,
            model,
            engine,
            context_tokens,
            model_path: text(&props["model_path"])?,
            template_sha256: digest(text(&props["chat_template"])?.as_bytes()),
            defaults,
            template_capabilities: props["chat_template_caps"].clone(),
            shared_settings: Value::Null,
            server: Value::Object(server),
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
        let responses_source = |expected: &str| {
            self.host
                .responses_source_sha256
                .as_ref()
                .is_some_and(|digest| digest.as_str() == expected)
        };
        let llama = self.identity.service == Service::LlamaServer;
        let reviewed_runtime = match self.identity.service {
            Service::Strata => {
                self.schema_version == 1
                    && self.identity.parallel == 1
                    && responses_source(
                        "aba98b4aba933fe55cf699985add2e1014a66dacb55ed768ca9aa404c3fcb29b",
                    )
                    && self
                        .host
                        .server_release_manifest_sha256
                        .as_ref()
                        .is_some_and(|release| {
                            STRATA_RELEASES
                                .contains(&(self.identity.engine.as_str(), release.as_str()))
                        })
            }
            Service::Basalt => {
                self.schema_version == 2
                    && self.identity.engine == "0.4.1"
                    && (1..=2).contains(&self.identity.parallel)
                    && self.identity.shared_settings.is_null()
                    && self.host.backend == "cuda"
                    && self.host.engine_sha256.as_str()
                        == "5a15f95f8bf41fbd61c8e7299910d08049243eb32b26699b0b7a6f7976ad92e8"
                    && responses_source(
                        "c1ba29dd88ac98f052b34e75de46106737a559871c75eb565fb852930e7bcf23",
                    )
                    // Basalt's serving code is already in its artifact manifest.
                    && self.host.server_release_manifest_sha256.is_none()
            }
            // Each host builds its own engine, so its binary digest is the
            // lock's own. The release is pinned by its source commit, which
            // the server's build_info abbreviates.
            Service::LlamaServer => {
                self.schema_version == 3
                    && self.identity.parallel == 1
                    && self.identity.shared_settings.is_null()
                    && self.identity.server.is_object()
                    && self.host.backend == "cpu"
                    && self.host.responses_source_sha256.is_none()
                    && self.host.server_release_manifest_sha256.is_none()
                    && self.host.build_manifest_sha256.is_some()
                    && self.host.source_commit.as_deref().is_some_and(|commit| {
                        LLAMA_SERVER_RELEASES.iter().any(|(_, c)| *c == commit)
                            && self
                                .identity
                                .engine
                                .strip_prefix('b')
                                .and_then(|e| e.split_once('-'))
                                .is_some_and(|(build, short)| {
                                    !build.is_empty()
                                        && build.bytes().all(|b| b.is_ascii_digit())
                                        && short == &commit[..7]
                                })
                    })
                    && self.settings.llama_thinking()
            }
        };
        let strata_backend = matches!(self.host.backend.as_str(), "cuda" | "hip")
            && self.host.source_commit.is_none()
            && self.host.build_manifest_sha256.is_none()
            && self.identity.server.is_null()
            && self.settings.responses_thinking();
        if !reviewed_runtime
            || (!llama && !strata_backend)
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

    /// The fingerprint of the same lock without its server release manifest,
    /// which sessions saved before the verifier checked the release are bound to.
    /// This lock requires everything that one did, so those sessions may resume
    /// under their saved binding.
    pub fn earlier_fingerprint(&self) -> Result<Option<Sha256Digest>> {
        self.validate()?;
        if self.host.server_release_manifest_sha256.is_none() {
            return Ok(None);
        }
        let mut earlier = self.clone();
        earlier.host.server_release_manifest_sha256 = None;
        Ok(Some(digest(
            &serde_json::to_vec(&earlier).map_err(|e| Failure::new(FailureKind::Identity, e))?,
        )))
    }
}
