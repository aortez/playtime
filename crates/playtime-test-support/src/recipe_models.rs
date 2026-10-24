//! Synthetic reviewed capability records exercise authoring independently of
//! the future native profile review. Never use these records for inference.
use playtime_ollama::json;
use playtime_protocol::api::*;
use serde_json::json as value;
use std::path::Path;

pub fn boolean_model() -> RegisteredModel {
    let definition = value!({"template":"synthetic boolean template", "parameters":"",
        "details":{"family":"synthetic"},"model_info":{"general.architecture":"synthetic","synthetic.context_length":49152},
        "capabilities":["completion","tools","thinking"]});
    let provenance = "synthetic-boolean-fixture-v1; no native capability evidence".to_owned();
    let digest = "b".repeat(64);
    let backend = playtime_ollama::Profile::VERSION.to_owned();
    RegisteredModel {
        schema_version: 1,
        summary: ModelRegistrationSummary {
            registration_id: ModelRegistrationId::new("model-synthetic-boolean").unwrap(),
            identity: RegisteredModelIdentity {
                schema_version: 1,
                name: "synthetic-boolean:fixture".into(),
                digest: digest.clone(),
                backend_version: backend.clone(),
                definition_sha256: json::hash(
                    &json::canonical(&definition, MODEL_DEFINITION_BYTES).unwrap(),
                ),
                advertised_capabilities: vec![
                    "completion".into(),
                    "tools".into(),
                    "thinking".into(),
                ],
                capability_provenance: provenance.clone(),
                reviewed_capabilities: Some(RecipeModelCapabilities {
                    model_digest: digest,
                    backend_version: backend,
                    provenance,
                    reasoning: vec![
                        RecipeReasoning::ModelDefault,
                        RecipeReasoning::On,
                        RecipeReasoning::Off,
                    ],
                    default_reasoning: Some(RecipeReasoning::Off),
                    context_ceiling: Some(49152),
                }),
            },
        },
        definition,
    }
}
/// Seed an isolated fixture database with a historical reviewed capability record.
/// This deliberately bypasses live registration: no second native profile is approved here.
pub fn retain(root: &Path, model: &RegisteredModel) {
    let mut db = rusqlite::Connection::open(root.join(".playtime/service.sqlite3")).unwrap();
    let tx = db.transaction().unwrap();
    let record = serde_json::to_string(model).unwrap();
    let identity = json::canonical(
        &serde_json::to_value(&model.summary.identity).unwrap(),
        12 << 10,
    )
    .unwrap();
    let request = ModelRegistrationRequest {
        endpoint_binding: "e".repeat(64),
        identity: model.summary.identity.clone(),
    };
    tx.execute(
        "INSERT INTO model_registrations(id,identity_hash,digest,record) VALUES(?1,?2,?3,?4)",
        rusqlite::params![
            model.summary.registration_id.as_str(),
            json::hash(&identity),
            json::hash(record.as_bytes()),
            record
        ],
    )
    .unwrap();
    tx.execute(
        "INSERT INTO model_acceptances VALUES(?1,?2,?3)",
        rusqlite::params![
            format!("fixture-{}", model.summary.registration_id),
            serde_json::to_string(&request).unwrap(),
            model.summary.registration_id.as_str()
        ],
    )
    .unwrap();
    tx.commit().unwrap();
}
