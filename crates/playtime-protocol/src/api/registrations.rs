//! Explicit model acceptance; immutable records are not execution authority.
use super::*;
pub const MODEL_REGISTRATION: &str = "model_registration_v1";
pub const MODEL_REGISTRATION_LIMIT: usize = 256;
pub const MODEL_DEFINITION_BYTES: usize = 1 << 20;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RegisteredModelIdentity {
    pub schema_version: u32,
    pub name: String,
    pub digest: String,
    pub backend_version: String,
    pub definition_sha256: String,
    pub advertised_capabilities: Vec<String>,
    pub capability_provenance: String,
    pub reviewed_capabilities: Option<RecipeModelCapabilities>,
}
impl RegisteredModelIdentity {
    pub fn from_inspection(value: &ModelInspection) -> Option<Self> {
        if value.status != ModelMetadataStatus::Candidate {
            return None;
        }
        let result = Self {
            schema_version: 1,
            name: value.name.clone(),
            digest: value.digest.clone()?,
            backend_version: value.backend_version.clone(),
            definition_sha256: value.definition_sha256.clone()?,
            advertised_capabilities: value.advertised_capabilities.clone(),
            capability_provenance: value.capability_provenance.clone(),
            reviewed_capabilities: value.reviewed_capabilities.clone(),
        };
        result.valid().then_some(result)
    }
    pub fn valid(&self) -> bool {
        fn text(s: &str, limit: usize) -> bool {
            !s.is_empty() && s.len() <= limit && !s.chars().any(char::is_control)
        }
        self.schema_version == 1
            && text(&self.name, 256)
            && text(&self.backend_version, 256)
            && registration_digest(&self.digest)
            && registration_digest(&self.definition_sha256)
            && self.advertised_capabilities.len() <= 64
            && self.advertised_capabilities.iter().all(|s| text(s, 128))
            && text(&self.capability_provenance, 1024)
            && self.reviewed_capabilities.as_ref().is_none_or(|c| {
                c.model_digest == self.digest
                    && c.backend_version == self.backend_version
                    && text(&c.provenance, 512)
                    && !c.reasoning.is_empty()
                    && c.reasoning.len() <= 4
                    && c.context_ceiling != Some(0)
                    && c.default_reasoning.is_none_or(|r| c.reasoning.contains(&r))
            })
    }
}
pub fn registration_digest(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelRegistrationRequest {
    /// Opaque binding to the accepting service and configured endpoint, not model identity.
    pub endpoint_binding: String,
    pub identity: RegisteredModelIdentity,
}
impl ModelRegistrationRequest {
    pub fn valid(&self) -> bool {
        registration_digest(&self.endpoint_binding) && self.identity.valid()
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelRegistrationReview {
    pub inspection: ModelInspection,
    /// Absent when identity cannot be accepted. Registration never certifies harness support.
    pub request: Option<ModelRegistrationRequest>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelRegistrationSummary {
    pub registration_id: ModelRegistrationId,
    pub identity: RegisteredModelIdentity,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RegisteredModel {
    pub schema_version: u32,
    pub summary: ModelRegistrationSummary,
    #[schema(value_type = Object)]
    pub definition: Value,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelRegistrations {
    pub schema_version: u32,
    pub items: Vec<ModelRegistrationSummary>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelRegistrationResponse {
    pub service_id: ServiceId,
    pub key: IdempotencyKey,
    pub request: ModelRegistrationRequest,
    pub registration: ModelRegistrationSummary,
    pub replayed: bool,
}
