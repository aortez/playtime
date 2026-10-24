//! Read-only model discovery; independent of archive/workshop readiness.
use crate::application::{ApiError, Result};
use playtime_ollama::{
    CancellationToken, Endpoint, Failure, FailureKind, Limits, NativeTransport, Trust,
    discovery::Discovery,
};
use playtime_protocol::api::*;
use std::time::Duration;
use tokio::{sync::Semaphore, time::Instant};

pub struct ModelBrowser {
    endpoint: Endpoint,
    reads: Semaphore,
}
fn observed_at() -> playtime_core::UtcTime {
    use crate::lifecycle::ports::{Clock, SystemClock};
    SystemClock.now().utc
}
fn failure(error: Failure) -> ApiError {
    ApiError::new(match error.kind {
        FailureKind::ByteLimit => "limit_exceeded",
        FailureKind::Request => "invalid_parameters",
        FailureKind::Timeout => "request_timeout",
        FailureKind::Identity => "model_metadata_changed",
        _ => "backend_unavailable",
    })
}
fn reviewed_capabilities(value: &ModelInspection) -> Option<RecipeModelCapabilities> {
    let locked: serde_json::Value =
        serde_json::from_slice(playtime_runner::docker::MODEL_LOCK).ok()?;
    // Discovery has already fingerprinted the canonical definition. Match the
    // same identity fields the execution adapter verifies, not the generated
    // Modelfile's host-specific blob path. Keep the reviewed lock itself intact.
    if locked["name"].as_str() != Some(value.name.as_str())
        || locked["digest"].as_str() != value.digest.as_deref()
        || locked["ollama_version"].as_str() != Some(value.backend_version.as_str())
        || locked["definition_sha256"].as_str() != value.definition_sha256.as_deref()
    {
        return None;
    }
    playtime_runner::docker::reviewed_model_capabilities(&locked).ok()
}
impl ModelBrowser {
    pub fn new(endpoint: &str) -> Result<Self> {
        let endpoint = Endpoint::parse(endpoint).map_err(failure)?;
        if !endpoint.as_str().starts_with("http://") {
            return Err(ApiError::new("invalid_parameters"));
        }
        Ok(Self {
            endpoint,
            reads: Semaphore::new(2),
        })
    }
    fn reader(&self) -> Result<Discovery<NativeTransport>> {
        let limits = Limits::default();
        Discovery::new(
            NativeTransport::new(self.endpoint.clone(), Trust::BundledWebPki, &limits)
                .map_err(failure)?,
            limits,
        )
        .map_err(failure)
    }
    pub async fn inventory(&self) -> Result<ModelInventory> {
        let _permit = self
            .reads
            .try_acquire()
            .map_err(|_| ApiError::new("model_discovery_busy"))?;
        // Below the HTTP handler's 30s bound. Dropping the future drops transport.
        let inventory = self
            .reader()?
            .inventory(
                Instant::now() + Duration::from_secs(25),
                &CancellationToken::new(),
            )
            .await
            .map_err(failure)?;
        Ok(ModelInventory {
            schema_version: 1,
            observed_at: observed_at(),
            backend_version: inventory.version,
            loaded_warning: inventory
                .loaded
                .is_none()
                .then(|| "Loaded-state metadata unavailable; installed inventory retained".into()),
            models: inventory
                .entries
                .into_iter()
                .map(|e| {
                    let loaded = match (&inventory.loaded, &e.digest) {
                        (Some(loaded), Some(digest)) if !e.rejected => {
                            if loaded.contains(&(e.name.clone(), digest.clone())) {
                                ModelLoaded::Loaded
                            } else {
                                ModelLoaded::NotLoaded
                            }
                        }
                        _ => ModelLoaded::Unknown,
                    };
                    DiscoveredModel {
                        name: e.name,
                        digest: e.digest,
                        status: if e.rejected {
                            ModelMetadataStatus::Rejected
                        } else {
                            ModelMetadataStatus::Unverified
                        },
                        reason: Some(
                            if e.rejected {
                                "Remote/cloud delegation is unsupported"
                            } else {
                                "Select to inspect identity; not registered"
                            }
                            .into(),
                        ),
                        loaded,
                    }
                })
                .collect(),
        })
    }
    pub async fn inspect(&self, request: ModelInspectRequest) -> Result<ModelInspection> {
        self.inspect_definition(request)
            .await
            .map(|(value, _)| value)
    }
    pub(crate) fn binding(&self, service: &ServiceId) -> String {
        playtime_ollama::json::hash(
            &serde_json::to_vec(&(service, self.endpoint.as_str())).expect("endpoint identity"),
        )
    }
    pub async fn review(
        &self,
        service: &ServiceId,
        request: ModelInspectRequest,
    ) -> Result<ModelRegistrationReview> {
        let (inspection, _) = self.inspect_definition(request).await?;
        let request = RegisteredModelIdentity::from_inspection(&inspection).map(|identity| {
            ModelRegistrationRequest {
                endpoint_binding: self.binding(service),
                identity,
            }
        });
        Ok(ModelRegistrationReview {
            inspection,
            request,
        })
    }
    pub(crate) async fn accept(
        &self,
        service: &ServiceId,
        request: &ModelRegistrationRequest,
    ) -> Result<serde_json::Value> {
        if !request.valid() {
            return Err(ApiError::new("invalid_parameters"));
        }
        if request.endpoint_binding != self.binding(service) {
            return Err(ApiError::new("model_metadata_changed"));
        }
        let (inspection, definition) = self
            .inspect_definition(ModelInspectRequest {
                name: request.identity.name.clone(),
                expected_digest: Some(request.identity.digest.clone()),
            })
            .await?;
        if RegisteredModelIdentity::from_inspection(&inspection).as_ref() != Some(&request.identity)
        {
            return Err(ApiError::new("model_metadata_changed"));
        }
        definition.ok_or_else(|| ApiError::new("model_metadata_changed"))
    }
    async fn inspect_definition(
        &self,
        request: ModelInspectRequest,
    ) -> Result<(ModelInspection, Option<serde_json::Value>)> {
        let _permit = self
            .reads
            .try_acquire()
            .map_err(|_| ApiError::new("model_discovery_busy"))?;
        let detail = self
            .reader()?
            .inspect(
                &request.name,
                request.expected_digest.as_deref(),
                Instant::now() + Duration::from_secs(25),
                &CancellationToken::new(),
            )
            .await
            .map_err(failure)?;
        let entry = detail
            .inventory
            .entries
            .iter()
            .find(|e| e.name == request.name);
        let mut value = ModelInspection {
            schema_version: 1,
            observed_at: observed_at(),
            name: request.name,
            backend_version: detail.inventory.version.clone(),
            digest: entry.and_then(|e| e.digest.clone()),
            status: ModelMetadataStatus::Unverified,
            reason: None,
            definition_sha256: detail.definition_sha256,
            advertised_capabilities: vec![],
            capability_provenance: "Ollama metadata observation; capability support unreviewed"
                .into(),
            reviewed_capabilities: None,
        };
        if entry.is_none() {
            value.status = ModelMetadataStatus::Missing;
            value.reason =
                Some("Selected model is not installed; no replacement was selected".into());
        } else if detail.rejected {
            value.status = ModelMetadataStatus::Rejected;
            value.reason = Some("Remote/cloud delegation is unsupported".into());
        } else if request.expected_digest.is_some() && request.expected_digest != value.digest {
            value.status = ModelMetadataStatus::Changed;
            value.reason = Some("Digest changed since inventory; refresh and inspect again".into());
        } else if value.definition_sha256.is_none() {
            value.reason =
                Some("Identity metadata is incomplete; candidate remains unverified".into());
        } else if value.backend_version != playtime_ollama::Profile::VERSION {
            value.reason = Some("Backend version is not reviewed for execution".into());
        } else if let Some(show) = &detail.show {
            if let Some(caps) = show
                .get("capabilities")
                .and_then(serde_json::Value::as_array)
                && caps.len() <= 64
                && caps.iter().all(|v| {
                    v.as_str().is_some_and(|s| {
                        !s.is_empty() && s.len() <= 128 && !s.chars().any(char::is_control)
                    })
                })
            {
                value.advertised_capabilities =
                    caps.iter().map(|v| v.as_str().unwrap().into()).collect();
                value.status = ModelMetadataStatus::Candidate;
                value.reason = Some(
                    if value.advertised_capabilities.iter().any(|c| c == "tools") {
                        "Identity candidate only; registration and recipe selection are separate operations"
                    } else {
                        "Current memento harness requires tools; model does not advertise tools"
                    }.into()
                );
                value.reviewed_capabilities = reviewed_capabilities(&value);
                if let Some(c) = &value.reviewed_capabilities {
                    value.capability_provenance = c.provenance.clone();
                }
            } else {
                value.reason =
                    Some("Capability metadata is missing or malformed; support is unknown".into());
            }
        }
        Ok((value, detail.definition))
    }
}
