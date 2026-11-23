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
        self.check_request_features(&request)?;
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
        self.check_request_features(&pending.request)?;
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
    fn check_request_features(&self, request: &RecipeMutation) -> Result<()> {
        check_request_features(&self.capabilities().features, request)
    }
}
/// Refuses harness and seed-file fields that the service doesn't advertise.
fn check_request_features(features: &[String], request: &RecipeMutation) -> Result<()> {
    if request.seeds().is_some() && !features.iter().any(|f| f == RECIPE_SEED_FILES) {
        return Err(Error::UnsupportedCapability);
    }
    if (compaction_settings(request).is_some()
        || selected_harness(request).is_some_and(|id| id.as_str() == CONTINUOUS_COMPACTION_V1))
        && !features.iter().any(|f| f == RECIPE_COMPACTION_SETTINGS)
    {
        return Err(Error::UnsupportedCapability);
    }
    if compaction_settings(request).is_some_and(|s| !s.allow_finish_run)
        && !features.iter().any(|f| f == RECIPE_COMPACTION_FINISH)
    {
        return Err(Error::UnsupportedCapability);
    }
    if selected_harness(request).is_some()
        && !features.iter().any(|f| f == RECIPE_HARNESS_SELECTION)
    {
        return Err(Error::UnsupportedCapability);
    }
    Ok(())
}
fn compaction_settings(request: &RecipeMutation) -> Option<&CompactionSettings> {
    match request {
        RecipeMutation::Clone { request } => request.compaction.as_ref(),
        RecipeMutation::Revise { request, .. } => request.compaction.as_ref(),
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
        (1, false) | (2, true) | (3, _)
    ) || !is_digest(&value.definition_digest)
        || value.name.is_empty()
        || value.name.len() > 128
        || value.prompt.len() > 65536
        || !value.inherited.is_object()
        || value.seeds.len() > SEED_FILES
        || value.seeds.iter().any(|s| !is_digest(&s.sha256))
        || value.seed_origin == Some(SeedOrigin::Empty) && !value.seeds.is_empty()
        || value.seed_origin == Some(SeedOrigin::Uploaded) && value.seeds.is_empty()
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
    if (value.schema_version == 3) != value.inherited.get("compaction").is_some() {
        return Err(Error::InvalidResponse);
    }
    if value.schema_version == 3
        && (value.inherited["harness"] != CONTINUOUS_COMPACTION_V1
            || HarnessView::from_definition(&value.inherited).is_err())
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
    // A confirmed receipt keeps no seed contents. The definition's seed list
    // still has to match each file's size and digest below.
    if response.service_id != pending.service_id
        || response.key != pending.key
        || without_contents(&response.request) != without_contents(&pending.request)
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
    if let Some(settings) = compaction_settings(&pending.request)
        && definition.inherited["compaction"]["settings"]
            != serde_json::to_value(settings).map_err(|_| Error::InvalidResponse)?
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
    let seeds_match = match pending.request.seeds() {
        None => true,
        Some(SeedSelection::Empty) => {
            definition.seeds.is_empty() && definition.seed_origin == Some(SeedOrigin::Empty)
        }
        Some(SeedSelection::Uploaded { files }) => {
            let mut sent: Vec<_> = files
                .iter()
                .map(|f| RecipeSeed {
                    path: f.path.clone(),
                    size_bytes: f.size_bytes,
                    sha256: f.sha256.clone(),
                })
                .collect();
            sent.sort_by(|a, b| a.path.cmp(&b.path));
            definition.seeds == sent && definition.seed_origin == Some(SeedOrigin::Uploaded)
        }
    };
    if !seeds_match {
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
/// A receipt holds its whole request, seed files included, until the save is
/// confirmed. The service accepts requests of up to 4 MiB.
const RECEIPT_BYTES: u64 = 5 << 20;

/// A request with each uploaded seed file's path, size and digest but not its
/// contents.
fn without_contents(request: &RecipeMutation) -> RecipeMutation {
    let mut request = request.clone();
    let seeds = match &mut request {
        RecipeMutation::Clone { request } => &mut request.seeds,
        RecipeMutation::Revise { request, .. } => &mut request.seeds,
    };
    if let Some(SeedSelection::Uploaded { files }) = seeds {
        for file in files {
            file.content.clear();
        }
    }
    request
}

/// A confirmed save is only looked up, never sent again, so its receipt drops
/// the seed contents.
fn confirmed(pending: &PendingRecipeSave) -> PendingRecipeSave {
    PendingRecipeSave {
        request: without_contents(&pending.request),
        ..pending.clone()
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
            let same = if old.saved_execution.is_some() {
                old.pending == confirmed(pending)
            } else {
                old.pending == *pending
            };
            if !same {
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
        if bytes.len() as u64 > RECEIPT_BYTES {
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
        if confirmed(&stored.pending) != confirmed(&receipt.pending)
            || stored
                .saved_execution
                .as_ref()
                .is_some_and(|id| id != &response.definition.execution_recipe_id)
        {
            return Err(StateError::Conflict);
        }
        stored.pending = confirmed(&stored.pending);
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
    let bytes = match directory.read(&format!("{key}.json"), RECEIPT_BYTES) {
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn leaving_out_finish_run_needs_a_service_that_offers_the_choice() {
        let revise = |allow_finish_run| RecipeMutation::Revise {
            recipe_id: RecipeId::new("continuous").unwrap(),
            request: ReviseRecipeRequest {
                seeds: None,
                compaction: Some(CompactionSettings {
                    allow_finish_run,
                    ..Default::default()
                }),
                harness: None,
                expected_head: RecipeId::new("continuous").unwrap(),
                name: "Continuous".into(),
                prompt: "Explore.".into(),
                model_settings: None,
                model_registration: None,
            },
        };
        let older = [RECIPE_COMPACTION_SETTINGS.to_owned()];
        check_request_features(&older, &revise(true)).unwrap();
        assert!(matches!(
            check_request_features(&older, &revise(false)),
            Err(Error::UnsupportedCapability)
        ));
        let current = [older[0].clone(), RECIPE_COMPACTION_FINISH.to_owned()];
        check_request_features(&current, &revise(false)).unwrap();
    }
    #[test]
    fn seed_files_need_a_service_that_offers_them() {
        let clone = |seeds| RecipeMutation::Clone {
            request: CloneRecipeRequest {
                compaction: None,
                harness: None,
                model_registration: None,
                model_settings: None,
                source: RecipeSource {
                    execution_recipe_id: RecipeId::new("curiosity").unwrap(),
                    definition_digest: "a".repeat(64),
                },
                name: "Seeded".into(),
                prompt: "Explore.".into(),
                seeds,
            },
        };
        check_request_features(&[], &clone(None)).unwrap();
        assert!(matches!(
            check_request_features(&[], &clone(Some(SeedSelection::Empty))),
            Err(Error::UnsupportedCapability)
        ));
        let current = [RECIPE_SEED_FILES.to_owned()];
        check_request_features(&current, &clone(Some(SeedSelection::Empty))).unwrap();
    }
}
