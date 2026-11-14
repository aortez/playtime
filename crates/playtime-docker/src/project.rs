//! Explicit persistent project tools. Only the selected directory is writable
//! on the host. Unlike the experiment's tmpfs volume, it has no aggregate disk
//! quota and is never removed by cleanup. The fixed image supplies the toolchain.
mod directory;
pub use directory::{ProjectDirectory, ProjectRoot};

use crate::{
    engine::outcome_error,
    sandbox::{Resource, create_container, remove_container, report},
    transport::Operation,
    *,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use tokio::time::Instant;

pub(crate) const PROBE: &str = "import os,json\ns=os.stat('/work')\nprint(json.dumps({'device':s.st_dev,'inode':s.st_ino,'uid':os.getuid(),'gid':os.getgid(),'writable':os.access('/work',os.W_OK|os.X_OK)}))\n";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectLimits {
    pub nano_cpus: u64,
    pub memory_bytes: u64,
    pub tmp_bytes: u64,
    pub tmp_inodes: u32,
    pub pids: u32,
    pub seed: u32,
}
impl ProjectLimits {
    fn container_limits(&self) -> Limits {
        Limits {
            nano_cpus: self.nano_cpus,
            memory_bytes: self.memory_bytes,
            tmp_bytes: self.tmp_bytes,
            workspace_inodes: self.tmp_inodes,
            // The shared container verifier uses this descriptor. A project
            // never creates a volume, so its volume size is deliberately inert.
            workspace_bytes: 4096,
            pids: self.pids,
            seed: self.seed,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectProposal {
    pub container: Proposal,
    pub root: ProjectRoot,
}
impl ProjectProposal {
    pub fn digest(&self) -> String {
        crate::types::hash(&serde_json::to_vec(self).expect("project serialization"))
    }
    pub fn labels(&self) -> BTreeMap<String, String> {
        let mut labels = self.container.labels();
        labels.insert("io.playtime.project".into(), self.digest());
        labels
    }
    pub(crate) fn verify_mount(&self, value: &Value) -> Result<()> {
        if value["HostConfig"]["Mounts"]
            != json!([{
                "Type":"bind","Source":self.root.path(),"Target":"/work",
                "BindOptions":{"Propagation":"rprivate","NonRecursive":true}
            }])
            || value["Mounts"]
                != json!([{
                    "Type":"bind","Source":self.root.path(),"Destination":"/work",
                    "Mode":"","RW":true,"Propagation":"rprivate"
                }])
        {
            return Err(Error::new(
                Failure::Conflict,
                "project_mount",
                "project mount configuration differs",
            ));
        }
        Ok(())
    }
}

#[async_trait::async_trait]
pub trait ProjectJournal: Send + Sync {
    /// Durably publish the exact directory identity, container and limits before
    /// any create. Cancellation can leave a committed but undispatched proposal.
    async fn commit(&self, proposal: &ProjectProposal) -> Result<()>;
}

/// One turn's owned process lifetime; the directory lease can span many turns.
/// Dropped operations remain fenced until exact cleanup, like `Sandbox`.
pub struct ProjectWorkspace {
    directory: Arc<ProjectDirectory>,
    ready: ReadyImage,
    proposal: ProjectProposal,
    resource: Option<Resource>,
    journal: Arc<dyn ProjectJournal>,
    work: WorkDeadline,
    fenced: bool,
    closed: bool,
    cleanup_deadline: Option<Instant>,
    released: bool,
}
impl ProjectWorkspace {
    /// Reserves the directory durably. Like directory admission, call this
    /// constructor off the async executor; failure never dispatches to Docker.
    pub fn new(
        ready: ReadyImage,
        owner: Ownership,
        directory: Arc<ProjectDirectory>,
        limits: ProjectLimits,
        work: WorkDeadline,
        journal: Arc<dyn ProjectJournal>,
    ) -> Result<Self> {
        owner.validate()?;
        let limits = limits.container_limits();
        limits.validate()?;
        if owner.daemon != *ready.daemon() {
            return Err(Error::new(
                Failure::ChangedIdentity,
                "project",
                "ownership belongs to another daemon",
            ));
        }
        let proposal = ProjectProposal {
            container: Proposal::new(owner, Role::Agent, None, ready.image.clone(), limits, work),
            root: directory.root().clone(),
        };
        directory.reserve(&proposal)?;
        Ok(Self {
            directory,
            ready,
            proposal,
            resource: None,
            journal,
            work,
            fenced: true,
            closed: false,
            cleanup_deadline: None,
            released: false,
        })
    }
    pub fn proposal(&self) -> &ProjectProposal {
        &self.proposal
    }
    pub fn is_fenced(&self) -> bool {
        self.fenced
    }

    async fn verify_directory(&self, deadline: Instant, cancel: &CancellationToken) -> Result<()> {
        let root = self.directory.root().clone();
        // A stalled host metadata read must not occupy the async executor or
        // prevent cancellation of a running container. A late check is read-only.
        let task = tokio::task::spawn_blocking(move || root.verify());
        tokio::select! {biased;
            _=cancel.cancelled()=>Err(Error::new(Failure::Cancelled,"project_identity","directory check cancelled")),
            _=tokio::time::sleep_until(deadline)=>Err(Error::new(Failure::Deadline,"project_identity","directory check deadline expired")),
            result=task=>result.unwrap_or_else(|_|Err(Error::new(Failure::ChangedIdentity,"project_identity","directory check worker failed"))),
        }
    }

    pub async fn start_workspace(
        &mut self,
        deadline: Instant,
        cancel: CancellationToken,
    ) -> Result<()> {
        if self.closed || self.resource.is_some() {
            return Err(Error::new(
                Failure::Fenced,
                "project_start",
                "workspace creation is not replayable",
            ));
        }
        let deadline = deadline
            .min(self.work.instant())
            .min(Instant::now() + Duration::from_secs(20));
        self.verify_directory(deadline, &cancel).await?;
        tokio::select! {
            biased;
            _=cancel.cancelled()=>return Err(Error::new(Failure::Cancelled,"project_start","cancelled before ownership commit")),
            _=tokio::time::sleep_until(deadline)=>return Err(Error::new(Failure::Deadline,"project_start","ownership deadline expired")),
            result=self.journal.commit(&self.proposal)=>result?,
        }
        self.resource = Some(Resource::project(self.proposal.clone()));
        self.verify_directory(deadline, &cancel).await?;
        let resource = self.resource.as_mut().expect("committed project");
        create_container(&self.ready.engine, resource, deadline, cancel.clone()).await?;
        let id = resource
            .id
            .as_ref()
            .expect("acknowledged container")
            .clone();
        self.ready
            .engine
            .probe(&id, deadline, cancel.clone())
            .await?;
        let probe = self
            .ready
            .engine
            .call(Operation::ProjectProbe(id), deadline, cancel.clone())
            .await?;
        if !probe.success() {
            return Err(outcome_error(&probe, "project_identity"));
        }
        let value: Value = serde_json::from_slice(&probe.stdout).map_err(|_| {
            Error::new(
                Failure::Malformed,
                "project_identity",
                "invalid directory probe",
            )
        })?;
        self.directory.root().verify_probe(&value)?;
        self.verify_directory(deadline, &cancel).await?;
        self.fenced = false;
        Ok(())
    }

    pub async fn tool(
        &mut self,
        tool: Tool,
        mut limits: ToolLimits,
        deadline: Instant,
        cancel: CancellationToken,
    ) -> Result<ToolReply> {
        if self.closed || self.fenced {
            return Err(Error::new(
                Failure::Fenced,
                "project_tool",
                "workspace is fenced",
            ));
        }
        if !limits.command_seconds.is_finite() || limits.command_seconds <= 0.0 {
            return Err(Error::new(
                Failure::InvalidInput,
                "project_tool",
                "invalid command limit",
            ));
        }
        let deadline = deadline.min(self.work.instant());
        limits.command_seconds = limits.command_seconds.min(
            deadline
                .saturating_duration_since(Instant::now())
                .as_secs_f64(),
        );
        let payload = tool.encode(limits)?;
        let deadline =
            deadline.min(Instant::now() + Duration::from_secs_f64(limits.command_seconds + 5.0));
        self.fenced = true;
        self.verify_directory(deadline, &cancel).await?;
        let resource = self.resource.as_ref().expect("started project");
        let id = resource.id.as_ref().expect("started container");
        let actual = self
            .ready
            .engine
            .container(id, deadline, cancel.clone())
            .await?
            .ok_or_else(|| Error::new(Failure::Fenced, "project_tool", "container is absent"))?;
        if resource.verify(&actual)? != *id || actual["State"]["Running"] != true {
            return Err(Error::new(
                Failure::Fenced,
                "project_tool",
                "container identity or running state changed",
            ));
        }
        let result = self
            .ready
            .engine
            .call(
                Operation::Bridge {
                    id: id.clone(),
                    payload,
                },
                deadline,
                cancel,
            )
            .await?;
        if !result.success() {
            return Err(outcome_error(&result, "project_tool"));
        }
        let reply = tool.decode(&result.stdout)?;
        self.fenced = false;
        Ok(reply)
    }

    pub async fn cleanup(&mut self, deadline: Instant, cancel: CancellationToken) -> Cleanup {
        self.closed = true;
        self.fenced = true;
        let deadline = deadline.min(
            *self
                .cleanup_deadline
                .get_or_insert(deadline.min(Instant::now() + CLEANUP_BOUND)),
        );
        let mut resources = if let Some(resource) = self.resource.as_mut() {
            let result = remove_container(&self.ready.engine, resource, deadline, cancel).await;
            vec![report(resource, result)]
        } else {
            vec![]
        };
        if !self.released
            && resources
                .iter()
                .all(|r| !matches!(r, ResourceOutcome::Residual { .. }))
        {
            let directory = self.directory.clone();
            let proposal = self.proposal.clone();
            // Container cessation has already been proven. A dropped wait may
            // finish releasing this reservation, but can never dispatch tools.
            let release = tokio::task::spawn_blocking(move || directory.release(&proposal));
            let result = tokio::select! {
                biased;
                _=tokio::time::sleep_until(deadline)=>Err(Error::new(Failure::Deadline,"project_release","reservation release deadline expired")),
                result=release=>result.unwrap_or_else(|_| Err(Error::new(Failure::Journal,"project_release","reservation release worker failed"))),
            };
            match result {
                Ok(()) => self.released = true,
                Err(error) => resources.push(ResourceOutcome::Residual {
                    role: Role::Agent,
                    name: self.proposal.container.name.clone(),
                    error,
                }),
            }
        }
        Cleanup { resources }
    }
}
