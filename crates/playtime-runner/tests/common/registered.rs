//! Exact reviewed Qwen identity for execution tests; no live metadata is consulted.
use playtime_protocol::api::*;
use playtime_runner::docker::{QWEN_MODEL_LOCK, reviewed_identity_capabilities};
use serde_json::Value;

pub fn qwen() -> RegisteredModel {
    from_lock(QWEN_MODEL_LOCK)
}

pub fn from_lock(bytes: &[u8]) -> RegisteredModel {
    let lock: Value = serde_json::from_slice(bytes).unwrap();
    let text = |key: &str| lock[key].as_str().unwrap().to_owned();
    let capabilities = reviewed_identity_capabilities(
        &text("name"),
        &text("digest"),
        &text("ollama_version"),
        &text("definition_sha256"),
    )
    .unwrap();
    RegisteredModel {
        schema_version: 1,
        summary: ModelRegistrationSummary {
            registration_id: ModelRegistrationId::new("model-reviewed-qwen").unwrap(),
            identity: RegisteredModelIdentity {
                schema_version: 1,
                name: text("name"),
                digest: text("digest"),
                backend_version: text("ollama_version"),
                definition_sha256: text("definition_sha256"),
                advertised_capabilities: serde_json::from_value(
                    lock["definition"]["capabilities"].clone(),
                )
                .unwrap(),
                capability_provenance: capabilities.provenance.clone(),
                reviewed_capabilities: Some(capabilities),
            },
        },
        definition: lock["definition"].clone(),
    }
}
