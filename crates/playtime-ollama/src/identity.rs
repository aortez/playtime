use crate::{Failure, FailureKind, Result, json};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Frozen execution identity supplied by the owner after capability review.
/// Validation establishes consistency and the supported wire version, not approval
/// of a new model. The recipe/worker boundary owns that approval.
#[derive(Clone, Debug)]
pub struct ExecutionModel {
    pub name: String,
    pub digest: String,
    pub backend_version: String,
    pub definition_sha256: String,
    pub definition: Value,
    pub reasoning: Vec<Option<crate::Thinking>>,
}
impl ExecutionModel {
    pub fn validate(&self) -> Result<()> {
        let digest = |s: &str| {
            s.len() == 64
                && s.bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        };
        let definition = self.definition.as_object();
        if self.name.is_empty() || self.name.len() > 256 || self.name.chars().any(char::is_control)
            || self.backend_version != Profile::VERSION
            || !digest(&self.digest) || !digest(&self.definition_sha256)
            || !definition.is_some_and(|d| d.len() == 5 && ["template", "parameters", "model_info", "details", "capabilities"].iter().all(|k| d.contains_key(*k)))
            || !self.definition["capabilities"].as_array().is_some_and(|c| c.iter().all(Value::is_string) && ["completion", "tools"].iter().all(|required| c.iter().any(|v| v == required)))
            || self.reasoning.is_empty() || self.reasoning.len() > 4
            || self.reasoning.iter().enumerate().any(|(i, choice)| self.reasoning[..i].contains(choice)
                || matches!(choice, Some(crate::Thinking::Level(level)) if !crate::THINKING_LEVELS.contains(&level.as_str())))
            || json::hash(&json::canonical(&self.definition, 1 << 20)?) != self.definition_sha256
        {
            return Err(Failure::new(FailureKind::Identity, "invalid frozen model execution identity"));
        }
        Ok(())
    }
    pub fn locked(lock: &ModelLock) -> Result<Self> {
        lock.validate()?;
        Ok(Self {
            name: lock.name.clone(),
            digest: lock.digest.clone(),
            backend_version: lock.ollama_version.clone(),
            definition_sha256: lock.definition_sha256.clone(),
            definition: lock.definition.clone(),
            reasoning: std::iter::once(None)
                .chain(
                    crate::THINKING_LEVELS
                        .into_iter()
                        .map(|level| Some(crate::Thinking::Level(level.into()))),
                )
                .collect(),
        })
    }
}

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
    lock: &ExecutionModel,
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
    if version.get("version").and_then(Value::as_str) != Some(lock.backend_version.as_str()) {
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
