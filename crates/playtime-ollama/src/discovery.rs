//! Bounded read-only discovery, deliberately independent of ModelLock/Adapter.
use crate::{
    CancellationToken, Failure, FailureKind, Limits, Operation, Request, Result, Transport,
    adapter::{bounded, check, encoding, read_body},
    identity, json,
};
use serde_json::{Value, json};
use std::{collections::BTreeSet, time::Duration};
use tokio::time::Instant;
pub const MAX_MODELS: usize = 256;

pub struct Discovery<T> {
    transport: T,
    limits: Limits,
}
#[derive(Clone, Debug)]
pub struct Entry {
    pub name: String,
    pub digest: Option<String>,
    pub rejected: bool,
}
pub struct Inventory {
    pub version: String,
    pub entries: Vec<Entry>,
    pub loaded: Option<BTreeSet<(String, String)>>,
}
pub struct Detail {
    pub inventory: Inventory,
    pub show: Option<Value>,
    pub definition_sha256: Option<String>,
    pub definition: Option<Value>,
    pub rejected: bool,
}

pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 256 && !name.chars().any(char::is_control)
}
pub fn valid_digest(digest: &str) -> bool {
    digest.len() == 64
        && digest
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}
fn protocol() -> Failure {
    Failure::new(FailureKind::Protocol, "invalid model metadata")
}
fn entries(value: &Value) -> Result<Vec<Entry>> {
    let array = value
        .get("models")
        .and_then(Value::as_array)
        .ok_or_else(protocol)?;
    if array.len() > MAX_MODELS {
        return Err(Failure::new(
            FailureKind::ByteLimit,
            "model inventory entry bound exceeded",
        ));
    }
    let mut names = BTreeSet::new();
    let mut result = Vec::new();
    for item in array {
        let name = item
            .get("name")
            .and_then(Value::as_str)
            .filter(|n| valid_name(n))
            .ok_or_else(protocol)?;
        if !names.insert(name) {
            return Err(protocol());
        }
        let digest = item
            .get("digest")
            .and_then(Value::as_str)
            .filter(|d| valid_digest(d))
            .map(str::to_owned);
        result.push(Entry {
            name: name.into(),
            digest,
            rejected: identity::local(item).is_err(),
        });
    }
    result.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(result)
}
impl<T: Transport> Discovery<T> {
    pub fn new(transport: T, limits: Limits) -> Result<Self> {
        limits.validate()?;
        Ok(Self { transport, limits })
    }
    async fn metadata(
        &self,
        operation: Operation,
        name: Option<&str>,
        deadline: Instant,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        // No general request API is exposed by this reader.
        if !matches!(
            operation,
            Operation::Version | Operation::Tags | Operation::Loaded | Operation::Show
        ) {
            return Err(protocol());
        }
        let deadline = deadline.min(Instant::now() + Duration::from_secs(15));
        let body =
            name.map(|name| serde_json::to_vec(&json!({"model":name})).expect("string JSON"));
        let mut response = bounded(
            self.transport.send(Request { operation, body }),
            deadline,
            cancel,
        )
        .await?;
        encoding(&response)?;
        let limit = if response.status == 200 {
            self.limits.metadata_bytes
        } else {
            self.limits.error_bytes.min(self.limits.metadata_bytes)
        };
        let mut bytes = Vec::new();
        read_body(&mut response, &mut bytes, limit, deadline, cancel).await?;
        if response.status != 200 {
            return Err(Failure::new(
                FailureKind::Http,
                format!("metadata returned HTTP {}", response.status),
            ));
        }
        let value = json::parse(&bytes, self.limits.metadata_bytes, true)?;
        check(deadline, cancel)?;
        Ok(value)
    }
    async fn base(&self, deadline: Instant, cancel: &CancellationToken) -> Result<Inventory> {
        let value = self
            .metadata(Operation::Version, None, deadline, cancel)
            .await?;
        let version = value
            .get("version")
            .and_then(Value::as_str)
            .filter(|v| valid_name(v))
            .ok_or_else(protocol)?
            .into();
        let entries = entries(
            &self
                .metadata(Operation::Tags, None, deadline, cancel)
                .await?,
        )?;
        Ok(Inventory {
            version,
            entries,
            loaded: None,
        })
    }
    pub async fn inventory(
        &self,
        deadline: Instant,
        cancel: &CancellationToken,
    ) -> Result<Inventory> {
        let deadline = deadline.min(Instant::now() + Duration::from_secs(45));
        let mut result = self.base(deadline, cancel).await?;
        // Advisory failure cannot turn a valid inventory into a false empty list.
        if let Ok(value) = self
            .metadata(Operation::Loaded, None, deadline, cancel)
            .await
            && let Ok(loaded) = entries(&value)
            && loaded.iter().all(|e| e.digest.is_some() && !e.rejected)
        {
            result.loaded = Some(
                loaded
                    .into_iter()
                    .map(|e| (e.name, e.digest.unwrap()))
                    .collect(),
            );
        }
        if cancel.is_cancelled() {
            check(deadline, cancel)?;
        }
        Ok(result)
    }
    pub async fn inspect(
        &self,
        name: &str,
        expected_digest: Option<&str>,
        deadline: Instant,
        cancel: &CancellationToken,
    ) -> Result<Detail> {
        if !valid_name(name) || expected_digest.is_some_and(|d| !valid_digest(d)) {
            return Err(Failure::new(FailureKind::Request, "invalid model selector"));
        }
        let deadline = deadline.min(Instant::now() + Duration::from_secs(45));
        let inventory = self.base(deadline, cancel).await?;
        let mut detail = Detail {
            inventory,
            show: None,
            definition_sha256: None,
            definition: None,
            rejected: false,
        };
        let Some(entry) = detail.inventory.entries.iter().find(|e| e.name == name) else {
            return Ok(detail);
        };
        detail.rejected = entry.rejected;
        if entry.rejected
            || entry.digest.is_none()
            || expected_digest.is_some_and(|d| entry.digest.as_deref() != Some(d))
        {
            return Ok(detail);
        }
        let show = self
            .metadata(Operation::Show, Some(name), deadline, cancel)
            .await?;
        detail.rejected = identity::local(&show).is_err();
        if !detail.rejected
            && show.get("template").is_some_and(Value::is_string)
            && show.get("model_info").is_some_and(Value::is_object)
            && show.get("details").is_some_and(Value::is_object)
        {
            let definition = Value::Object(
                [
                    "template",
                    "parameters",
                    "model_info",
                    "details",
                    "capabilities",
                ]
                .into_iter()
                .map(|key| (key.into(), show.get(key).cloned().unwrap_or(Value::Null)))
                .collect(),
            );
            detail.definition_sha256 = Some(json::hash(&json::canonical(
                &definition,
                self.limits.metadata_bytes,
            )?));
            detail.definition = Some(definition);
        }
        // Refuse an inventory/show race. This is still an observation, not a lease.
        let after = entries(
            &self
                .metadata(Operation::Tags, None, deadline, cancel)
                .await?,
        )?;
        if !after
            .iter()
            .any(|e| e.name == entry.name && e.digest == entry.digest && !e.rejected)
        {
            return Err(Failure::new(
                FailureKind::Identity,
                "model changed during inspection",
            ));
        }
        let version = self
            .metadata(Operation::Version, None, deadline, cancel)
            .await?;
        if version.get("version").and_then(Value::as_str) != Some(detail.inventory.version.as_str())
        {
            return Err(Failure::new(
                FailureKind::Identity,
                "backend changed during inspection",
            ));
        }
        detail.show = Some(show);
        check(deadline, cancel)?;
        Ok(detail)
    }
}
