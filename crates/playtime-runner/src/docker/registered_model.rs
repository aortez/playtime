//! Execution approval is compiled capability evidence, never a stored claim alone.
use super::{LiveModel, MODEL_LOCK, reviewed_model_capabilities};
use crate::{Error, Result};
use playtime_protocol::api::*;
use serde_json::{Value, json};

pub const QWEN_MODEL_LOCK: &[u8] = include_bytes!("qwen3-1.7b-lock.json");

pub fn reviewed_identity_capabilities(
    name: &str,
    digest: &str,
    backend: &str,
    definition_sha256: &str,
) -> Option<RecipeModelCapabilities> {
    for bytes in [MODEL_LOCK, QWEN_MODEL_LOCK] {
        let lock: Value = serde_json::from_slice(bytes).ok()?;
        if lock["name"].as_str() != Some(name)
            || lock["digest"].as_str() != Some(digest)
            || lock["ollama_version"].as_str() != Some(backend)
            || lock["definition_sha256"].as_str() != Some(definition_sha256)
        {
            continue;
        }
        if bytes == MODEL_LOCK {
            return reviewed_model_capabilities(&lock).ok();
        }
        return Some(RecipeModelCapabilities {
            model_digest: digest.into(), backend_version: backend.into(),
            provenance: "reviewed-qwen3-1.7b-ollama-0.32.3-v1; boolean thinking and tools checked on CPU; context ceiling from locked model_info".into(),
            reasoning: vec![RecipeReasoning::ModelDefault, RecipeReasoning::On, RecipeReasoning::Off],
            default_reasoning: None,
            context_ceiling: lock.pointer("/definition/model_info/qwen3.context_length").and_then(Value::as_u64),
        });
    }
    None
}

pub(super) fn execution_model(model: &RegisteredModel) -> Result<playtime_ollama::ExecutionModel> {
    let id = &model.summary.identity;
    let error =
        || Error::Invalid("selected model identity or capability evidence is unsupported".into());
    let capabilities = reviewed_identity_capabilities(
        &id.name,
        &id.digest,
        &id.backend_version,
        &id.definition_sha256,
    )
    .ok_or_else(error)?;
    if model.schema_version != 1
        || !id.valid()
        || id.reviewed_capabilities.as_ref() != Some(&capabilities)
        || id.capability_provenance != capabilities.provenance
        || model.definition["capabilities"] != serde_json::to_value(&id.advertised_capabilities)?
    {
        return Err(error());
    }
    let model = playtime_ollama::ExecutionModel {
        name: id.name.clone(),
        digest: id.digest.clone(),
        backend_version: id.backend_version.clone(),
        definition_sha256: id.definition_sha256.clone(),
        definition: model.definition.clone(),
        reasoning: capabilities
            .reasoning
            .iter()
            .map(|choice| match choice {
                RecipeReasoning::ModelDefault => None,
                RecipeReasoning::Low => Some(playtime_ollama::Thinking::Level("low".into())),
                RecipeReasoning::Medium => Some(playtime_ollama::Thinking::Level("medium".into())),
                RecipeReasoning::High => Some(playtime_ollama::Thinking::Level("high".into())),
                RecipeReasoning::On => Some(playtime_ollama::Thinking::Boolean(true)),
                RecipeReasoning::Off => Some(playtime_ollama::Thinking::Boolean(false)),
            })
            .collect(),
    };
    model.validate().map_err(|_| error())?;
    Ok(model)
}
pub(super) fn selected_lock(model: &RegisteredModel) -> Value {
    let id = &model.summary.identity;
    // Generated host-local Modelfile paths are not execution identity.
    json!({"name":id.name,"digest":id.digest,"ollama_version":id.backend_version,
        "definition_sha256":id.definition_sha256,"definition":model.definition,"modelfile":""})
}
impl LiveModel {
    /// Recheck compiled approval even when a worker reads an internally consistent
    /// but unsupported stored record. Retained JSON never approves a new profile.
    pub fn execution_model(&self) -> Result<playtime_ollama::ExecutionModel> {
        if let Some(summary) = &self.registration {
            let model = RegisteredModel {
                schema_version: 1,
                summary: summary.clone(),
                definition: self.lock["definition"].clone(),
            };
            if self.lock != selected_lock(&model) {
                return Err(Error::Invalid(
                    "selected model differs from execution lock".into(),
                ));
            }
            execution_model(&model)
        } else {
            let lock = playtime_ollama::ModelLock::parse(&serde_json::to_vec(&self.lock)?)
                .map_err(|e| Error::Invalid(e.to_string()))?;
            playtime_ollama::ExecutionModel::locked(&lock)
                .map_err(|e| Error::Invalid(e.to_string()))
        }
    }
}
