//! A reviewed Strata lock on a host the daemon holds for one run. The daemon
//! acquires the host through the chat launcher's hold and freezes what the hold
//! verified; the worker only connects to that loopback forward.
use crate::{Error, Result};
use playtime_strata::{ModelLock, Settings, Thinking};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// The reviewed Strata locks this build runs, by name. A lock's identity and
/// host fields identify the model, so a refreshed lock gets a new name.
const REVIEWED: &[(&str, &[u8])] = &[(
    "framework-strata-gyro-m-fast-v2",
    include_bytes!("framework-strata-gyro-m-fast-v2-lock.json"),
)];
/// The synthetic lock that the Strata fixture server's metadata produces. It
/// is outside the profile digest, so a fixture build advertises the same profile.
#[cfg(feature = "strata-fixture")]
const FIXTURE: &[(&str, &[u8])] = &[(
    "fixture-strata-gyro-m",
    include_bytes!("fixture-strata-lock.json"),
)];
#[cfg(not(feature = "strata-fixture"))]
const FIXTURE: &[(&str, &[u8])] = &[];

/// The names of the Strata locks this build runs.
pub fn strata_lock_names() -> impl Iterator<Item = &'static str> {
    REVIEWED.iter().chain(FIXTURE).map(|(name, _)| *name)
}

fn compiled(name: &str) -> Result<&'static [u8]> {
    REVIEWED
        .iter()
        .chain(FIXTURE)
        .find(|(reviewed, _)| *reviewed == name)
        .map(|(_, bytes)| *bytes)
        .ok_or_else(|| Error::Invalid(format!("{name} is not a reviewed Strata lock")))
}

/// The compiled lock with this name, if this build runs it.
pub fn strata_lock(name: &str) -> Result<ModelLock> {
    let lock: ModelLock = serde_json::from_slice(compiled(name)?)?;
    lock.validate()
        .map_err(|e| Error::Invalid(format!("compiled Strata lock {name}: {e}")))?;
    Ok(lock)
}

/// The SHA-256 of the compiled lock file, which a launcher package's copy of
/// the lock must match.
pub fn strata_lock_sha256(name: &str) -> Result<String> {
    Ok(playtime_storage::sha256(compiled(name)?).to_string())
}

/// Every reviewed lock, for the profile digest.
pub(super) fn reviewed_locks() -> Result<Vec<serde_json::Value>> {
    REVIEWED
        .iter()
        .map(|(name, _)| Ok(serde_json::json!({"name": name, "lock": strata_lock(name)?})))
        .collect()
}
/// The first reviewed lock, which stands for all of them in the digest's template.
pub(super) fn first_reviewed() -> &'static str {
    REVIEWED[0].0
}

/// Archived Ollama models have no provider field, so this one tells them apart.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StrataProvider {
    Strata,
}

/// What the daemon's hold verified, frozen when the run is admitted.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StrataHost {
    /// The named target the run was launched on, if any. Boxed to keep
    /// `LiveModel`'s variants close in size; it serializes the same.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<Box<playtime_protocol::api::InferenceTarget>>,
    /// The chat launcher profile whose hold lent the endpoint.
    pub profile: String,
    /// The hold's loopback forward to the server.
    pub endpoint: String,
    /// The server process's start identity when the hold was verified. The
    /// worker refuses any other process.
    pub started: u64,
    /// The hold's verifier report. Its host identity is the lock's.
    pub verification: serde_json::Value,
    /// Machine-wide flock that every checkout's live worker holds for its lifetime.
    pub live_lock: PathBuf,
}
impl StrataHost {
    fn valid(&self, lock: &ModelLock) -> bool {
        use playtime_protocol::api::{InferenceProvider, valid_profile};
        self.target.as_ref().is_none_or(|t| {
            t.valid()
                && t.provider == InferenceProvider::Strata
                && t.profile.as_ref() == Some(&self.profile)
        }) && valid_profile(&self.profile)
            && playtime_strata::Endpoint::parse(&self.endpoint).is_ok()
            && self.started > 0
            && serde_json::to_value(&lock.host).is_ok_and(|host| self.verification["host"] == host)
            && self.live_lock.is_absolute()
    }
}

/// The exact reviewed lock and request settings, plus the held host they go to.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StrataModel {
    pub provider: StrataProvider,
    pub lock_name: String,
    pub lock: ModelLock,
    /// Sent with every request in place of the lock's own settings.
    pub settings: Settings,
    pub host: StrataHost,
}
impl StrataModel {
    /// The named lock with its own reviewed settings, for requests within
    /// `context_tokens`.
    pub fn freeze(lock_name: &str, host: StrataHost, context_tokens: u64) -> Result<Self> {
        let lock = strata_lock(lock_name)?;
        if context_tokens > lock.identity.context_tokens {
            return Err(Error::Invalid(format!(
                "{lock_name} serves {} context tokens, fewer than the run's {context_tokens}",
                lock.identity.context_tokens
            )));
        }
        Ok(Self {
            provider: StrataProvider::Strata,
            lock_name: lock_name.into(),
            settings: lock.settings.clone(),
            lock,
            host,
        })
    }
    /// The named lock with a recipe's sampling: its reasoning level,
    /// temperature, top-p, top-k and repeat penalty. Model default reasoning,
    /// and every setting a recipe doesn't choose, come from the lock.
    pub fn freeze_recipe(
        lock_name: &str,
        host: StrataHost,
        context_tokens: u64,
        sampling: &super::Sampling,
    ) -> Result<Self> {
        let mut model = Self::freeze(lock_name, host, context_tokens)?;
        let settings = &mut model.settings;
        settings.thinking = match sampling.thinking.as_str() {
            "default" => settings.thinking,
            "low" => Thinking::Low,
            "medium" => Thinking::Medium,
            "high" => Thinking::High,
            other => {
                return Err(Error::Invalid(format!(
                    "{lock_name} has no reasoning level {other}"
                )));
            }
        };
        settings.temperature = sampling.temperature;
        settings.top_p = sampling.top_p;
        settings.top_k = sampling.top_k;
        settings.repetition_penalty = sampling.repeat_penalty;
        settings.validate().map_err(|_| {
            Error::Invalid(format!("{lock_name} doesn't take the recipe's sampling"))
        })?;
        Ok(model)
    }
    pub fn validate(&self) -> Result<()> {
        let invalid = || Error::Invalid("unsupported frozen Strata model or host".into());
        if strata_lock(&self.lock_name)? != self.lock || !self.host.valid(&self.lock) {
            return Err(invalid());
        }
        self.settings.validate().map_err(|_| invalid())
    }
    /// The lock a client verifies against. Identity and host stay the reviewed
    /// ones; only the request settings differ.
    pub fn client_lock(&self) -> ModelLock {
        ModelLock {
            settings: self.settings.clone(),
            ..self.lock.clone()
        }
    }
}
