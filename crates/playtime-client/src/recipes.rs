//! Shared authoring operations and private durable save receipts.
use crate::{Client, Error, Result, ambiguous, config::normalize_endpoint, state::StateError};
use playtime_local::PrivateDirectory;
use playtime_protocol::api::*;
use reqwest::Method;
use serde::{Deserialize, Serialize};
use std::{
    io,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingRecipeSave {
    pub schema_version: u32,
    pub service_id: ServiceId,
    pub key: IdempotencyKey,
    pub request: RecipeMutation,
}
impl Client {
    pub async fn recipe_model_settings(
        &self,
        definition: &RecipeDefinition,
    ) -> Result<RecipeSettingsView> {
        let value: RecipeSettingsView = self
            .json(self.request(
                Method::GET,
                &format!("recipes/{}/model-settings", definition.execution_recipe_id),
            ))
            .await?;
        if value.execution_recipe_id != definition.execution_recipe_id
            || value.definition_digest != definition.definition_digest
            || serde_json::to_value(&value.settings).map_err(|_| Error::InvalidResponse)?
                != definition_settings(definition)
            || model_identity(definition)
                != (
                    Some(value.capabilities.model_digest.as_str()),
                    Some(value.capabilities.backend_version.as_str()),
                )
            || definition.inherited["recipe"]["parameters"]["limits"]["generated_tokens"].as_u64()
                != Some(value.generated_tokens)
        {
            return Err(Error::InvalidResponse);
        }
        Ok(value)
    }

    pub async fn recipe_definition(&self, id: &RecipeId) -> Result<RecipeDefinition> {
        let value: RecipeDefinition = self
            .json(self.request(Method::GET, &format!("recipes/{id}/definition")))
            .await?;
        validate_definition(&value)?;
        if value.execution_recipe_id != *id {
            return Err(Error::InvalidResponse);
        }
        Ok(value)
    }
    pub async fn saved_recipes(&self, query: &PageQuery) -> Result<Page<Recipe>> {
        self.json(self.request(Method::GET, "saved-recipes").query(query))
            .await
    }
    pub async fn recipe_revisions(&self, id: &RecipeId, query: &PageQuery) -> Result<Page<Recipe>> {
        self.json(
            self.request(Method::GET, &format!("saved-recipes/{id}/revisions"))
                .query(query),
        )
        .await
    }
    pub fn prepare_recipe_save(&self, request: RecipeMutation) -> Result<PendingRecipeSave> {
        self.check_harness_selection(&request)?;
        let mut bytes = [0; 24];
        getrandom::fill(&mut bytes).map_err(|_| Error::Io)?;
        Ok(PendingRecipeSave {
            schema_version: 1,
            service_id: self.service.clone(),
            key: IdempotencyKey::new(bytes.iter().map(|b| format!("{b:02x}")).collect::<String>())
                .unwrap(),
            request,
        })
    }
    fn check_recipe_save(&self, pending: &PendingRecipeSave) -> Result<()> {
        if pending.schema_version != 1 {
            return Err(Error::InvalidResponse);
        }
        if pending.service_id != self.service {
            return Err(Error::IdentityMismatch);
        }
        Ok(())
    }
    pub async fn save_recipe(&self, pending: &PendingRecipeSave) -> Result<RecipeSaveResponse> {
        self.check_recipe_save(pending)?;
        self.check_harness_selection(&pending.request)?;
        let request = match &pending.request {
            RecipeMutation::Clone { request } => {
                self.request(Method::POST, "saved-recipes").json(request)
            }
            RecipeMutation::Revise { recipe_id, request } => self
                .request(
                    Method::POST,
                    &format!("saved-recipes/{recipe_id}/revisions"),
                )
                .json(request),
        }
        .header("idempotency-key", pending.key.as_str());
        let response = ambiguous(self.json(request).await)?;
        ambiguous(validate_save(&response, pending))?;
        Ok(response)
    }
    pub async fn lookup_recipe_save(
        &self,
        pending: &PendingRecipeSave,
    ) -> Result<RecipeSaveResponse> {
        self.check_recipe_save(pending)?;
        let response = self
            .json(self.request(Method::GET, &format!("recipe-saves/{}", pending.key)))
            .await?;
        validate_save(&response, pending)?;
        Ok(response)
    }
    fn check_harness_selection(&self, request: &RecipeMutation) -> Result<()> {
        if selected_harness(request).is_some()
            && !self
                .capabilities()
                .features
                .iter()
                .any(|f| f == RECIPE_HARNESS_SELECTION)
        {
            return Err(Error::UnsupportedCapability);
        }
        Ok(())
    }
}
fn selected_harness(request: &RecipeMutation) -> Option<&HarnessId> {
    match request {
        RecipeMutation::Clone { request } => request.harness.as_ref(),
        RecipeMutation::Revise { request, .. } => request.harness.as_ref(),
    }
}
fn model_identity(definition: &RecipeDefinition) -> (Option<&str>, Option<&str>) {
    if let Some(model) = &definition.model_registration {
        (
            Some(model.identity.digest.as_str()),
            Some(model.identity.backend_version.as_str()),
        )
    } else {
        (
            definition.inherited["model_lock"]["digest"].as_str(),
            definition.inherited["model_lock"]["ollama_version"].as_str(),
        )
    }
}
fn is_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}
fn validate_definition(value: &RecipeDefinition) -> Result<()> {
    if !matches!(
        (value.schema_version, value.model_registration.is_some()),
        (1, false) | (2, true)
    ) || !is_digest(&value.definition_digest)
        || value.name.is_empty()
        || value.name.len() > 128
        || value.prompt.len() > 65536
        || !value.inherited.is_object()
        || value.seeds.len() > 16
        || value.seeds.iter().any(|s| !is_digest(&s.sha256))
        || value.model_registration.as_ref().is_some_and(|m| {
            !m.identity.valid()
                || serde_json::to_value(m).ok().as_ref()
                    != value.inherited.pointer("/model_registration/summary")
        })
        || (value.model_registration.is_none()
            && value
                .inherited
                .get("model_registration")
                .is_some_and(|m| !m.is_null()))
        || value.saved.as_ref().is_some_and(|s| {
            s.execution_recipe_id != value.execution_recipe_id
                || s.definition_digest != value.definition_digest
                || s.name != value.name
                || s.revision == 0
                || s.creation_sequence == 0
        })
    {
        return Err(Error::InvalidResponse);
    }
    Ok(())
}
fn validate_save(response: &RecipeSaveResponse, pending: &PendingRecipeSave) -> Result<()> {
    validate_definition(&response.definition)?;
    let definition = &response.definition;
    let saved = definition.saved.as_ref().ok_or(Error::InvalidResponse)?;
    let source = definition.source.as_ref().ok_or(Error::InvalidResponse)?;
    if response.service_id != pending.service_id
        || response.key != pending.key
        || response.request != pending.request
    {
        return Err(Error::InvalidResponse);
    }
    let (name, prompt) = match &pending.request {
        RecipeMutation::Clone { request } => {
            if source != &request.source || saved.revision != 1 {
                return Err(Error::InvalidResponse);
            }
            (&request.name, &request.prompt)
        }
        RecipeMutation::Revise { recipe_id, request } => {
            if saved.recipe_id != *recipe_id
                || source.execution_recipe_id != request.expected_head
                || saved.revision < 2
            {
                return Err(Error::InvalidResponse);
            }
            (&request.name, &request.prompt)
        }
    };
    let model = match &pending.request {
        RecipeMutation::Clone { request } => &request.model_registration,
        RecipeMutation::Revise { request, .. } => &request.model_registration,
    };
    if selected_harness(&pending.request)
        .is_some_and(|id| definition.inherited["harness"].as_str() != Some(id.as_str()))
    {
        return Err(Error::InvalidResponse);
    }
    if model.as_ref().is_some_and(|id| {
        definition
            .model_registration
            .as_ref()
            .map(|m| &m.registration_id)
            != Some(id)
    }) {
        return Err(Error::InvalidResponse);
    }
    let settings = match &pending.request {
        RecipeMutation::Clone { request } => &request.model_settings,
        RecipeMutation::Revise { request, .. } => &request.model_settings,
    };
    if let Some(settings) = settings {
        let expected = definition_settings(definition);
        if serde_json::to_value(settings).map_err(|_| Error::InvalidResponse)? != expected {
            return Err(Error::InvalidResponse);
        }
    }
    if &definition.name != name || &definition.prompt != prompt {
        return Err(Error::InvalidResponse);
    }
    Ok(())
}

fn definition_settings(definition: &RecipeDefinition) -> serde_json::Value {
    let recipe = &definition.inherited["recipe"];
    let m = &recipe["model"];
    let p = &recipe["parameters"];
    let expected = serde_json::json!({
        "reasoning": match m["thinking"].as_str() { Some("default") => "model_default", Some(v) => v, None => "" },
        "temperature": m["temperature"], "context_tokens": p["limits"]["context_tokens"],
        "tokens_per_call": p["limits"]["tokens_per_call"], "handoff_tokens": p["limits"]["handoff_tokens"],
        "top_p": m["top_p"], "top_k": m["top_k"], "repeat_penalty": m["repeat_penalty"], "sampling_seed": p["sampling_seed"],
    });
    expected
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecipeSaveReceipt {
    pub pending: PendingRecipeSave,
    pub endpoint: String,
    pub saved_execution: Option<RecipeId>,
}
impl RecipeSaveReceipt {
    pub fn check_target(
        &self,
        service: &ServiceId,
        endpoint: &str,
    ) -> std::result::Result<(), StateError> {
        if self.pending.schema_version != 1 {
            return Err(StateError::Invalid);
        }
        if self.pending.service_id != *service {
            return Err(StateError::IdentityMismatch);
        }
        if normalize_endpoint(endpoint).map_err(|_| StateError::Invalid)? != self.endpoint {
            return Err(StateError::EndpointChanged);
        }
        Ok(())
    }
}
pub struct RecipeSaveStore {
    directory: PathBuf,
}
impl RecipeSaveStore {
    pub fn new(state: &Path) -> Self {
        Self {
            directory: state.join("recipe-saves"),
        }
    }
    fn open(&self, create: bool) -> io::Result<PrivateDirectory> {
        if create {
            PrivateDirectory::open(self.directory.parent().unwrap(), true)?;
        }
        PrivateDirectory::open(&self.directory, create)
    }
    pub fn save(
        &self,
        pending: &PendingRecipeSave,
        endpoint: &str,
    ) -> std::result::Result<RecipeSaveReceipt, StateError> {
        let directory = self.open(true)?;
        let _lock = directory.lock()?;
        if let Some(old) = read(&directory, &pending.key)? {
            if old.pending != *pending {
                return Err(StateError::Conflict);
            }
            return Ok(old);
        }
        if pending.schema_version != 1 {
            return Err(StateError::Invalid);
        }
        let receipt = RecipeSaveReceipt {
            pending: pending.clone(),
            endpoint: normalize_endpoint(endpoint).map_err(|_| StateError::Invalid)?,
            saved_execution: None,
        };
        let bytes = serde_json::to_vec(&receipt).map_err(|_| StateError::Invalid)?;
        if bytes.len() > 1 << 20 {
            return Err(StateError::Invalid);
        }
        directory.write_new(&format!("{}.json", pending.key), &bytes)?;
        Ok(receipt)
    }
    pub fn read(
        &self,
        key: &IdempotencyKey,
    ) -> std::result::Result<Option<RecipeSaveReceipt>, StateError> {
        let directory = self.open(false)?;
        let _lock = directory.lock()?;
        read(&directory, key)
    }
    pub fn acknowledge(
        &self,
        receipt: &RecipeSaveReceipt,
        response: &RecipeSaveResponse,
    ) -> std::result::Result<RecipeSaveReceipt, StateError> {
        validate_save(response, &receipt.pending).map_err(|_| StateError::Invalid)?;
        let directory = self.open(false)?;
        let _lock = directory.lock()?;
        let mut stored = read(&directory, &receipt.pending.key)?.ok_or(StateError::Invalid)?;
        if stored.pending != receipt.pending
            || stored
                .saved_execution
                .as_ref()
                .is_some_and(|id| id != &response.definition.execution_recipe_id)
        {
            return Err(StateError::Conflict);
        }
        stored.saved_execution = Some(response.definition.execution_recipe_id.clone());
        directory.replace(
            &format!("{}.json", receipt.pending.key),
            &serde_json::to_vec(&stored).map_err(|_| StateError::Invalid)?,
        )?;
        Ok(stored)
    }
    /// Only a definitive refusal of the first submission may discard a receipt.
    pub fn discard(&self, receipt: &RecipeSaveReceipt) -> std::result::Result<(), StateError> {
        let directory = self.open(false)?;
        let _lock = directory.lock()?;
        if let Some(stored) = read(&directory, &receipt.pending.key)? {
            if stored.pending != receipt.pending || stored.saved_execution.is_some() {
                return Err(StateError::Conflict);
            }
            directory.remove(&format!("{}.json", receipt.pending.key))?;
        }
        Ok(())
    }
    pub fn list(&self) -> std::result::Result<Vec<RecipeSaveReceipt>, StateError> {
        self.list_service(None)
    }
    /// Apply service scope before the page bound, so other services' pending
    /// receipts cannot hide recovery for the connected service.
    pub fn list_service(
        &self,
        service: Option<&ServiceId>,
    ) -> std::result::Result<Vec<RecipeSaveReceipt>, StateError> {
        let directory = match self.open(false) {
            Ok(d) => d,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => return Err(e.into()),
        };
        let _lock = directory.lock()?;
        let mut receipts = Vec::new();
        for name in directory.names()? {
            let key = IdempotencyKey::new(name.strip_suffix(".json").ok_or(StateError::Invalid)?)
                .map_err(|_| StateError::Invalid)?;
            let receipt = read(&directory, &key)?.ok_or(StateError::Invalid)?;
            if receipt.saved_execution.is_none()
                && service.is_none_or(|id| *id == receipt.pending.service_id)
            {
                receipts.push(receipt);
            }
            if receipts.len() >= 200 {
                break;
            }
        }
        Ok(receipts)
    }
}
fn read(
    directory: &PrivateDirectory,
    key: &IdempotencyKey,
) -> std::result::Result<Option<RecipeSaveReceipt>, StateError> {
    let bytes = match directory.read(&format!("{key}.json"), 1 << 20) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let receipt: RecipeSaveReceipt =
        serde_json::from_slice(&bytes).map_err(|_| StateError::Invalid)?;
    if receipt.pending.schema_version != 1
        || receipt.pending.key != *key
        || normalize_endpoint(&receipt.endpoint).map_err(|_| StateError::Invalid)?
            != receipt.endpoint
    {
        return Err(StateError::Invalid);
    }
    Ok(Some(receipt))
}
