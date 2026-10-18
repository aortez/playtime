//! One retained native effect owner. The controller thread never waits on Docker
//! or ownership-journal I/O, and a cancelled request cannot create another owner.
use playtime_docker::{
    CancellationToken, Engine, Ownership, OwnershipJournal, Proposal, ReadyImage, Sandbox, Tool,
    ToolLimits, WorkDeadline,
    capture::{ArchiveUsage, CaptureProfile, SnapshotCandidate},
    transport::Cli,
};
use playtime_engine::{Effect, EffectKind, FileInfo, Input, Memory, Outcome, ToolPolicy};
use playtime_protocol::worker::Binding;
use playtime_runner::{CheckpointBytes, Effects, Error, Result, docker::Profile};
use playtime_storage::CommittedFrontier;
use playtime_worker_control::{Bootstrap, boot_ms, write_once};
use serde::Serialize;
use std::{
    collections::{BTreeMap, VecDeque},
    path::PathBuf,
    sync::{Arc, Mutex, mpsc},
    time::Duration,
};
use tokio::time::Instant;

#[derive(Clone, Serialize)]
struct Intent {
    effect: u64,
    frontier: playtime_protocol::storage::EventFrontier,
}
struct Journal {
    directory: PathBuf,
    binding: Binding,
    intent: Mutex<Option<Intent>>,
    slot: Arc<tokio::sync::Semaphore>,
}
#[async_trait::async_trait]
impl OwnershipJournal for Journal {
    async fn commit(&self, proposal: &Proposal) -> playtime_docker::Result<()> {
        let error = || playtime_docker::Error {
            failure: playtime_docker::Failure::Journal,
            operation: "ownership",
            detail: "durable ownership publication failed; no create authorized",
        };
        let intent = self
            .intent
            .lock()
            .map_err(|_| error())?
            .clone()
            .ok_or_else(error)?;
        let bytes = serde_json::to_vec(&serde_json::json!({"version":1,"binding":self.binding,
            "intent":intent,"proposal":proposal}))
        .map_err(|_| error())?;
        if bytes.len() > 32768 {
            return Err(error());
        }
        let permit = self.slot.clone().try_acquire_owned().map_err(|_| error())?;
        let path = self.directory.join(format!(
            "docker-{}-{}.json",
            match proposal.role {
                playtime_docker::Role::Volume => "volume",
                playtime_docker::Role::Holder => "holder",
                playtime_docker::Role::Agent => "agent",
            },
            proposal.session.unwrap_or(0)
        ));
        // Cancellation detaches observation only. The single retained permit
        // prevents an overlapping journal write after an uncertain publication.
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            write_once(&path, &bytes)
        })
        .await
        .map_err(|_| error())?
        .map_err(|_| error())
    }
}

struct Task {
    effect: Effect,
    confirmed: CommittedFrontier,
    cancel: CancellationToken,
}
struct Reply {
    id: u64,
    outcome: Outcome,
    candidate: Option<SnapshotCandidate>,
}
struct Actor {
    sandbox: Sandbox,
    profile: Profile,
    memory: Memory,
    journal: Arc<Journal>,
    models: VecDeque<playtime_engine::simulation::ModelScript>,
    calls: u64,
    private: PathBuf,
}
impl Actor {
    async fn execute(&mut self, task: &Task) -> Result<(Outcome, Option<SnapshotCandidate>)> {
        let effect = &task.effect;
        let deadline =
            Instant::now() + Duration::from_millis(effect.deadline_ms.saturating_sub(boot_ms()));
        if task.cancel.is_cancelled() || boot_ms() >= effect.deadline_ms {
            return Err(Error::Uncertain(
                "effect retired before native dispatch".into(),
            ));
        }
        *self.journal.intent.lock().expect("journal context") = Some(Intent {
            effect: effect.id,
            frontier: task.confirmed.frontier().clone(),
        });
        let cancel = task.cancel.clone();
        let map = |e: playtime_docker::Error| Error::Uncertain(e.to_string());
        let outcome = match &effect.kind {
            EffectKind::StartSession { ordinal, .. } => {
                if *ordinal == 1 {
                    self.sandbox
                        .initialize(deadline, cancel.clone())
                        .await
                        .map_err(map)?;
                }
                self.sandbox
                    .start_session(*ordinal, deadline, cancel)
                    .await
                    .map_err(map)?;
                Outcome::Started(self.memory.clone())
            }
            EffectKind::Model(request) => {
                self.calls += 1;
                if self.calls == 4
                    && (request.messages.len() != 2
                        || request
                            .messages
                            .iter()
                            .any(|m| matches!(m["role"].as_str(), Some("assistant" | "tool")))
                        || !request.messages.iter().any(|m| {
                            m["content"]
                                .as_str()
                                .is_some_and(|text| text.contains(&self.memory.content))
                        }))
                {
                    return Err(Error::Invalid(
                        "second session did not receive a fresh conversation and actual memento"
                            .into(),
                    ));
                }
                let model = self
                    .models
                    .pop_front()
                    .ok_or_else(|| Error::Invalid("frozen model responses exhausted".into()))?;
                let response = model
                    .completion
                    .ok_or_else(|| Error::Invalid("missing scripted completion".into()))?;
                match response.outcome {
                    playtime_engine::simulation::ModelEnding::Response(r) => Outcome::Model(r),
                    playtime_engine::simulation::ModelEnding::Error(e) => Outcome::ModelError(e),
                }
            }
            EffectKind::Tool(request) => {
                let a = &request.arguments;
                let string = |key: &str| {
                    a[key]
                        .as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| Error::Invalid("invalid frozen tool argument".into()))
                };
                let tool = match request.name.as_str() {
                    "exec" => Tool::Exec {
                        command: string("command")?,
                        cwd: a["cwd"].as_str().map(str::to_owned),
                        timeout_seconds: a["timeout_seconds"].as_f64(),
                    },
                    "read_file" => Tool::ReadFile {
                        path: string("path")?,
                        offset_bytes: a["offset_bytes"].as_u64().unwrap_or(0),
                    },
                    "write_file" => Tool::WriteFile {
                        path: string("path")?,
                        content: string("content")?,
                        append: a["append"].as_bool().unwrap_or(false),
                    },
                    _ => return Err(Error::Invalid("unsupported native tool".into())),
                };
                // Reserve return overhead INSIDE the original frozen effect cap.
                // The bridge's own timeout is authoritative only after its reply.
                let remaining = effect.deadline_ms.saturating_sub(boot_ms());
                let command_ms = remaining
                    .checked_sub(self.profile.command_return_ms)
                    .filter(|n| *n > 0)
                    .ok_or_else(|| {
                        Error::Uncertain("insufficient command return allowance".into())
                    })?;
                let limits = ToolLimits {
                    command_seconds: command_ms as f64 / 1000.0,
                    output_bytes: self.profile.scenario.parameters.limits.output_bytes.get() as u32,
                };
                Outcome::Tool(serde_json::to_value(
                    self.sandbox
                        .tool(tool, limits, deadline, cancel)
                        .await
                        .map_err(map)?,
                )?)
            }
            EffectKind::Snapshot { session_id } => {
                let profile = capture_profile(&self.profile, &task.confirmed)?;
                let mut stopped = self
                    .sandbox
                    .quiesce(deadline, cancel.clone())
                    .await
                    .map_err(map)?;
                let candidate = stopped.capture(&profile, cancel).await.map_err(map)?;
                let workspace = candidate.workspace();
                let files = workspace
                    .entries()
                    .iter()
                    .map(|e| {
                        (
                            e.path.clone(),
                            FileInfo {
                                size_bytes: e.size_bytes,
                                mode: e.mode,
                                entry_type: e.entry_type.clone(),
                                sha256: e.sha256.as_ref().map(|s| {
                                    playtime_core::Sha256Digest::new(s.clone())
                                        .expect("validated digest")
                                }),
                                link: e.link.clone(),
                            },
                        )
                    })
                    .collect();
                self.memory = Memory {
                    status: candidate.memory().status.clone(),
                    content: candidate.memory().content.clone(),
                };
                let snapshot = playtime_engine::Snapshot {
                    workspace: playtime_storage::content_ref(
                        playtime_core::ArchivePath::new(format!(
                            "sessions/{session_id}/workspace.tar"
                        ))
                        .unwrap(),
                        workspace.tar(),
                    ),
                    files,
                    memory: self.memory.clone(),
                };
                return Ok((Outcome::Snapshot(snapshot), Some(candidate)));
            }
            EffectKind::PublishCheckpoint(_) => Outcome::Published,
            EffectKind::Cleanup => {
                let cleanup = self.sandbox.cleanup(deadline, cancel).await;
                let evidence =
                    serde_json::to_vec(&serde_json::json!({"version":1,"cleanup":cleanup,
                    "observations":self.sandbox.observations()}))?;
                if evidence.len() > 256 * 1024 {
                    return Err(Error::Capacity("Docker diagnostic bound".into()));
                }
                // Service-private diagnostics are not writer receipts. Persist
                // before reporting cleanup, so uncertain writes retain admission.
                let path = self.private.join("docker-cleanup.json");
                tokio::task::spawn_blocking(move || write_once(&path, &evidence))
                    .await
                    .map_err(|_| Error::Uncertain("cleanup evidence owner lost".into()))?
                    .map_err(|e| Error::Uncertain(e.to_string()))?;
                Outcome::Cleaned(if cleanup.complete() {
                    vec![]
                } else {
                    vec![
                        "exact Docker cleanup remains uncertain; ownership records retained".into(),
                    ]
                })
            }
        };
        Ok((outcome, None))
    }
}

fn capture_profile(profile: &Profile, confirmed: &CommittedFrontier) -> Result<CaptureProfile> {
    let usage = confirmed.usage();
    let add = |a: u64, b: u64| {
        a.checked_add(b)
            .ok_or_else(|| Error::Capacity("capture reservation overflow".into()))
    };
    let storage = &profile.storage;
    // Conservative upper bounds for canonical index + source map, memory
    // leaves, event envelopes and references, on top of exact cumulative
    // writer-confirmed counts. The writer independently checks actual bytes.
    let usage = ArchiveUsage {
        content_bytes: add(
            usage.content_bytes,
            add(
                storage
                    .manifest_bytes
                    .checked_mul(2)
                    .ok_or_else(|| Error::Capacity("manifest reservation overflow".into()))?,
                profile
                    .capture
                    .memory_bytes
                    .checked_mul(2)
                    .ok_or_else(|| Error::Capacity("memory reservation overflow".into()))?,
            )?,
        )?,
        references: add(usage.references, 16)?,
        events: add(usage.events, 16)?,
        log_bytes: add(
            usage.log_bytes,
            storage
                .record_bytes
                .checked_mul(4)
                .ok_or_else(|| Error::Capacity("event reservation overflow".into()))?,
        )?,
    };
    CaptureProfile::prepare(
        profile.capture.clone(),
        profile.resources.workspace_bytes,
        storage,
        usage,
    )
    .map_err(|e| Error::Capacity(e.to_string()))
}

pub(crate) struct NativeEffects {
    prepared: Option<(ReadyImage, Ownership, Arc<Journal>, PathBuf)>,
    profile: Profile,
    sender: Option<mpsc::SyncSender<Task>>,
    receiver: Option<mpsc::Receiver<Reply>>,
    active: Option<(u64, CancellationToken, bool)>,
    retiring: bool,
    input: Option<(u64, Input)>,
    candidate: Option<SnapshotCandidate>,
}
impl NativeEffects {
    pub async fn prepare(profile: Profile, bootstrap: &Bootstrap) -> crate::Result<Self> {
        profile.validate()?;
        let cli = Cli::resolve(
            &profile.selection.executable,
            &profile.selection.socket,
            &bootstrap.private_dir.join("docker-config"),
        )
        .map_err(|_| crate::Error::Invalid("Docker selection unavailable"))?;
        if cli.selection() != &profile.selection {
            return Err(crate::Error::Invalid(
                "frozen Docker executable/socket changed",
            ));
        }
        let deadline = Instant::now()
            + Duration::from_millis(
                bootstrap
                    .startup_deadline_boot_ms
                    .saturating_sub(boot_ms())
                    .min(20000),
            );
        let engine = Engine::connect(Arc::new(cli), deadline, CancellationToken::new())
            .await
            .map_err(|_| crate::Error::Invalid("Docker daemon unavailable"))?;
        if engine.identity() != &profile.daemon {
            return Err(crate::Error::Invalid("frozen Docker daemon changed"));
        }
        let ready = engine
            .readiness(
                &serde_json::to_vec(&profile.image_lock)?,
                deadline,
                CancellationToken::new(),
            )
            .await
            .map_err(|_| crate::Error::Invalid("locked Docker image unavailable"))?;
        let binding = &bootstrap.binding;
        let owner = Ownership {
            service: binding.service_id.as_str().to_owned(),
            run: binding.run_id.to_string(),
            grant: binding.grant_id.as_str().to_owned(),
            invocation: binding.invocation_id.as_str().to_owned(),
            nonce: Ownership::random_nonce()
                .map_err(|_| crate::Error::Invalid("Docker nonce unavailable"))?,
            daemon: profile.daemon.clone(),
        };
        let journal = Arc::new(Journal {
            directory: bootstrap.private_dir.clone(),
            binding: binding.clone(),
            intent: Mutex::new(None),
            slot: Arc::new(tokio::sync::Semaphore::new(1)),
        });
        Ok(Self {
            prepared: Some((ready, owner, journal, bootstrap.private_dir.clone())),
            profile,
            sender: None,
            receiver: None,
            active: None,
            retiring: false,
            input: None,
            candidate: None,
        })
    }
}
impl Effects for NativeEffects {
    fn policy(&self) -> ToolPolicy {
        ToolPolicy::ConfirmedExternal {
            return_overhead_ms: self.profile.command_return_ms,
        }
    }
    fn begin(&mut self, started_ms: u64) -> Result<()> {
        let (ready, owner, journal, private) = self
            .prepared
            .take()
            .ok_or_else(|| Error::Invalid("native owner already begun".into()))?;
        let now_boot = boot_ms();
        let monotonic = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
        let now_ns = monotonic.tv_sec as u64 * 1_000_000_000 + monotonic.tv_nsec as u64;
        let end = started_ms
            .checked_add(self.profile.scenario.parameters.limits.wall_seconds.get() * 1000)
            .ok_or_else(|| Error::Invalid("work deadline overflow".into()))?;
        let work = WorkDeadline::at(now_ns + end.saturating_sub(now_boot) * 1_000_000)
            .map_err(|e| Error::Invalid(e.to_string()))?;
        let sandbox = Sandbox::new(
            ready,
            owner,
            self.profile.resources.clone(),
            work,
            journal.clone(),
        )
        .map_err(|e| Error::Invalid(e.to_string()))?;
        let mut actor = Actor {
            sandbox,
            profile: self.profile.clone(),
            memory: Memory {
                status: "missing".into(),
                content: String::new(),
            },
            journal,
            models: self.profile.scenario.models.clone().into(),
            calls: 0,
            private,
        };
        let (sender, tasks) = mpsc::sync_channel::<Task>(1);
        let (replies, receiver) = mpsc::sync_channel(1);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        std::thread::spawn(move || {
            while let Ok(task) = tasks.recv() {
                let result = runtime.block_on(actor.execute(&task));
                let (outcome, candidate) = match result {
                    Ok(v) => v,
                    Err(e) => (Outcome::Failed(e.to_string()), None),
                };
                if replies
                    .send(Reply {
                        id: task.effect.id,
                        outcome,
                        candidate,
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        self.sender = Some(sender);
        self.receiver = Some(receiver);
        Ok(())
    }
    fn poll(&mut self, now: u64) {
        if self.active.is_none() {
            return;
        }
        let result = self.receiver.as_ref().expect("begun").try_recv();
        let reply = match result {
            Ok(r) => r,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => Reply {
                id: self.active.as_ref().unwrap().0,
                outcome: Outcome::Failed(
                    "native effect owner lost; resources remain reserved".into(),
                ),
                candidate: None,
            },
        };
        self.active = None;
        if !self.retiring {
            if let Some(candidate) = reply.candidate {
                self.candidate = Some(candidate);
            }
            self.input = Some((
                now,
                Input::Complete {
                    effect_id: reply.id,
                    outcome: reply.outcome,
                },
            ));
        }
        self.retiring = false;
    }
    fn active(&self) -> Option<u64> {
        self.active.as_ref().map(|v| v.0).or_else(|| {
            self.input.as_ref().and_then(|(_, i)| {
                if let Input::Complete { effect_id, .. } = i {
                    Some(*effect_id)
                } else {
                    None
                }
            })
        })
    }
    fn next_input_ms(&self) -> Option<u64> {
        self.input.as_ref().map(|v| v.0)
    }
    fn pop_input(&mut self) -> Option<(u64, Input)> {
        self.input.take()
    }
    fn cancel(&mut self) {
        self.input = None;
        if let Some((_, cancel, _)) = &self.active {
            self.retiring = true;
            cancel.cancel();
        }
    }
    fn signal_cancel(&mut self) {
        if let Some((_, cancel, true)) = &self.active {
            cancel.cancel();
        }
    }
    fn retiring(&self) -> bool {
        self.active.is_some()
    }
    fn dispatch(&mut self, now: u64, effect: &Effect, confirmed: &CommittedFrontier) -> Result<()> {
        if self.active().is_some() || now >= effect.deadline_ms {
            return Err(Error::Uncertain("native dispatch owner unavailable".into()));
        }
        let cancel = CancellationToken::new();
        self.sender
            .as_ref()
            .ok_or_else(|| Error::Invalid("native owner not begun".into()))?
            .try_send(Task {
                effect: effect.clone(),
                confirmed: confirmed.clone(),
                cancel: cancel.clone(),
            })
            .map_err(|_| Error::Uncertain("native owner unavailable; no replay".into()))?;
        self.active = Some((
            effect.id,
            cancel,
            !matches!(
                effect.kind,
                EffectKind::Cleanup | EffectKind::PublishCheckpoint(_)
            ),
        ));
        Ok(())
    }
    fn checkpoint<'a>(&'a self, _: &playtime_engine::Checkpoint) -> Result<CheckpointBytes<'a>> {
        let workspace = self
            .candidate
            .as_ref()
            .ok_or_else(|| Error::Invalid("no validated native capture".into()))?
            .workspace();
        let files: BTreeMap<_, _> = workspace
            .entries()
            .iter()
            .filter_map(|e| {
                e.content_slot.map(|slot| {
                    (
                        e.path.as_str(),
                        workspace.content(slot).expect("validated slot"),
                    )
                })
            })
            .collect();
        Ok(CheckpointBytes::Contents {
            workspace: workspace.tar(),
            files,
        })
    }
}
impl Drop for NativeEffects {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[cfg(test)]
#[path = "docker_tests.rs"]
mod tests;
