//! Immutable model registrations and keyed acceptance, separate from recipe saves.
use crate::{
    application::{ApiError, Result},
    lifecycle::Ledger,
    models::ModelBrowser,
};
use playtime_ollama::json;
use playtime_protocol::api::*;
use playtime_worker_control::random_id;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use std::sync::Arc;
use tokio::sync::Semaphore;

const RECORD_BYTES: usize = 2 << 20;
// Leaves every bounded list (256 summaries) below the default 4 MiB HTTP limit.
const REQUEST_BYTES: usize = 12 << 10;
const REGISTRY_BYTES: u64 = 128 << 20;
const ACCEPTANCES: u64 = 4096;
pub(crate) fn migrate(db: &Connection) -> rusqlite::Result<()> {
    db.execute_batch(
        "CREATE TABLE model_registrations (
        seq INTEGER PRIMARY KEY AUTOINCREMENT, id TEXT NOT NULL UNIQUE,
        identity_hash TEXT NOT NULL UNIQUE, digest TEXT NOT NULL, record TEXT NOT NULL);
        CREATE TABLE model_acceptances (key TEXT PRIMARY KEY, request TEXT NOT NULL,
        registration TEXT NOT NULL REFERENCES model_registrations(id));",
    )
}
fn corrupt() -> ApiError {
    ApiError::new("model_registry_corrupt")
}
fn database(error: rusqlite::Error) -> ApiError {
    use rusqlite::ErrorCode;
    if matches!(
        error.sqlite_error_code(),
        Some(
            ErrorCode::DatabaseBusy
                | ErrorCode::DatabaseLocked
                | ErrorCode::DiskFull
                | ErrorCode::SystemIoFailure
                | ErrorCode::CannotOpen
                | ErrorCode::ReadOnly
                | ErrorCode::OperationInterrupted
                | ErrorCode::OutOfMemory
        )
    ) {
        ApiError::new("backend_unavailable")
    } else {
        corrupt()
    }
}
fn encode<T: Serialize>(v: &T, limit: usize) -> Result<String> {
    let bytes = crate::http::bounded_json(v, limit).map_err(|error| {
        if error.code() == "evidence_limit_exceeded" {
            ApiError::new("limit_exceeded")
        } else {
            error
        }
    })?;
    String::from_utf8(bytes).map_err(|_| corrupt())
}
fn identity_hash(identity: &RegisteredModelIdentity) -> Result<String> {
    let value = serde_json::to_value(identity).map_err(|_| corrupt())?;
    json::canonical(&value, REQUEST_BYTES)
        .map(|v| json::hash(&v))
        .map_err(|_| corrupt())
}
fn validate(record: &RegisteredModel) -> Result<()> {
    if record.schema_version != 1
        || !record.summary.identity.valid()
        || !record.definition.is_object()
    {
        return Err(corrupt());
    }
    let bytes =
        json::canonical(&record.definition, MODEL_DEFINITION_BYTES).map_err(|_| corrupt())?;
    if json::hash(&bytes) != record.summary.identity.definition_sha256 {
        return Err(corrupt());
    }
    let fields = record.definition.as_object().unwrap();
    if fields.len() != 5
        || ![
            "template",
            "parameters",
            "model_info",
            "details",
            "capabilities",
        ]
        .iter()
        .all(|k| fields.contains_key(*k))
        || record.definition["capabilities"]
            != serde_json::to_value(&record.summary.identity.advertised_capabilities)
                .map_err(|_| corrupt())?
    {
        return Err(corrupt());
    }
    Ok(())
}
fn integer(row: &rusqlite::Row<'_>, i: usize) -> rusqlite::Result<u64> {
    let value: i64 = row.get(i)?;
    u64::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(i, value))
}
fn budget(db: &Connection) -> Result<(u64, u64, u64)> {
    let (rows, keys, bytes): (u64, u64, u64) = db
        .query_row(
            "SELECT
        (SELECT count(*) FROM model_registrations), (SELECT count(*) FROM model_acceptances),
        (SELECT coalesce(sum(length(CAST(record AS BLOB))),0) FROM model_registrations) +
        (SELECT coalesce(sum(length(CAST(request AS BLOB))),0) FROM model_acceptances)",
            [],
            |r| Ok((integer(r, 0)?, integer(r, 1)?, integer(r, 2)?)),
        )
        .map_err(database)?;
    if rows > MODEL_REGISTRATION_LIMIT as u64 || keys > ACCEPTANCES || bytes > REGISTRY_BYTES {
        return Err(corrupt());
    }
    Ok((rows, keys, bytes))
}
fn record(db: &Connection, id: &ModelRegistrationId) -> Result<RegisteredModel> {
    let row: Option<(String, String, String)> = db
        .query_row(
            "SELECT CASE WHEN length(identity_hash)=64 THEN identity_hash ELSE NULL END, CASE WHEN length(digest)=64 THEN digest ELSE NULL END,
        CASE WHEN length(CAST(record AS BLOB))<=?2 THEN record ELSE NULL END
        FROM model_registrations WHERE id=?1",
            params![id.as_str(), RECORD_BYTES as i64],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()
        .map_err(database)?;
    let (identity, digest, bytes) = row.ok_or_else(|| ApiError::new("not_found"))?;
    if json::hash(bytes.as_bytes()) != digest {
        return Err(corrupt());
    }
    let value: RegisteredModel = serde_json::from_str(&bytes).map_err(|_| corrupt())?;
    validate(&value)?;
    if value.summary.registration_id != *id || identity_hash(&value.summary.identity)? != identity {
        return Err(corrupt());
    }
    Ok(value)
}
fn replay(
    db: &Connection,
    service: &ServiceId,
    key: &IdempotencyKey,
    expected: Option<&ModelRegistrationRequest>,
) -> Result<Option<ModelRegistrationResponse>> {
    let row: Option<(String, String)> = db
        .query_row(
            "SELECT
        CASE WHEN length(CAST(request AS BLOB))<=?2 THEN request ELSE NULL END,
        CASE WHEN length(registration)<=128 THEN registration ELSE NULL END
        FROM model_acceptances WHERE key=?1",
            params![key.as_str(), REQUEST_BYTES as i64],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(database)?;
    let Some((request, id)) = row else {
        return Ok(None);
    };
    let request: ModelRegistrationRequest =
        serde_json::from_str(&request).map_err(|_| corrupt())?;
    if !request.valid() {
        return Err(corrupt());
    }
    let id = ModelRegistrationId::new(id).map_err(|_| corrupt())?;
    let registration = record(db, &id)
        .map_err(|error| {
            if error.code() == "not_found" {
                corrupt()
            } else {
                error
            }
        })?
        .summary;
    if registration.identity != request.identity {
        return Err(corrupt());
    }
    if expected.is_some_and(|e| e != &request) {
        return Err(ApiError::new("idempotency_conflict"));
    }
    Ok(Some(ModelRegistrationResponse {
        service_id: service.clone(),
        key: key.clone(),
        request,
        registration,
        replayed: true,
    }))
}
fn check(db: &Connection) -> Result<Vec<ModelRegistrationSummary>> {
    budget(db)?;
    let mut statement=db.prepare("SELECT CASE WHEN length(id)<=128 AND seq>0 THEN id ELSE NULL END FROM model_registrations ORDER BY seq").map_err(database)?;
    let ids = statement
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(database)?;
    let mut items = Vec::new();
    for id in ids {
        items.push(
            record(
                db,
                &ModelRegistrationId::new(id.map_err(database)?).map_err(|_| corrupt())?,
            )?
            .summary,
        );
    }
    let by_id: std::collections::BTreeMap<_, _> = items
        .iter()
        .map(|s| (&s.registration_id, &s.identity))
        .collect();
    let mut referenced = std::collections::BTreeSet::new();
    let mut statement = db
        .prepare(
            "SELECT CASE WHEN length(key)<=128 THEN key ELSE NULL END,
        CASE WHEN length(CAST(request AS BLOB))<=?1 THEN request ELSE NULL END,
        CASE WHEN length(registration)<=128 THEN registration ELSE NULL END FROM model_acceptances",
        )
        .map_err(database)?;
    let rows = statement
        .query_map([REQUEST_BYTES as i64], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })
        .map_err(database)?;
    for row in rows {
        let (key, request, id) = row.map_err(database)?;
        IdempotencyKey::new(key).map_err(|_| corrupt())?;
        let id = ModelRegistrationId::new(id).map_err(|_| corrupt())?;
        let request: ModelRegistrationRequest =
            serde_json::from_str(&request).map_err(|_| corrupt())?;
        if !request.valid() || by_id.get(&id).copied() != Some(&request.identity) {
            return Err(corrupt());
        }
        referenced.insert(id);
    }
    if referenced.len() != items.len() {
        return Err(corrupt());
    }
    Ok(items)
}
#[derive(Clone)]
pub struct Registry {
    ledger: Ledger,
    browser: Option<Arc<ModelBrowser>>,
    accepts: Arc<Semaphore>,
}
impl Registry {
    pub fn new(ledger: Ledger, browser: Option<Arc<ModelBrowser>>) -> Self {
        Self {
            ledger,
            browser,
            accepts: Arc::new(Semaphore::new(1)),
        }
    }
    pub(crate) fn browser(&self) -> Option<Arc<ModelBrowser>> {
        self.browser.clone()
    }
    fn service(&self) -> ServiceId {
        ServiceId::new(self.ledger.service_id().as_str()).unwrap()
    }
    async fn call<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        self.ledger
            .call(move |db| Ok(f(db)))
            .await
            .map_err(|_| ApiError::new("backend_unavailable"))?
    }
    pub async fn check(&self) -> Result<()> {
        self.call(move |db| check(db).map(|_| ())).await
    }
    pub async fn review(&self, request: ModelInspectRequest) -> Result<ModelRegistrationReview> {
        self.browser
            .as_ref()
            .ok_or_else(|| ApiError::new("backend_unavailable"))?
            .review(&self.service(), request)
            .await
    }
    pub async fn get(&self, id: ModelRegistrationId) -> Result<RegisteredModel> {
        self.call(move |db| {
            budget(db)?;
            record(db, &id)
        })
        .await
    }
    pub async fn list(&self) -> Result<ModelRegistrations> {
        self.call(move |db| {
            let tx = db.transaction().map_err(database)?;
            Ok(ModelRegistrations {
                schema_version: 1,
                items: check(&tx)?,
            })
        })
        .await
    }
    async fn accepted(
        &self,
        key: IdempotencyKey,
        request: Option<ModelRegistrationRequest>,
    ) -> Result<Option<ModelRegistrationResponse>> {
        let service = self.service();
        self.call(move |db| {
            budget(db)?;
            replay(db, &service, &key, request.as_ref())
        })
        .await
    }
    pub async fn lookup(&self, key: IdempotencyKey) -> Result<ModelRegistrationResponse> {
        self.accepted(key, None)
            .await?
            .ok_or_else(|| ApiError::new("not_found"))
    }
    pub async fn register(
        &self,
        key: IdempotencyKey,
        request: ModelRegistrationRequest,
    ) -> Result<ModelRegistrationResponse> {
        if let Some(reply) = self.accepted(key.clone(), Some(request.clone())).await? {
            return Ok(reply);
        }
        // Serialize new acceptance through inspection and commit, without an
        // unbounded queue. Busy/retry remains unresolved under the same key.
        let _permit = self
            .accepts
            .try_acquire()
            .map_err(|_| ApiError::new("model_registration_busy"))?;
        if let Some(reply) = self.accepted(key.clone(), Some(request.clone())).await? {
            return Ok(reply);
        }
        if !request.valid() {
            return Err(ApiError::new("invalid_parameters"));
        }
        let request_json = encode(&request, REQUEST_BYTES)?;
        let definition = self
            .browser
            .as_ref()
            .ok_or_else(|| ApiError::new("backend_unavailable"))?
            .accept(&self.service(), &request)
            .await?;
        json::canonical(&definition, MODEL_DEFINITION_BYTES)
            .map_err(|_| ApiError::new("limit_exceeded"))?;
        let service = self.service();
        self.call(move |db| {
            let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .map_err(database)?;
            check(&tx)?;
            if let Some(reply) = replay(&tx, &service, &key, Some(&request))? {
                return Ok(reply);
            }
            let identity_hash = identity_hash(&request.identity)?;
            let old: Option<String> = tx.query_row(
                "SELECT id FROM model_registrations WHERE identity_hash=?1",
                [&identity_hash], |r| r.get(0),
            ).optional().map_err(database)?;
            let existing = old.map(|id| {
                record(&tx, &ModelRegistrationId::new(id).map_err(|_| corrupt())?)
            }).transpose()?;
            let (registration, new_record) = if let Some(old) = existing {
                if old.summary.identity != request.identity || old.definition != definition {
                    return Err(corrupt());
                }
                (old.summary, None)
            } else {
                let random = random_id().map_err(|_| ApiError::new("internal_error"))?;
                let id = ModelRegistrationId::new(format!("model-{}", random.as_str())).unwrap();
                let value = RegisteredModel {
                    schema_version: 1,
                    summary: ModelRegistrationSummary {
                        registration_id: id,
                        identity: request.identity.clone(),
                    },
                    definition,
                };
                validate(&value)?;
                let bytes = encode(&value, RECORD_BYTES)?;
                (value.summary, Some(bytes))
            };
            let (rows, keys, bytes) = budget(&tx)?;
            if (new_record.is_some() && rows >= MODEL_REGISTRATION_LIMIT as u64)
                || keys >= ACCEPTANCES
                || bytes + request_json.len() as u64
                    + new_record.as_ref().map_or(0, |v| v.len() as u64) > REGISTRY_BYTES
            {
                return Err(ApiError::new("model_registry_full"));
            }
            if let Some(bytes) = new_record {
                tx.execute(
                    "INSERT INTO model_registrations(id,identity_hash,digest,record) VALUES(?1,?2,?3,?4)",
                    params![registration.registration_id.as_str(), identity_hash, json::hash(bytes.as_bytes()), bytes],
                ).map_err(database)?;
            }
            tx.execute("INSERT INTO model_acceptances VALUES(?1,?2,?3)",
                params![key.as_str(), request_json, registration.registration_id.as_str()],
            ).map_err(database)?;
            tx.commit().map_err(database)?;
            Ok(ModelRegistrationResponse {
                service_id: service, key, request, registration, replayed: false,
            })
        }).await
    }
}
