//! A reviewed Strata lock on a host the daemon holds for one run. The daemon
//! acquires the host through the chat launcher's hold and freezes what the hold
//! verified; the worker only connects to that loopback forward.
use crate::{Error, Result};
use playtime_strata::{ModelLock, Settings};
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

/// The compiled lock with this name, if this build runs it.
pub fn strata_lock(name: &str) -> Result<ModelLock> {
    let (_, bytes) = REVIEWED
        .iter()
        .chain(FIXTURE)
        .find(|(reviewed, _)| *reviewed == name)
        .ok_or_else(|| Error::Invalid(format!("{name} is not a reviewed Strata lock")))?;
    let lock: ModelLock = serde_json::from_slice(bytes)?;
    lock.validate()
        .map_err(|e| Error::Invalid(format!("compiled Strata lock {name}: {e}")))?;
    Ok(lock)
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
        (1..=64).contains(&self.profile.len())
            && self
                .profile
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
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
