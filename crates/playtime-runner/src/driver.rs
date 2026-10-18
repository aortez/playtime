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

/// Execute the frozen scripted scenario incrementally. The caller authorizes
/// this prepared identity to run; finding a folder is never authorization.
pub fn run_prepared(
    path: impl AsRef<Path>,
    prepared: &PreparedSimulation,
    clock: &mut impl DriverClock,
    options: DriverOptions,
) -> Result<RunReport> {
    let path = path.as_ref();
    let reader = ArchiveReader::open(path, prepared.limits.clone())?;
    if reader.identity() != &prepared.identity {
        return Err(Error::Invalid(
            "prepared identity changed before worker handoff".into(),
        ));
    }
    let snapshot = reader.offline_snapshot()?;
    let scenario_bytes = reader.read_content(&snapshot, &reader.manifest().configuration)?;
    let scenario: Scenario = serde_json::from_slice(&scenario_bytes)?;
    if prepared.limits.event_log_bytes != scenario.parameters.limits.log_bytes.get() {
        return Err(Error::Invalid(
            "writer allowance differs from frozen scenario".into(),
        ));
    }
    let current = reference(
        reader.manifest().worker.executable.path.as_str(),
        &executable(prepared.limits.content_bytes)?,
    );
    if current != reader.manifest().worker.executable {
        return Err(Error::Invalid(
            "executing worker bytes differ from frozen identity".into(),
        ));
    }
    let mut adapters = ScriptedAdapters::new(&scenario)?;
    if adapters.config().initial_workspace != reader.manifest().initial_workspace {
        return Err(Error::Invalid(
            "initial simulation workspace differs from frozen input".into(),
        ));
    }
    let writer = ArchiveWriter::open_prepared(path, &prepared.identity, prepared.limits.clone())?;
    let mut confirmed = writer.confirmed_frontier().clone();
    options.confirmed.publish(&confirmed)?;
    let worker = StorageThread::spawn(writer, options.writer_hooks);
    let mut worker = Some(worker);
    let start_ms = clock.now_ms();
    let mut controller = Controller::new(
        adapters.config().clone(),
        prepared.identity.run_id.clone(),
        RunMode::Simulation,
        SequentialIds,
        start_ms,
    )?;
    let mut encoder = Encoder::new(
        prepared.identity.run_id.clone(),
        prepared.limits.clone(),
        adapters.config().initial_files.clone(),
    );
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
    let mut fail_record = scenario.fail_record_sequence;
    if let Err(error) = encoder.start(&clock.utc(), prepared.identity.input_manifest.clone()) {
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
        let cancelled = options.cancellation.is_cancelled()
            || scenario
                .cancel_at_ms
                .is_some_and(|time| now.saturating_sub(start_ms) >= time);
        if cancelled && !cancel_notified {
            cancel_notified = true;
            controller.advance(now, Input::Cancel)?;
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
                        if let Some((effect_id, _, _)) = &completed.publication.checkpoint {
                            let now = clock.now_ms();
                            if !cancel_notified
                                && (options.cancellation.is_cancelled()
                                    || scenario
                                        .cancel_at_ms
                                        .is_some_and(|time| now.saturating_sub(start_ms) >= time))
                            {
                                cancel_notified = true;
                                controller.advance(now, Input::Cancel)?;
                            }
                            controller.advance(
                                now,
                                Input::PublicationCommitted {
                                    effect_id: *effect_id,
                                },
                            )?;
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
                &adapters,
                queue_since,
                &scenario,
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
                    || scenario
                        .cancel_at_ms
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
                    EffectKind::Snapshot { .. } | EffectKind::Cleanup
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
            let dispatch_result = adapters.dispatch(dispatch_time, &effect);
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
                let EffectKind::PublishCheckpoint(checkpoint) = &effect.kind else {
                    unreachable!()
                };
                let bytes = &adapters.artifacts()[checkpoint.snapshot.workspace.path.as_str()];
                match encoder.checkpoint(
                    &clock.utc(),
                    effect.id,
                    checkpoint,
                    bytes,
                    confirmed.frontier().committed_sequence,
                ) {
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
            &adapters,
            queue_since,
            &scenario,
            start_ms,
            cancel_notified,
            now,
        );
        clock.idle(next, false);
    }
}

fn next_time(
    controller: &Controller<SequentialIds>,
    adapters: &ScriptedAdapters,
    queued_at: Option<u64>,
    scenario: &Scenario,
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
    if !cancelled && let Some(time) = scenario.cancel_at_ms {
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
