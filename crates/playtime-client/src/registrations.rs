//! Explicit registration and private durable recovery receipts, shared by clients.
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
pub struct PendingModelRegistration {
    pub schema_version: u32,
    pub service_id: ServiceId,
    pub key: IdempotencyKey,
    pub request: ModelRegistrationRequest,
}
impl Client {
    fn registration_capability(&self) -> Result<()> {
        if self
            .capabilities()
            .features
            .iter()
            .any(|f| f == MODEL_REGISTRATION)
        {
            return Ok(());
        }
        Err(Error::Api {
            status: 422,
            response: playtime_protocol::v1::ErrorResponse {
                code: "unsupported_capability".into(),
                message: "Model registration unavailable".into(),
                request_id: None,
                details: Default::default(),
            },
        })
    }
    pub async fn review_model(
        &self,
        request: &ModelInspectRequest,
    ) -> Result<ModelRegistrationReview> {
        self.registration_capability()?;
        if !crate::models::name(&request.name)
            || request
                .expected_digest
                .as_deref()
                .is_some_and(|s| !crate::models::digest(s))
        {
            return Err(Error::InvalidResponse);
        }
        let review: ModelRegistrationReview = self
            .json(self.request(Method::POST, "models/review").json(request))
            .await?;
        crate::models::validate_inspection(&review.inspection, request)?;
        if RegisteredModelIdentity::from_inspection(&review.inspection).as_ref()
            != review.request.as_ref().map(|r| &r.identity)
            || review.request.as_ref().is_some_and(|r| !r.valid())
        {
            return Err(Error::InvalidResponse);
        }
        Ok(review)
    }
    pub async fn registered_models(&self) -> Result<ModelRegistrations> {
        self.registration_capability()?;
        let value: ModelRegistrations = self
            .json(self.request(Method::GET, "models/registrations"))
            .await?;
        let mut ids = std::collections::BTreeSet::new();
        if value.schema_version != 1
            || value.items.len() > MODEL_REGISTRATION_LIMIT
            || value
                .items
                .iter()
                .any(|s| !s.identity.valid() || !ids.insert(&s.registration_id))
        {
            return Err(Error::InvalidResponse);
        }
        Ok(value)
    }
    pub async fn registered_model(&self, id: &ModelRegistrationId) -> Result<RegisteredModel> {
        self.registration_capability()?;
        let value: RegisteredModel = self
            .json(self.request(Method::GET, &format!("models/registrations/{id}")))
            .await?;
        if value.schema_version != 1
            || value.summary.registration_id != *id
            || !value.summary.identity.valid()
            || !value.definition.is_object()
        {
            return Err(Error::InvalidResponse);
        }
        Ok(value)
    }
    pub fn prepare_model_registration(
        &self,
        request: ModelRegistrationRequest,
    ) -> Result<PendingModelRegistration> {
        self.registration_capability()?;
        if !request.valid() {
            return Err(Error::InvalidResponse);
        }
        let mut bytes = [0; 24];
        getrandom::fill(&mut bytes).map_err(|_| Error::Io)?;
        Ok(PendingModelRegistration {
            schema_version: 1,
            service_id: self.service.clone(),
            key: IdempotencyKey::new(bytes.iter().map(|b| format!("{b:02x}")).collect::<String>())
                .unwrap(),
            request,
        })
    }
    fn check_registration(&self, pending: &PendingModelRegistration) -> Result<()> {
        self.registration_capability()?;
        if pending.schema_version != 1 || !pending.request.valid() {
            return Err(Error::InvalidResponse);
        }
        if pending.service_id != self.service {
            return Err(Error::IdentityMismatch);
        }
        Ok(())
    }
    pub async fn register_model(
        &self,
        pending: &PendingModelRegistration,
    ) -> Result<ModelRegistrationResponse> {
        self.check_registration(pending)?;
        let response = ambiguous(
            self.json(
                self.request(Method::POST, "models/registrations")
                    .header("idempotency-key", pending.key.as_str())
                    .json(&pending.request),
            )
            .await,
        )?;
        ambiguous(validate_registration(&response, pending))?;
        Ok(response)
    }
    pub async fn lookup_model_registration(
        &self,
        pending: &PendingModelRegistration,
    ) -> Result<ModelRegistrationResponse> {
        self.check_registration(pending)?;
        let response = self
            .json(self.request(
                Method::GET,
                &format!("model-registration-requests/{}", pending.key),
            ))
            .await?;
        validate_registration(&response, pending)?;
        Ok(response)
    }
}
fn validate_registration(
    response: &ModelRegistrationResponse,
    pending: &PendingModelRegistration,
) -> Result<()> {
    if response.service_id != pending.service_id
        || response.key != pending.key
        || response.request != pending.request
        || response.registration.identity != pending.request.identity
        || !response.request.valid()
    {
        return Err(Error::InvalidResponse);
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelRegistrationReceipt {
    pub pending: PendingModelRegistration,
    pub endpoint: String,
    pub registered_model: Option<ModelRegistrationId>,
}
impl ModelRegistrationReceipt {
    pub fn check_target(
        &self,
        service: &ServiceId,
        endpoint: &str,
    ) -> std::result::Result<(), StateError> {
        if self.pending.schema_version != 1 || !self.pending.request.valid() {
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
pub struct ModelRegistrationStore {
    directory: PathBuf,
}
impl ModelRegistrationStore {
    pub fn new(state: &Path) -> Self {
        Self {
            directory: state.join("model-registrations"),
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
        pending: &PendingModelRegistration,
        endpoint: &str,
    ) -> std::result::Result<ModelRegistrationReceipt, StateError> {
        let directory = self.open(true)?;
        let _lock = directory.lock()?;
        if let Some(old) = read(&directory, &pending.key)? {
            if old.pending != *pending {
                return Err(StateError::Conflict);
            }
            old.check_target(&pending.service_id, endpoint)?;
            return Ok(old);
        }
        if pending.schema_version != 1 || !pending.request.valid() {
            return Err(StateError::Invalid);
        }
        let receipt = ModelRegistrationReceipt {
            pending: pending.clone(),
            endpoint: normalize_endpoint(endpoint).map_err(|_| StateError::Invalid)?,
            registered_model: None,
        };
        let bytes = serde_json::to_vec(&receipt).map_err(|_| StateError::Invalid)?;
        if bytes.len() > 32 << 10 {
            return Err(StateError::Invalid);
        }
        directory.write_new(&format!("{}.json", pending.key), &bytes)?;
        Ok(receipt)
    }
    pub fn read(
        &self,
        key: &IdempotencyKey,
    ) -> std::result::Result<Option<ModelRegistrationReceipt>, StateError> {
        let directory = self.open(false)?;
        let _lock = directory.lock()?;
        read(&directory, key)
    }
    pub fn acknowledge(
        &self,
        receipt: &ModelRegistrationReceipt,
        response: &ModelRegistrationResponse,
    ) -> std::result::Result<ModelRegistrationReceipt, StateError> {
        validate_registration(response, &receipt.pending).map_err(|_| StateError::Invalid)?;
        let directory = self.open(false)?;
        let _lock = directory.lock()?;
        let mut stored = read(&directory, &receipt.pending.key)?.ok_or(StateError::Invalid)?;
        if stored.pending != receipt.pending
            || stored.endpoint != receipt.endpoint
            || stored
                .registered_model
                .as_ref()
                .is_some_and(|id| id != &response.registration.registration_id)
        {
            return Err(StateError::Conflict);
        }
        stored.registered_model = Some(response.registration.registration_id.clone());
        directory.replace(
            &format!("{}.json", receipt.pending.key),
            &serde_json::to_vec(&stored).map_err(|_| StateError::Invalid)?,
        )?;
        Ok(stored)
    }
    /// Only a definitive refusal of the first submission may discard a receipt.
    pub fn discard(
        &self,
        receipt: &ModelRegistrationReceipt,
    ) -> std::result::Result<(), StateError> {
        let directory = self.open(false)?;
        let _lock = directory.lock()?;
        if let Some(stored) = read(&directory, &receipt.pending.key)? {
            if stored.pending != receipt.pending
                || stored.endpoint != receipt.endpoint
                || stored.registered_model.is_some()
            {
                return Err(StateError::Conflict);
            }
            directory.remove(&format!("{}.json", receipt.pending.key))?;
        }
        Ok(())
    }
    pub fn list(&self) -> std::result::Result<Vec<ModelRegistrationReceipt>, StateError> {
        self.list_service(None)
    }
    /// Apply service scope before the page bound, so other services' pending
    /// receipts cannot hide recovery for the connected service.
    pub fn list_service(
        &self,
        service: Option<&ServiceId>,
    ) -> std::result::Result<Vec<ModelRegistrationReceipt>, StateError> {
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
            if receipt.registered_model.is_none()
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
) -> std::result::Result<Option<ModelRegistrationReceipt>, StateError> {
    let bytes = match directory.read(&format!("{key}.json"), 32 << 10) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let receipt: ModelRegistrationReceipt =
        serde_json::from_slice(&bytes).map_err(|_| StateError::Invalid)?;
    if receipt.pending.schema_version != 1
        || !receipt.pending.request.valid()
        || receipt.pending.key != *key
        || normalize_endpoint(&receipt.endpoint).map_err(|_| StateError::Invalid)?
            != receipt.endpoint
    {
        return Err(StateError::Invalid);
    }
    Ok(Some(receipt))
}
