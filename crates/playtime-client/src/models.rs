//! Read-only model metadata. No registration, recipe mutation or inference.
use crate::{Client, Error, Result};
use playtime_protocol::api::*;
use reqwest::Method;
pub(crate) fn name(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
}
pub(crate) fn digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}
impl Client {
    fn model_discovery(&self) -> Result<()> {
        if self
            .capabilities()
            .features
            .iter()
            .any(|f| f == MODEL_DISCOVERY)
        {
            Ok(())
        } else {
            Err(Error::Api {
                status: 422,
                response: playtime_protocol::v1::ErrorResponse {
                    code: "unsupported_capability".into(),
                    message: "Model discovery unavailable".into(),
                    request_id: None,
                    details: Default::default(),
                },
            })
        }
    }
    pub async fn model_inventory(&self) -> Result<ModelInventory> {
        self.model_discovery()?;
        let value: ModelInventory = self.json(self.request(Method::GET, "models")).await?;
        let mut names = std::collections::BTreeSet::new();
        if value.schema_version != 1
            || !name(&value.backend_version)
            || value.models.len() > MODEL_INVENTORY_LIMIT
            || value.models.iter().any(|m| {
                !name(&m.name)
                    || !names.insert(&m.name)
                    || m.digest.as_deref().is_some_and(|s| !digest(s))
                    || !matches!(
                        m.status,
                        ModelMetadataStatus::Unverified | ModelMetadataStatus::Rejected
                    )
                    || m.reason.as_ref().is_some_and(|s| s.len() > 1024)
            })
            || value
                .loaded_warning
                .as_ref()
                .is_some_and(|s| s.len() > 1024)
        {
            return Err(Error::InvalidResponse);
        }
        Ok(value)
    }
    pub async fn inspect_model(&self, request: &ModelInspectRequest) -> Result<ModelInspection> {
        self.model_discovery()?;
        if !name(&request.name)
            || request
                .expected_digest
                .as_deref()
                .is_some_and(|s| !digest(s))
        {
            return Err(Error::InvalidResponse);
        }
        let value: ModelInspection = self
            .json(self.request(Method::POST, "models/inspect").json(request))
            .await?;
        validate_inspection(&value, request)?;
        Ok(value)
    }
}

pub(crate) fn validate_inspection(
    value: &ModelInspection,
    request: &ModelInspectRequest,
) -> Result<()> {
    if value.schema_version != 1
        || value.name != request.name
        || !name(&value.backend_version)
        || value.digest.as_deref().is_some_and(|s| !digest(s))
        || value
            .definition_sha256
            .as_deref()
            .is_some_and(|s| !digest(s))
        || value.reason.as_ref().is_some_and(|s| s.len() > 1024)
        || value.capability_provenance.len() > 1024
        || value.advertised_capabilities.len() > 64
        || value
            .advertised_capabilities
            .iter()
            .any(|s| s.len() > 128 || s.chars().any(char::is_control))
    {
        return Err(Error::InvalidResponse);
    }
    if value.status == ModelMetadataStatus::Candidate
        && (value.digest.is_none()
            || value.definition_sha256.is_none()
            || request
                .expected_digest
                .as_ref()
                .is_some_and(|d| Some(d) != value.digest.as_ref()))
    {
        return Err(Error::InvalidResponse);
    }
    if let Some(c) = &value.reviewed_capabilities
        && (Some(&c.model_digest) != value.digest.as_ref()
            || c.backend_version != value.backend_version
            || c.provenance.len() > 512
            || c.reasoning.len() > 4
            || c.reasoning.is_empty()
            || c.context_ceiling == Some(0))
    {
        return Err(Error::InvalidResponse);
    }
    Ok(())
}
