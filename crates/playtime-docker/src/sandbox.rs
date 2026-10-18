use crate::{
    CancellationToken, Error, Failure, Result,
    bridge::{Tool, ToolLimits, ToolReply},
    engine::{ReadyImage, outcome_error, verify_container, verify_volume},
    transport::{Operation, Outcome},
    types::*,
};
use serde::Serialize;
use std::{sync::Arc, time::Duration};
use tokio::time::Instant;

#[derive(Clone, Debug)]
struct Resource {
    proposal: Proposal,
    id: Option<String>,
    dispatched: bool,
    absent: bool,
}
impl Resource {
    fn new(proposal: Proposal) -> Self {
        Self {
            proposal,
            id: None,
            dispatched: false,
            absent: false,
        }
    }
}

/// An owned lifecycle handle. Methods require exclusive access; dropping an
/// in-flight future leaves the resource proposed/fenced, never ready to replay.
/// Drop does not perform best-effort destructive actions without evidence.
pub struct Sandbox {
    ready: ReadyImage,
    owner: Ownership,
    limits: Limits,
    work: WorkDeadline,
    journal: Arc<dyn OwnershipJournal>,
    volume: Option<Resource>,
    holder: Option<Resource>,
    agent: Option<Resource>,
    ordinal: u32,
    initialized: bool,
    fenced: bool,
    closed: bool,
    capture_deadline: Option<Instant>,
    quiescence_deadline: Option<Instant>,
    cleanup_deadline: Option<Instant>,
}

/// A non-serializable exclusive borrow. The holder cannot be removed and a new
/// agent cannot start while the future checkpoint owner retains this handle.
/// D2 supplies capture; this token alone does not publish a checkpoint.
pub struct Quiescent<'a> {
    sandbox: &'a mut Sandbox,
    deadline: Instant,
}
impl Quiescent<'_> {
    pub fn holder_id(&self) -> &str {
        self.sandbox
            .holder
            .as_ref()
            .and_then(|r| r.id.as_deref())
            .expect("verified holder")
    }
    pub fn volume_name(&self) -> &str {
        &self
            .sandbox
            .volume
            .as_ref()
            .expect("verified volume")
            .proposal
            .name
    }
    pub fn deadline(&self) -> Instant {
        self.deadline
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ResourceOutcome {
    NotDispatched {
        role: Role,
        name: String,
    },
    ConfirmedAbsent {
        role: Role,
        name: String,
    },
    Residual {
        role: Role,
        name: String,
        error: Error,
    },
}
#[derive(Clone, Debug, Serialize)]
pub struct Cleanup {
    pub resources: Vec<ResourceOutcome>,
}
impl Cleanup {
    pub fn complete(&self) -> bool {
        self.resources
            .iter()
            .all(|r| !matches!(r, ResourceOutcome::Residual { .. }))
    }
}

impl Sandbox {
    pub fn new(
        ready: ReadyImage,
        owner: Ownership,
        limits: Limits,
        work: WorkDeadline,
        journal: Arc<dyn OwnershipJournal>,
    ) -> Result<Self> {
        owner.validate()?;
        limits.validate()?;
        if owner.daemon != *ready.daemon() {
            return Err(Error::new(
                Failure::ChangedIdentity,
                "ownership",
                "descriptor is bound to another daemon",
            ));
        }
        Ok(Self {
            ready,
            owner,
            limits,
            work,
            journal,
            volume: None,
            holder: None,
            agent: None,
            ordinal: 0,
            initialized: false,
            fenced: false,
            closed: false,
            capture_deadline: None,
            quiescence_deadline: None,
            cleanup_deadline: None,
        })
    }
    pub fn is_fenced(&self) -> bool {
        self.fenced
    }
    pub fn proposals(&self) -> Vec<Proposal> {
        [&self.volume, &self.holder, &self.agent]
            .into_iter()
            .flatten()
            .map(|r| r.proposal.clone())
            .collect()
    }
    pub fn observations(&self) -> Vec<crate::engine::Observation> {
        self.ready.engine.observations()
    }
    fn proposal(&self, role: Role, session: Option<u32>) -> Proposal {
        Proposal::new(
            self.owner.clone(),
            role,
            session,
            self.ready.image.clone(),
            self.limits.clone(),
            self.work,
        )
    }
    fn work_bound(&self, deadline: Instant) -> Instant {
        deadline
            .min(self.work.instant)
            .min(Instant::now() + Duration::from_secs(20))
    }
    async fn journal(
        &self,
        p: &Proposal,
        deadline: Instant,
        cancel: &CancellationToken,
    ) -> Result<()> {
        tokio::select! {
            biased;
            _=cancel.cancelled()=>Err(Error::new(Failure::Cancelled,"ownership","cancelled before ownership commit")),
            _=tokio::time::sleep_until(deadline)=>Err(Error::new(Failure::Deadline,"ownership","ownership deadline expired")),
            result=self.journal.commit(p)=>result,
        }
    }
    pub async fn initialize(&mut self, deadline: Instant, cancel: CancellationToken) -> Result<()> {
        if self.closed || self.volume.is_some() {
            return Err(Error::new(
                Failure::Fenced,
                "initialize",
                "initialization is not replayable",
            ));
        }
        let deadline = self.work_bound(deadline);
        let p = self.proposal(Role::Volume, None);
        self.journal(&p, deadline, &cancel).await?;
        self.volume = Some(Resource::new(p));
        self.fenced = true;
        let engine = self.ready.engine.clone();
        let resource = self.volume.as_mut().expect("volume");
        if engine
            .volume(&resource.proposal.name, deadline, cancel.clone())
            .await?
            .is_some()
        {
            return Err(Error::new(
                Failure::Conflict,
                "volume_create",
                "proposed name already exists",
            ));
        }
        // Mark before await: timeout/drop/lost reply cannot erase this intent.
        resource.dispatched = true;
        let outcome = engine
            .call(
                Operation::CreateVolume(resource.proposal.clone()),
                deadline,
                cancel.clone(),
            )
            .await?;
        acknowledgement(&outcome, &resource.proposal.name, "volume_create")?;
        let actual = engine
            .volume(&resource.proposal.name, deadline, cancel.clone())
            .await?
            .ok_or_else(|| {
                Error::new(
                    Failure::Transport,
                    "volume_create",
                    "acknowledged volume is absent",
                )
            })?;
        verify_volume(&actual, &resource.proposal)?;

        let p = self.proposal(Role::Holder, None);
        self.journal(&p, deadline, &cancel).await?;
        self.holder = Some(Resource::new(p));
        create_container(
            &engine,
            self.holder.as_mut().expect("holder"),
            deadline,
            cancel.clone(),
        )
        .await?;
        let id = self
            .holder
            .as_ref()
            .and_then(|r| r.id.as_deref())
            .expect("holder id");
        engine.probe(id, deadline, cancel).await?;
        self.initialized = true;
        self.fenced = false;
        Ok(())
    }
    pub async fn start_session(
        &mut self,
        ordinal: u32,
        deadline: Instant,
        cancel: CancellationToken,
    ) -> Result<()> {
        if !self.initialized
            || self.closed
            || self.fenced
            || self.agent.is_some()
            || ordinal
                != self.ordinal.checked_add(1).ok_or_else(|| {
                    Error::new(Failure::InvalidInput, "start_session", "session overflow")
                })?
        {
            return Err(Error::new(
                Failure::Fenced,
                "start_session",
                "requires initialized workspace and confirmed previous agent quiescence",
            ));
        }
        let deadline = self.work_bound(deadline);
        self.verify_holder(deadline, cancel.clone()).await?;
        self.verify_mounts(deadline, cancel.clone()).await?;
        let p = self.proposal(Role::Agent, Some(ordinal));
        self.journal(&p, deadline, &cancel).await?;
        self.ordinal = ordinal;
        self.capture_deadline = None;
        self.quiescence_deadline = None;
        self.fenced = true;
        self.agent = Some(Resource::new(p));
        create_container(
            &self.ready.engine,
            self.agent.as_mut().expect("agent"),
            deadline,
            cancel.clone(),
        )
        .await?;
        let id = self
            .agent
            .as_ref()
            .and_then(|r| r.id.as_deref())
            .expect("agent id");
        self.ready.engine.probe(id, deadline, cancel).await?;
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
        if self.closed || self.fenced || !self.initialized {
            return Err(Error::new(Failure::Fenced, "tool", "agent is fenced"));
        }
        let agent = self
            .agent
            .as_ref()
            .ok_or_else(|| Error::new(Failure::Fenced, "tool", "no running agent"))?;
        let deadline = deadline.min(self.work.instant);
        // Validate the complete encoded request before any Docker process. The
        // bridge cap is a valid result; this outer transport deadline is not.
        if !limits.command_seconds.is_finite() || limits.command_seconds <= 0.0 {
            return Err(Error::new(
                Failure::InvalidInput,
                "tool",
                "invalid command limit",
            ));
        }
        limits.command_seconds = limits.command_seconds.min(
            deadline
                .saturating_duration_since(Instant::now())
                .as_secs_f64(),
        );
        let input = tool.encode(limits)?;
        let deadline =
            deadline.min(Instant::now() + Duration::from_secs_f64(limits.command_seconds + 5.0));
        self.fenced = true;
        let actual = self
            .ready
            .engine
            .container(
                agent.id.as_deref().ok_or_else(|| {
                    Error::new(Failure::Fenced, "tool", "no acknowledged agent identity")
                })?,
                deadline,
                cancel.clone(),
            )
            .await?
            .ok_or_else(|| Error::new(Failure::Fenced, "tool", "agent no longer exists"))?;
        let id = verify_container(&actual, &agent.proposal)?;
        if agent.id.as_deref() != Some(id.as_str()) || actual["State"]["Running"] != true {
            return Err(Error::new(
                Failure::Fenced,
                "tool",
                "agent stopped or changed identity",
            ));
        }
        let result = self
            .ready
            .engine
            .call(Operation::Bridge { id, payload: input }, deadline, cancel)
            .await?;
        if !result.success() {
            return Err(outcome_error(&result, "tool"));
        }
        let result = tool.decode(&result.stdout)?;
        self.fenced = false;
        Ok(result)
    }
    /// Shares the caller's total stop/capture deadline, spending at most twenty
    /// seconds on quiescence. Never stops the trusted holder here.
    pub async fn quiesce(
        &mut self,
        deadline: Instant,
        cancel: CancellationToken,
    ) -> Result<Quiescent<'_>> {
        if !self.initialized || self.closed {
            return Err(Error::new(
                Failure::Fenced,
                "quiesce",
                "workspace is not initialized",
            ));
        }
        let deadline = deadline.min(
            *self
                .capture_deadline
                .get_or_insert(deadline.min(Instant::now() + Duration::from_secs(80))),
        );
        let stop_deadline = deadline.min(
            *self
                .quiescence_deadline
                .get_or_insert(deadline.min(Instant::now() + CLEANUP_BOUND)),
        );
        self.fenced = true;
        if let Some(agent) = self.agent.as_mut() {
            remove_container(&self.ready.engine, agent, stop_deadline, cancel.clone()).await?;
            self.agent = None;
        }
        self.verify_holder(stop_deadline, cancel.clone()).await?;
        self.verify_mounts(stop_deadline, cancel).await?;
        self.fenced = false;
        Ok(Quiescent {
            sandbox: self,
            deadline,
        })
    }
    async fn verify_holder(&self, deadline: Instant, cancel: CancellationToken) -> Result<()> {
        let volume = self
            .volume
            .as_ref()
            .ok_or_else(|| Error::new(Failure::Fenced, "holder", "missing volume"))?;
        let actual = self
            .ready
            .engine
            .volume(&volume.proposal.name, deadline, cancel.clone())
            .await?
            .ok_or_else(|| Error::new(Failure::Fenced, "holder", "workspace volume disappeared"))?;
        verify_volume(&actual, &volume.proposal)?;
        let holder = self
            .holder
            .as_ref()
            .ok_or_else(|| Error::new(Failure::Fenced, "holder", "missing holder"))?;
        let id = holder
            .id
            .as_deref()
            .ok_or_else(|| Error::new(Failure::Fenced, "holder", "holder identity unconfirmed"))?;
        let actual = self
            .ready
            .engine
            .container(id, deadline, cancel)
            .await?
            .ok_or_else(|| {
                Error::new(
                    Failure::Fenced,
                    "holder",
                    "holder disappeared; tmpfs may be lost",
                )
            })?;
        if verify_container(&actual, &holder.proposal)? != id || actual["State"]["Running"] != true
        {
            return Err(Error::new(
                Failure::Fenced,
                "holder",
                "holder stopped; tmpfs continuity is unproven",
            ));
        }
        Ok(())
    }
    async fn verify_mounts(&self, deadline: Instant, cancel: CancellationToken) -> Result<()> {
        let volume = &self.volume.as_ref().expect("volume").proposal.name;
        let holder = self
            .holder
            .as_ref()
            .and_then(|r| r.id.as_deref())
            .expect("holder id");
        let outcome = self
            .ready
            .engine
            .call(Operation::Mounts(volume.clone()), deadline, cancel)
            .await?;
        if !outcome.success() {
            return Err(outcome_error(&outcome, "volume_mounts"));
        }
        let text = std::str::from_utf8(&outcome.stdout).map_err(|_| {
            Error::new(
                Failure::Malformed,
                "volume_mounts",
                "invalid inventory UTF-8",
            )
        })?;
        let ids: Vec<_> = text.lines().collect();
        if ids != vec![holder] {
            return Err(Error::new(
                Failure::Conflict,
                "volume_mounts",
                "unexpected workspace mount; cannot establish quiescence",
            ));
        }
        Ok(())
    }
    /// Explicit final cleanup after the checkpoint borrow ends. All resources
    /// share one twenty-second budget. Errors never erase other residuals.
    pub async fn cleanup(&mut self, deadline: Instant, cancel: CancellationToken) -> Cleanup {
        self.closed = true;
        self.fenced = true;
        let deadline = deadline.min(
            *self
                .cleanup_deadline
                .get_or_insert(deadline.min(Instant::now() + CLEANUP_BOUND)),
        );
        let mut resources = Vec::new();
        for resource in [&mut self.agent, &mut self.holder].into_iter().flatten() {
            let result =
                remove_container(&self.ready.engine, resource, deadline, cancel.clone()).await;
            resources.push(report(resource, result));
        }
        if let Some(volume) = self.volume.as_mut() {
            let result = remove_volume(&self.ready.engine, volume, deadline, cancel).await;
            resources.push(report(volume, result));
        }
        Cleanup { resources }
    }
}
fn report(resource: &Resource, result: Result<()>) -> ResourceOutcome {
    let role = resource.proposal.role;
    let name = resource.proposal.name.clone();
    if !resource.dispatched {
        ResourceOutcome::NotDispatched { role, name }
    } else {
        match result {
            Ok(()) => ResourceOutcome::ConfirmedAbsent { role, name },
            Err(error) => ResourceOutcome::Residual { role, name, error },
        }
    }
}
fn acknowledgement(outcome: &Outcome, expected: &str, operation: &'static str) -> Result<()> {
    if !outcome.success() {
        return Err(outcome_error(outcome, operation));
    }
    if std::str::from_utf8(&outcome.stdout).ok().map(str::trim) != Some(expected) {
        return Err(Error::new(
            Failure::Malformed,
            operation,
            "invalid acknowledgement; reconcile proposed resource",
        ));
    }
    Ok(())
}
async fn create_container(
    engine: &crate::Engine,
    resource: &mut Resource,
    deadline: Instant,
    cancel: CancellationToken,
) -> Result<()> {
    let p = &resource.proposal;
    if engine
        .container(&p.name, deadline, cancel.clone())
        .await?
        .is_some()
    {
        return Err(Error::new(
            Failure::Conflict,
            "container_create",
            "proposed name already exists",
        ));
    }
    resource.dispatched = true;
    let result = engine
        .call(
            Operation::CreateContainer(p.clone()),
            deadline,
            cancel.clone(),
        )
        .await?;
    if !result.success() {
        return Err(outcome_error(&result, "container_create"));
    }
    let id = std::str::from_utf8(&result.stdout)
        .ok()
        .map(str::trim)
        .filter(|s| hex(s, 64))
        .ok_or_else(|| {
            Error::new(
                Failure::Malformed,
                "container_create",
                "invalid create reply; reconcile by proposed name",
            )
        })?
        .to_owned();
    resource.id = Some(id.clone());
    let actual = engine
        .container(&id, deadline, cancel.clone())
        .await?
        .ok_or_else(|| {
            Error::new(
                Failure::Transport,
                "container_create",
                "created container absent",
            )
        })?;
    if verify_container(&actual, p)? != id
        || actual["State"]["Status"] != "created"
        || actual["State"]["Running"] != false
    {
        return Err(Error::new(
            Failure::Conflict,
            "container_create",
            "unexpected created state",
        ));
    }
    let result = engine
        .call(Operation::Start(id.clone()), deadline, cancel.clone())
        .await?;
    acknowledgement(&result, &id, "container_start")?;
    let actual = engine
        .container(&id, deadline, cancel)
        .await?
        .ok_or_else(|| {
            Error::new(
                Failure::Transport,
                "container_start",
                "started container absent",
            )
        })?;
    if verify_container(&actual, p)? != id || actual["State"]["Running"] != true {
        return Err(Error::new(
            Failure::Fenced,
            "container_start",
            "container did not stay running; original timer may have expired",
        ));
    }
    Ok(())
}
async fn remove_container(
    engine: &crate::Engine,
    resource: &mut Resource,
    deadline: Instant,
    cancel: CancellationToken,
) -> Result<()> {
    if !resource.dispatched {
        return Ok(());
    }
    // Reinspect even a previous absence: cleanup must not invent current proof.
    let target = resource.id.as_deref().unwrap_or(&resource.proposal.name);
    let Some(actual) = engine.container(target, deadline, cancel.clone()).await? else {
        resource.absent = true;
        return Ok(());
    };
    let id = verify_container(&actual, &resource.proposal)?;
    if resource.id.as_ref().is_some_and(|old| old != &id) {
        return Err(Error::new(
            Failure::Conflict,
            "container_remove",
            "container ID changed",
        ));
    }
    resource.id = Some(id.clone());
    // Removal is safe to reconcile after a lost reply; tool execution is not.
    let _outcome = engine
        .call(
            Operation::RemoveContainer(id.clone()),
            deadline,
            cancel.clone(),
        )
        .await?;
    if engine.container(&id, deadline, cancel).await?.is_some() {
        return Err(Error::new(
            Failure::Transport,
            "container_remove",
            "owned container remains; cessation unconfirmed",
        ));
    }
    resource.absent = true;
    Ok(())
}
async fn remove_volume(
    engine: &crate::Engine,
    resource: &mut Resource,
    deadline: Instant,
    cancel: CancellationToken,
) -> Result<()> {
    if !resource.dispatched {
        return Ok(());
    }
    let name = &resource.proposal.name;
    let Some(actual) = engine.volume(name, deadline, cancel.clone()).await? else {
        resource.absent = true;
        return Ok(());
    };
    verify_volume(&actual, &resource.proposal)?;
    let _outcome = engine
        .call(
            Operation::RemoveVolume(name.clone()),
            deadline,
            cancel.clone(),
        )
        .await?;
    if engine.volume(name, deadline, cancel).await?.is_some() {
        return Err(Error::new(
            Failure::Transport,
            "volume_remove",
            "owned volume remains",
        ));
    }
    resource.absent = true;
    Ok(())
}
