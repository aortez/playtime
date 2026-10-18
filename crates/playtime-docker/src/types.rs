use crate::{Error, Failure, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, time::Duration};
use tokio::time::Instant;

pub const PYTHON: &str = "/usr/local/bin/python3.12";
pub const BRIDGE: &str = "/opt/playtime/tool.py";
pub const BRIDGE_SHA256: &str = "d4de00759f0e8dcfc61e4f2e54d880117e8dbac397d2394477025b6a49ee7eb8";
pub const STOP_GRACE: Duration = Duration::from_secs(180);
pub const CLEANUP_BOUND: Duration = Duration::from_secs(20);
pub const TIMER: &str = "import os,signal,sys,time\ndeadline=int(sys.argv[1])\nwhile True:\n remaining=deadline-time.monotonic_ns()\n if remaining<=0: os.kill(os.getpid(),signal.SIGKILL)\n time.sleep(min(remaining/1000000000,1))\n";

pub(crate) fn hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
pub(crate) fn hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}
pub(crate) fn token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 96
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonIdentity {
    pub id: String,
    pub name: String,
    pub cli_version: String,
    pub server_version: String,
    pub api_version: String,
    pub kernel: String,
}

/// Supplied by the trusted host. Deserializing this is not lifecycle permission.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ownership {
    pub service: String,
    pub run: String,
    pub grant: String,
    pub invocation: String,
    /// At least 128 random bits, generated before the ownership commit.
    pub nonce: String,
    pub daemon: DaemonIdentity,
}
impl Ownership {
    pub fn random_nonce() -> Result<String> {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes).map_err(|_| {
            Error::new(
                Failure::InvalidInput,
                "ownership",
                "random source unavailable",
            )
        })?;
        Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
    }
    pub(crate) fn validate(&self) -> Result<()> {
        if [&self.service, &self.run, &self.grant, &self.invocation]
            .iter()
            .all(|s| token(s))
            && hex(&self.nonce, 32)
        {
            Ok(())
        } else {
            Err(Error::new(
                Failure::InvalidInput,
                "ownership",
                "invalid trusted ownership descriptor",
            ))
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    pub nano_cpus: u64,
    pub memory_bytes: u64,
    pub workspace_bytes: u64,
    pub workspace_inodes: u32,
    pub tmp_bytes: u64,
    pub pids: u32,
    pub seed: u32,
}
impl Limits {
    pub fn smoke() -> Self {
        Self {
            nano_cpus: 1_000_000_000,
            memory_bytes: 1024 * 1024 * 1024,
            workspace_bytes: 8 * 1024 * 1024,
            workspace_inodes: 16384,
            tmp_bytes: 16 * 1024 * 1024,
            pids: 64,
            seed: 1042,
        }
    }
    pub(crate) fn validate(&self) -> Result<()> {
        if !(1_000_000..=64_000_000_000).contains(&self.nano_cpus)
            || !(64 * 1024 * 1024..=64 * 1024 * 1024 * 1024).contains(&self.memory_bytes)
            || !(4096..=1024 * 1024 * 1024).contains(&self.workspace_bytes)
            || !(16..=65536).contains(&self.workspace_inodes)
            || !(1024 * 1024..=1024 * 1024 * 1024).contains(&self.tmp_bytes)
            || !(16..=4096).contains(&self.pids)
        {
            Err(Error::new(
                Failure::InvalidInput,
                "limits",
                "unsupported resource bounds",
            ))
        } else {
            Ok(())
        }
    }
    pub(crate) fn volume_options(&self) -> BTreeMap<String, String> {
        BTreeMap::from([
            ("type".into(), "tmpfs".into()),
            ("device".into(), "tmpfs".into()),
            (
                "o".into(),
                format!(
                    "size={},uid=1000,gid=1000,mode=0700,nr_inodes={}",
                    self.workspace_bytes, self.workspace_inodes
                ),
            ),
        ])
    }
    pub(crate) fn tmpfs(&self) -> String {
        format!(
            "rw,nosuid,nodev,size={},mode=1777,nr_inodes={}",
            self.tmp_bytes, self.workspace_inodes
        )
    }
}

/// Linux CLOCK_MONOTONIC is shared by a local rootful container. Absolute timer
/// data is frozen once; starting or reconciling a resource cannot extend it.
#[derive(Clone, Copy, Debug)]
pub struct WorkDeadline {
    pub(crate) instant: Instant,
    pub(crate) monotonic_ns: u64,
}
impl WorkDeadline {
    /// Bind an already selected Linux CLOCK_MONOTONIC instant. Reattaching or
    /// converting the host's remaining budget must not manufacture a new cap.
    pub fn at(monotonic_ns: u64) -> Result<Self> {
        let clock = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
        let now = (clock.tv_sec as u64)
            .checked_mul(1_000_000_000)
            .and_then(|s| s.checked_add(clock.tv_nsec as u64))
            .ok_or_else(|| Error::new(Failure::InvalidInput, "deadline", "clock overflow"))?;
        let remaining = Duration::from_nanos(monotonic_ns.saturating_sub(now));
        if monotonic_ns == 0
            || remaining > Duration::from_secs(86400)
            || monotonic_ns
                .checked_add(STOP_GRACE.as_nanos() as u64)
                .is_none()
        {
            return Err(Error::new(
                Failure::InvalidInput,
                "deadline",
                "absolute deadline outside supported range",
            ));
        }
        Ok(Self {
            instant: Instant::now() + remaining,
            monotonic_ns,
        })
    }
    pub fn after(duration: Duration) -> Result<Self> {
        if duration.is_zero() || duration > Duration::from_secs(86400) {
            return Err(Error::new(
                Failure::InvalidInput,
                "deadline",
                "work duration outside supported range",
            ));
        }
        let clock = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
        let now = (clock.tv_sec as u64)
            .checked_mul(1_000_000_000)
            .and_then(|s| s.checked_add(clock.tv_nsec as u64))
            .ok_or_else(|| Error::new(Failure::InvalidInput, "deadline", "clock overflow"))?;
        let monotonic_ns = now
            .checked_add(duration.as_nanos() as u64)
            .ok_or_else(|| Error::new(Failure::InvalidInput, "deadline", "deadline overflow"))?;
        Self::at(monotonic_ns)
    }
    pub fn instant(self) -> Instant {
        self.instant
    }
    pub fn monotonic_ns(self) -> u64 {
        self.monotonic_ns
    }
    pub(crate) fn holder_ns(self) -> u64 {
        self.monotonic_ns + STOP_GRACE.as_nanos() as u64
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Volume,
    Holder,
    Agent,
}
impl Role {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Volume => "volume",
            Self::Holder => "holder",
            Self::Agent => "agent",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Proposal {
    pub schema: u32,
    pub ownership: Ownership,
    pub role: Role,
    pub session: Option<u32>,
    pub name: String,
    pub image: String,
    pub volume: String,
    pub limits: Limits,
    pub deadline_monotonic_ns: u64,
}
impl Proposal {
    pub(crate) fn new(
        ownership: Ownership,
        role: Role,
        session: Option<u32>,
        image: String,
        limits: Limits,
        deadline: WorkDeadline,
    ) -> Self {
        let digest = hash(&serde_json::to_vec(&ownership).expect("ownership serialization"));
        let prefix = format!("playtime-{}", &digest[..32]);
        Self {
            schema: 1,
            ownership,
            role,
            session,
            name: format!("{prefix}-{}-{}", role.name(), session.unwrap_or(0)),
            image,
            volume: format!("{prefix}-volume-0"),
            limits,
            deadline_monotonic_ns: if role == Role::Holder {
                deadline.holder_ns()
            } else {
                deadline.monotonic_ns
            },
        }
    }
    pub fn labels(&self) -> BTreeMap<String, String> {
        let o = &self.ownership;
        BTreeMap::from([
            ("io.playtime.schema".into(), "1".into()),
            ("io.playtime.service".into(), o.service.clone()),
            ("io.playtime.run".into(), o.run.clone()),
            ("io.playtime.grant".into(), o.grant.clone()),
            ("io.playtime.invocation".into(), o.invocation.clone()),
            ("io.playtime.nonce".into(), o.nonce.clone()),
            ("io.playtime.daemon".into(), o.daemon.id.clone()),
            ("io.playtime.role".into(), self.role.name().into()),
            (
                "io.playtime.session".into(),
                self.session.unwrap_or(0).to_string(),
            ),
            (
                "io.playtime.proposal".into(),
                hash(&serde_json::to_vec(self).expect("proposal serialization")),
            ),
        ])
    }
}

#[async_trait::async_trait]
pub trait OwnershipJournal: Send + Sync {
    /// Return only after durable publication of this exact proposal. No Docker
    /// mutation is sent if this fails. Production supplies its admission journal;
    /// tests must label their explicitly synthetic implementation.
    async fn commit(&self, proposal: &Proposal) -> Result<()>;
}
