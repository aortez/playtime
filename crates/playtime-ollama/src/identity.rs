use crate::{Failure, FailureKind, Result, json};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Profile;
impl Profile {
    pub const VERSION: &str = "0.32.3";
    pub const MODEL: &str = "gpt-oss:20b";
    pub const DIGEST: &str = "17052f91a42e97930aa6e28a6c6c06a983e6a58dbb00434885a0cf5313e376f7";
    pub const DEFINITION: &str = "5968b9af1d86b0583a95912df186c2c8ec6dc1a70a7e2bc612a294908402d912";
    /// Legacy Playtime classifier, not an official or universal backend code.
    pub fn tool_parse_error(error: &str) -> bool {
        error.starts_with("error parsing tool call:")
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelLock {
    pub name: String,
    pub digest: String,
    pub ollama_version: String,
    pub definition_sha256: String,
    pub definition: Value,
    pub modelfile: String,
}
impl ModelLock {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let value = json::parse(bytes, 16 * 1024 * 1024, true)?;
        let lock: Self = serde_json::from_value(value)
            .map_err(|e| Failure::new(FailureKind::Identity, e.to_string()))?;
        lock.validate()?;
        Ok(lock)
    }
    pub fn validate(&self) -> Result<()> {
        let bytes = json::canonical(&self.definition, 16 * 1024 * 1024)?;
        if self.name != Profile::MODEL
            || self.ollama_version != Profile::VERSION
            || self.digest != Profile::DIGEST
            || self.definition_sha256 != Profile::DEFINITION
            || json::hash(&bytes) != self.definition_sha256
            || bytes.len() != 8845
        {
            return Err(Failure::new(
                FailureKind::Identity,
                "unsupported or inconsistent model lock; never update it implicitly",
            ));
        }
        Ok(())
    }
}
#[derive(Clone, Debug)]
pub struct Inspection {
    pub version: Value,
    pub tags: Value,
    pub show: Value,
    pub loaded: Option<Value>,
    pub definition: Value,
    pub canonical_definition: Vec<u8>,
    pub definition_sha256: String,
}
pub(crate) fn local(value: &Value) -> Result<()> {
    for field in ["remote_host", "remote_model"] {
        if let Some(v) = value.get(field)
            && v.as_str() != Some("")
        {
            return Err(Failure::new(
                FailureKind::Identity,
                "remote/cloud model delegation is unsupported",
            ));
        }
    }
    Ok(())
}
pub(crate) fn verify(
    lock: &ModelLock,
    version: Value,
    tags: Value,
    show: Value,
    loaded: Option<Value>,
    limit: usize,
) -> Result<Inspection> {
    let bad = || {
        Failure::new(
            FailureKind::Identity,
            "missing, changed or unsupported Ollama identity",
        )
    };
    if version.get("version").and_then(Value::as_str) != Some(lock.ollama_version.as_str()) {
        return Err(bad());
    }
    let models = tags
        .get("models")
        .and_then(Value::as_array)
        .ok_or_else(bad)?;
    let matching: Vec<_> = models
        .iter()
        .filter(|m| m.get("name").and_then(Value::as_str) == Some(lock.name.as_str()))
        .collect();
    if matching.len() != 1
        || matching[0].get("digest").and_then(Value::as_str) != Some(lock.digest.as_str())
    {
        return Err(bad());
    }
    local(matching[0])?;
    local(&show)?;
    if !show.is_object()
        || !show
            .get("capabilities")
            .and_then(Value::as_array)
            .is_some_and(|a| a.iter().all(Value::is_string) && a.iter().any(|c| c == "tools"))
    {
        return Err(bad());
    }
    let definition = Value::Object(
        [
            "template",
            "parameters",
            "model_info",
            "details",
            "capabilities",
        ]
        .into_iter()
        .map(|k| (k.into(), show.get(k).cloned().unwrap_or(Value::Null)))
        .collect(),
    );
    let canonical_definition = json::canonical(&definition, limit)?;
    let definition_sha256 = json::hash(&canonical_definition);
    if definition_sha256 != lock.definition_sha256 {
        return Err(bad());
    }
    if let Some(ps) = &loaded
        && !ps.get("models").is_some_and(Value::is_array)
    {
        return Err(bad());
    }
    Ok(Inspection {
        version,
        tags,
        show,
        loaded,
        definition,
        canonical_definition,
        definition_sha256,
    })
}
