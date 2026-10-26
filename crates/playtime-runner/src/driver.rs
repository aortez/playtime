use std::{
    collections::BTreeSet,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use playtime_core::RunMode;
use playtime_engine::{
    Controller, EffectKind, Input, Outcome, RunResult,
    simulation::{Dispatch, Scenario, ScriptedAdapters, SequentialIds},
};
use playtime_protocol::archive::WorkerIdentity;
use playtime_storage::{ArchiveReader, ArchiveWriter, CommittedFrontier};

use crate::{
    Cancellation, DriverClock, Error, NoWriterHooks, PendingPublication, PreparedSimulation,
    Result, WriterHooks,
    mapping::{Encoder, Publication, STREAM_BATCH_BYTES, STREAM_BATCH_MS},
    prepare::{executable, reference},
    writer::StorageThread,
};

pub struct DriverOptions {
    pub cancellation: Cancellation,
    pub confirmed: crate::LatestReceipt,
    pub writer_hooks: Box<dyn WriterHooks>,
    /// Supervision bound for unresolved filesystem work, not evidence that the
    /// operation failed. Returning retains its owner in pending_publication.
    pub publication_wait: Duration,
}
impl Default for DriverOptions {
    fn default() -> Self {
        Self {
            cancellation: Cancellation::default(),
            confirmed: crate::LatestReceipt::default(),
            writer_hooks: Box::new(NoWriterHooks),
            publication_wait: Duration::from_secs(5),
        }
    }
}

pub struct RunReport {
    /// In-memory cleanup/result is diagnostic unless terminal_committed is true.
    pub result: Option<RunResult>,
    pub confirmed: CommittedFrontier,
    pub terminal_committed: bool,
    pub diagnostics: Vec<String>,
    pub dispatches: Vec<Dispatch>,
    pub accounted_log_bytes: u64,
    pub peak_queued_bytes: u64,
    pub pending_publication: Option<PendingPublication>,
}

struct InFlight {
    publication: Arc<Publication>,
    started: Instant,
}

/// Verified, exclusively owned preparation. This value is deliberately neither
/// Clone nor serializable, and confers no permission to Begin. The private
/// control owner must consume its durable grant before calling `execute`.
pub struct PreparedExecution {
    writer: ArchiveWriter,
    prepared: PreparedSimulation,
    cancel_at_ms: Option<u64>,
    fail_record_sequence: Option<u64>,
    mode: RunMode,
    config: ExecutionConfig,
    adapters: Box<dyn crate::Effects>,
    image: WorkerIdentity,
}

enum ExecutionConfig {
    Memento(Box<playtime_engine::ControllerConfig>),
    #[cfg(feature = "test-harness")]
    Workspace(Box<playtime_engine::WorkspaceConfig>),
}

impl PreparedExecution {
    /// Acquire the writer first, then verify through its returned zero receipt.
    /// No controller, work clock, event or adapter dispatch exists yet.
    pub fn acquire(path: impl AsRef<Path>, prepared: &PreparedSimulation) -> Result<Self> {
        Self::acquire_effects(path, prepared, None)
    }

    /// Native composition supplies its owned effect port; the frozen profile is
    /// independently verified while retaining the same prepared writer.
    pub fn acquire_docker(
        path: impl AsRef<Path>,
        prepared: &PreparedSimulation,
        effects: Box<dyn crate::Effects>,
    ) -> Result<Self> {
        Self::acquire_effects(path, prepared, Some(effects))
    }

    fn acquire_effects(
        path: impl AsRef<Path>,
        prepared: &PreparedSimulation,
        native: Option<Box<dyn crate::Effects>>,
    ) -> Result<Self> {
        let path = path.as_ref();
        let writer =
            ArchiveWriter::open_prepared(path, &prepared.identity, prepared.limits.clone())?;
        let reader = ArchiveReader::open(path, prepared.limits.clone())?;
        if reader.identity() != &prepared.identity {
            return Err(Error::Invalid(
                "prepared identity changed before worker handoff".into(),
            ));
        }
        if reader.manifest().schema_version != 1 {
            return Err(Error::Invalid(
                "production worker supports only the memento v1 execution contract".into(),
            ));
        }
        let snapshot = reader.live_snapshot(writer.confirmed_frontier())?;
        let scenario_bytes = reader.read_content(&snapshot, &reader.manifest().configuration)?;
        let current = reference(
            reader.manifest().worker.executable.path.as_str(),
            &executable(prepared.limits.content_bytes)?,
        );
        if current != reader.manifest().worker.executable {
            return Err(Error::Invalid(
                "executing worker bytes differ from frozen identity".into(),
            ));
        }
        let (scenario, mode, config, adapters): (_, _, _, Box<dyn crate::Effects>) =
            if let Some(effects) = native {
                let profile: crate::docker::Profile = serde_json::from_slice(&scenario_bytes)?;
                profile.validate()?;
                if reader
                    .manifest()
                    .files
                    .iter()
                    .any(|r| r.path.as_str() == playtime_protocol::recovery::EXECUTION_CONTRACT)
                    || prepared.limits != profile.storage
                    || reader.descriptor().mode != profile.mode
                {
                    return Err(Error::Invalid(
                        "invalid Docker profile or simulation recovery authorization".into(),
                    ));
                }
                let config = profile.config();
                (profile.scenario, profile.mode, config, effects)
            } else {
                let scenario: Scenario = serde_json::from_slice(&scenario_bytes)?;
                crate::recovery::verify_worker_contract(
                    &reader,
                    &snapshot,
                    &scenario,
                    &prepared.limits,
                )?;
                let adapters = ScriptedAdapters::new(&scenario)?;
                let config = adapters.config().clone();
                (scenario, RunMode::Simulation, config, Box::new(adapters))
            };
        if prepared.limits.event_log_bytes != scenario.parameters.limits.log_bytes.get()
            || config.initial_workspace != reader.manifest().initial_workspace
        {
            return Err(Error::Invalid(
                "frozen input/configuration allowance mismatch".into(),
            ));
        }
        Ok(Self {
            writer,
            prepared: prepared.clone(),
            cancel_at_ms: scenario.cancel_at_ms,
            fail_record_sequence: scenario.fail_record_sequence,
            mode,
            config: ExecutionConfig::Memento(Box::new(config)),
            adapters,
            image: reader.manifest().worker.clone(),
        })
    }

    /// Explicit feature-gated CPU probe. Production acquire routes reject v2.
    #[cfg(feature = "test-harness")]
    pub fn acquire_probe(path: impl AsRef<Path>, prepared: &PreparedSimulation) -> Result<Self> {
        let writer = ArchiveWriter::open_prepared(
            path.as_ref(),
            &prepared.identity,
            prepared.limits.clone(),
        )?;
        let reader = ArchiveReader::open(path.as_ref(), prepared.limits.clone())?;
        let snapshot = reader.live_snapshot(writer.confirmed_frontier())?;
        if reader.manifest().schema_version != 2 || reader.descriptor().mode != RunMode::Simulation
        {
            return Err(Error::Invalid(
                "test worker requires execution v2 simulation".into(),
            ));
        }
        let current = reference(
            reader.manifest().worker.executable.path.as_str(),
            &executable(prepared.limits.content_bytes)?,
        );
        if current != reader.manifest().worker.executable {
            return Err(Error::Invalid(
                "test worker executable differs from frozen capability".into(),
            ));
        }
        let scenario = crate::probe::Scenario::decode(
            &reader.read_content(&snapshot, &reader.manifest().configuration)?,
        )?;
        let config = scenario.config();
        if prepared.limits
            != (playtime_storage::StorageLimits {
                event_log_bytes: config.parameters.limits.log_bytes.get(),
                ..playtime_storage::StorageLimits::default()
            })
            || config.initial_workspace != reader.manifest().initial_workspace
        {
            return Err(Error::Invalid(
                "test profile frozen allowance/input mismatch".into(),
            ));
        }
        Ok(Self {
            writer,
            prepared: prepared.clone(),
            cancel_at_ms: scenario.cancel_at_ms,
            fail_record_sequence: scenario.fail_record_sequence,
            mode: RunMode::Simulation,
            config: ExecutionConfig::Workspace(Box::new(config)),
            adapters: Box::new(crate::probe::Adapter::new(scenario)),
            image: reader.manifest().worker.clone(),
        })
    }

    pub fn confirmed(&self) -> &CommittedFrontier {
        self.writer.confirmed_frontier()
    }
    pub fn image(&self) -> &WorkerIdentity {
        &self.image
    }

    /// Consume the exact prepared writer after externally authorized Begin.
    pub fn execute(
        self,
        clock: &mut impl DriverClock,
        options: DriverOptions,
    ) -> Result<RunReport> {
        execute(self, clock, options)
    }
}

/// Standalone convenience wrapper; the caller supplies execution authorization.
pub fn run_prepared(
    path: impl AsRef<Path>,
    prepared: &PreparedSimulation,
    clock: &mut impl DriverClock,
    options: DriverOptions,
) -> Result<RunReport> {
    PreparedExecution::acquire(path, prepared)?.execute(clock, options)
}

fn execute(
    owned: PreparedExecution,
    clock: &mut impl DriverClock,
    options: DriverOptions,
) -> Result<RunReport> {
    let PreparedExecution {
        writer,
        prepared,
        cancel_at_ms,
        fail_record_sequence,
        mut adapters,
        mode,
        config,
        ..
    } = owned;
    let mut confirmed = writer.confirmed_frontier().clone();
    options.confirmed.publish(&confirmed)?;
    let worker = StorageThread::spawn(writer, options.writer_hooks);
    let mut worker = Some(worker);
    let start_ms = clock.now_ms();
    adapters.begin(start_ms)?;
    let (mut controller, mut encoder) = match config {
        ExecutionConfig::Memento(config) => (
            Controller::with_tool_policy(
                (*config).clone(),
                prepared.identity.run_id.clone(),
                mode,
                SequentialIds,
                start_ms,
                adapters.policy(),
            )?,
            Encoder::new(
                prepared.identity.run_id.clone(),
                prepared.limits.clone(),
                config.initial_files.clone(),
            ),
        ),
        #[cfg(feature = "test-harness")]
        ExecutionConfig::Workspace(config) => {
            let mut encoder = Encoder::new(
                prepared.identity.run_id.clone(),
                prepared.limits.clone(),
                config.initial_files.clone(),
            );
            encoder.use_execution_v2();
            (
                Controller::test_workspace(
                    *config,
                    prepared.identity.run_id.clone(),
                    SequentialIds,
                    start_ms,
                )?,
                encoder,
            )
        }
    };
    let mut diagnostics = Vec::new();
    let mut healthy = true;
    let mut flush = true;
    let mut queue_since = None;
    let mut in_flight: Option<InFlight> = None;
    let mut intents = BTreeSet::new();
    let mut dispatches = Vec::new();
    let mut dispatched_ids = BTreeSet::new();
    let mut capacity_notified = false;
    let mut cancel_notified = false;
    let mut terminal_queued = false;
    let mut terminal_committed = false;
    let mut committed_log_bytes = 0;
    let mut peak_queued_bytes = 0;
    let mut fail_record = fail_record_sequence;
    if let Err(error) =
        encoder.start_mode(&clock.utc(), prepared.identity.input_manifest.clone(), mode)
    {
        fail(
            &mut controller,
            &mut encoder,
            clock.now_ms(),
            error,
            &mut healthy,
            &mut diagnostics,
        )?;
    }
    loop {
        let now = clock.now_ms();
        adapters.poll(now);
        let cancelled = options.cancellation.is_cancelled()
            || cancel_at_ms.is_some_and(|time| now.saturating_sub(start_ms) >= time);
        if cancelled && !cancel_notified {
            cancel_notified = true;
            controller.advance(now, Input::Cancel)?;
            adapters.signal_cancel();
        } else if controller.next_wakeup_ms().is_some_and(|time| now >= time) {
            controller.advance(now, Input::Tick)?;
        }
        if adapters
            .active()
            .is_some_and(|id| controller.pending().is_none_or(|effect| effect.id != id))
        {
            adapters.cancel();
        }

        let records = controller.take_observations();
        let controls = controller.take_control_updates();
        let barrier = !controls.is_empty()
            || records
                .iter()
                .any(|entry| entry.record.kind != "model_chunk");
        if healthy {
            let failure = fail_record.and_then(|sequence| {
                records
                    .iter()
                    .find(|entry| entry.record.sequence == sequence)
            });
            if failure.is_some() {
                fail_record = None;
                fail(
                    &mut controller,
                    &mut encoder,
                    now,
                    Error::Uncertain("scripted observation publication failure".into()),
                    &mut healthy,
                    &mut diagnostics,
                )?;
            } else {
                let mapped = encoder
                    .observations(&clock.utc(), records)
                    .and_then(|()| encoder.controls(&clock.utc(), controls));
                if let Err(error) = mapped {
                    fail(
                        &mut controller,
                        &mut encoder,
                        now,
                        error,
                        &mut healthy,
                        &mut diagnostics,
                    )?;
                }
                flush |= barrier;
                if encoder.has_queue() {
                    queue_since.get_or_insert(now);
                }
                if encoder.capacity_exhausted && !capacity_notified {
                    capacity_notified = true;
                    controller.advance(now, Input::ExternalLogBudgetExhausted)?;
                    continue;
                }
            }
        }
        peak_queued_bytes = peak_queued_bytes.max(
            encoder.queued_bytes()
                + in_flight
                    .as_ref()
                    .map_or(0, |pending| pending.publication.queued_bytes()),
        );

        if in_flight.is_some() {
            if let Some(result) = worker.as_ref().unwrap().poll() {
                let completed = in_flight.take().unwrap();
                match result {
                    Ok(receipt) => {
                        options.confirmed.publish(&receipt)?;
                        confirmed = receipt;
                        committed_log_bytes += completed.publication.log_cost();
                        encoder.confirmed(&completed.publication);
                        for event in &completed.publication.events {
                            if event.kind == "effect_intent" {
                                intents.insert(event.effect_id.unwrap());
                            }
                            if event.kind == "run_finished" {
                                terminal_committed = true;
                            }
                        }
                        if let Some(effect_id) = completed
                            .publication
                            .checkpoint
                            .as_ref()
                            .map(|(id, _, _)| *id)
                            .or_else(|| completed.publication.capture.as_ref().map(|(id, _)| *id))
                        {
                            let now = clock.now_ms();
                            if !cancel_notified
                                && (options.cancellation.is_cancelled()
                                    || cancel_at_ms
                                        .is_some_and(|time| now.saturating_sub(start_ms) >= time))
                            {
                                cancel_notified = true;
                                controller.advance(now, Input::Cancel)?;
                            }
                            controller.advance(now, Input::PublicationCommitted { effect_id })?;
                        }
                    }
                    Err(error) => {
                        if diagnostics.len() < 8 {
                            diagnostics.push(format!("retired failed publication {}..{} ({} events, {} retained bytes); no retry",
                                completed.publication.events.first().unwrap().sequence,
                                completed.publication.events.last().unwrap().sequence,
                                completed.publication.events.len(), completed.publication.queued_bytes()));
                        }
                        fail(
                            &mut controller,
                            &mut encoder,
                            clock.now_ms(),
                            error,
                            &mut healthy,
                            &mut diagnostics,
                        )?;
                    }
                }
                continue;
            }
            if healthy && in_flight.as_ref().unwrap().started.elapsed() >= options.publication_wait
            {
                fail(&mut controller, &mut encoder, now, Error::Uncertain("filesystem publication remains unresolved; ownership retained for supervision".into()), &mut healthy, &mut diagnostics)?;
                continue;
            }
        }

        if healthy
            && in_flight.is_none()
            && encoder.has_queue()
            && (flush
                || encoder.queue_log_bytes() >= STREAM_BATCH_BYTES
                || queue_since.is_some_and(|time| now.saturating_sub(time) >= STREAM_BATCH_MS)
                || controller.result().is_some())
        {
            let publication = Arc::new(encoder.batch().unwrap());
            worker.as_ref().unwrap().begin(publication.clone())?;
            in_flight = Some(InFlight {
                publication,
                started: Instant::now(),
            });
            if !encoder.has_queue() {
                flush = false;
                queue_since = None;
            }
            continue;
        }
        if controller.result().is_some() && (!healthy || terminal_committed) {
            let pending_publication = in_flight.take().map(|pending| PendingPublication {
                worker: worker.take().unwrap(),
                publication: pending.publication,
                confirmed: options.confirmed.clone(),
            });
            if let Some(owner) = worker.take() {
                owner.close();
            }
            return Ok(RunReport {
                result: controller.result().cloned(),
                confirmed,
                terminal_committed,
                diagnostics,
                dispatches,
                accounted_log_bytes: committed_log_bytes,
                peak_queued_bytes,
                pending_publication,
            });
        }
        if healthy && in_flight.is_some() {
            let wakeup = next_time(
                &controller,
                adapters.as_ref(),
                queue_since,
                cancel_at_ms,
                start_ms,
                cancel_notified,
                now,
            );
            clock.idle(wakeup, true);
            continue;
        }
        if controller.result().is_some() {
            if !terminal_queued {
                if let Err(error) = encoder.terminal(&clock.utc(), controller.result().unwrap()) {
                    fail(
                        &mut controller,
                        &mut encoder,
                        now,
                        error,
                        &mut healthy,
                        &mut diagnostics,
                    )?;
                } else {
                    terminal_queued = true;
                    flush = true;
                }
            }
            continue;
        }
        let effect = controller
            .pending()
            .expect("controller yields pending work or result")
            .clone();
        if healthy && encoder.has_queue() && adapters.active() != Some(effect.id) {
            flush = true;
            continue;
        }
        if adapters.active() != Some(effect.id) {
            adapters.cancel();
            if adapters.retiring() {
                clock.idle(now.saturating_add(1), true);
                continue;
            }
            if !healthy && dispatched_ids.contains(&effect.id) {
                controller.advance(
                    now,
                    Input::Complete {
                        effect_id: effect.id,
                        outcome: Outcome::Failed(
                            "archive publication unavailable or unresolved".into(),
                        ),
                    },
                )?;
                continue;
            }
            if healthy && !intents.contains(&effect.id) {
                return Err(Error::Invalid(
                    "effect reached dispatch without a committed intent".into(),
                ));
            }
            // Final clock/cancel check and controller dispatch occur after all
            // preceding writes. No persistence, await, or blocking hook is
            // allowed between this check and initiating the scripted adapter.
            let dispatch_time = clock.now_ms();
            if !cancel_notified
                && (options.cancellation.is_cancelled()
                    || cancel_at_ms
                        .is_some_and(|time| dispatch_time.saturating_sub(start_ms) >= time))
            {
                continue;
            }
            controller.advance(
                dispatch_time,
                Input::Dispatched {
                    effect_id: effect.id,
                },
            )?;
            if controller.pending().is_none_or(|pending| {
                pending.id != effect.id || dispatch_time >= pending.deadline_ms
            }) {
                continue;
            }
            dispatched_ids.insert(effect.id);
            if !healthy
                && !matches!(
                    effect.kind,
                    EffectKind::Snapshot { .. }
                        | EffectKind::CaptureWorkspace
                        | EffectKind::Cleanup
                )
            {
                controller.advance(
                    dispatch_time,
                    Input::Complete {
                        effect_id: effect.id,
                        outcome: Outcome::Failed("archive persistence unavailable".into()),
                    },
                )?;
                continue;
            }
            let dispatch_result = adapters.dispatch(dispatch_time, &effect, &confirmed);
            dispatches.push(Dispatch {
                at_ms: dispatch_time,
                effect: effect.clone(),
            });
            if let Err(error) = dispatch_result {
                controller.advance(
                    dispatch_time,
                    Input::Complete {
                        effect_id: effect.id,
                        outcome: Outcome::Failed(error.to_string()),
                    },
                )?;
                continue;
            }
        }
        if adapters.next_input_ms().is_some_and(|time| time <= now) {
            let (_, input) = adapters.pop_input().unwrap();
            if matches!(
                input,
                Input::Complete {
                    outcome: Outcome::Published,
                    ..
                }
            ) {
                let publication = (|| -> Result<Publication> {
                    match &effect.kind {
                        EffectKind::PublishCheckpoint(checkpoint) => {
                            match adapters.checkpoint(checkpoint)? {
                                crate::CheckpointBytes::Scripted(bytes) => encoder.checkpoint(
                                    &clock.utc(),
                                    effect.id,
                                    checkpoint,
                                    bytes,
                                    confirmed.frontier().committed_sequence,
                                ),
                                crate::CheckpointBytes::Contents { workspace, files } => encoder
                                    .checkpoint_contents(
                                        &clock.utc(),
                                        effect.id,
                                        checkpoint,
                                        workspace,
                                        &files,
                                        confirmed.frontier().committed_sequence,
                                    ),
                            }
                        }
                        EffectKind::PublishCapture(capture) => {
                            let crate::CheckpointBytes::Contents { workspace, files } =
                                adapters.capture(capture)?
                            else {
                                return Err(Error::Invalid(
                                    "run-scoped capture requires explicit file bytes".into(),
                                ));
                            };
                            encoder.capture_contents(
                                &clock.utc(),
                                effect.id,
                                capture,
                                workspace,
                                &files,
                                confirmed.frontier().committed_sequence,
                            )
                        }
                        _ => Err(Error::Invalid(
                            "publication reply for non-publication effect".into(),
                        )),
                    }
                })();
                match publication {
                    Ok(publication) => {
                        if encoder.capacity_exhausted && !capacity_notified {
                            capacity_notified = true;
                            controller
                                .advance(clock.now_ms(), Input::ExternalLogBudgetExhausted)?;
                        }
                        let publication = Arc::new(publication);
                        worker.as_ref().unwrap().begin(publication.clone())?;
                        in_flight = Some(InFlight {
                            publication,
                            started: Instant::now(),
                        });
                    }
                    Err(error) => fail(
                        &mut controller,
                        &mut encoder,
                        clock.now_ms(),
                        error,
                        &mut healthy,
                        &mut diagnostics,
                    )?,
                }
            } else {
                controller.advance(clock.now_ms(), input)?;
            }
            continue;
        }
        let next = next_time(
            &controller,
            adapters.as_ref(),
            queue_since,
            cancel_at_ms,
            start_ms,
            cancel_notified,
            now,
        );
        clock.idle(next, false);
    }
}

fn next_time(
    controller: &Controller<SequentialIds>,
    adapters: &dyn crate::Effects,
    queued_at: Option<u64>,
    cancel_at_ms: Option<u64>,
    start: u64,
    cancelled: bool,
    now: u64,
) -> u64 {
    let mut next = controller.next_wakeup_ms().unwrap_or(now.saturating_add(1));
    if let Some(time) = adapters.next_input_ms() {
        next = next.min(time);
    }
    if let Some(time) = queued_at {
        next = next.min(time.saturating_add(STREAM_BATCH_MS));
    }
    if !cancelled && let Some(time) = cancel_at_ms {
        next = next.min(start.saturating_add(time));
    }
    next.max(now)
}
fn fail(
    controller: &mut Controller<SequentialIds>,
    encoder: &mut Encoder,
    now: u64,
    error: Error,
    healthy: &mut bool,
    diagnostics: &mut Vec<String>,
) -> Result<()> {
    let (events, bytes) = encoder.retire();
    if diagnostics.len() < 8 {
        diagnostics.push(
            format!("{error}; retired {events} queued events ({bytes} bytes)")
                .chars()
                .take(1024)
                .collect(),
        );
    }
    if matches!(error, Error::LogLimit(_)) {
        controller.advance(now, Input::ExternalLogBudgetExhausted)?;
    }
    controller.advance(now, Input::PersistenceFailed(error.to_string()))?;
    *healthy = false;
    Ok(())
}
