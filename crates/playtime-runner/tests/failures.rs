mod common;

use common::*;
use playtime_core::StopReason;
use playtime_engine::EffectKind;
use playtime_protocol::{archive_queries::ArchiveFact, bridge::BridgeEvent};
use playtime_runner::{
    ArchiveQueries, Cancellation, CommitContext, DriverOptions, ReceiptDelivery, VirtualClock,
    WriterHooks, run_prepared,
};
use playtime_storage::{
    ArchiveWriter, CommittedFrontier, PublicationObserver, StorageLimits, WriteStep,
};
use std::{
    io,
    sync::{Arc, Mutex, mpsc},
    time::Duration,
};

struct FailAt {
    step: WriteStep,
    after: bool,
}
impl PublicationObserver for FailAt {
    fn before(&mut self, step: WriteStep) -> io::Result<()> {
        if step == self.step && !self.after {
            Err(io::Error::other("injected publication failure"))
        } else {
            Ok(())
        }
    }
    fn after(&mut self, step: WriteStep) -> io::Result<()> {
        if step == self.step && self.after {
            Err(io::Error::other("injected publication failure"))
        } else {
            Ok(())
        }
    }
}
#[derive(Clone, Copy, Debug)]
enum Boundary {
    FirstIntent,
    ModelIntent,
    Settlement,
    Checkpoint,
    Terminal,
}
fn matches(context: &CommitContext, boundary: Boundary) -> bool {
    match boundary {
        Boundary::FirstIntent => context.kinds.iter().any(|kind| kind == "run_started"),
        Boundary::ModelIntent => {
            context.kinds.iter().any(|kind| kind == "effect_intent")
                && context.effect_ids.contains(&2)
        }
        Boundary::Settlement => context.kinds.iter().any(|kind| kind == "request_finished"),
        Boundary::Checkpoint => context.checkpoint_effect.is_some(),
        Boundary::Terminal => context.kinds.iter().any(|kind| kind == "run_finished"),
    }
}
struct FaultHook {
    boundary: Boundary,
    step: WriteStep,
    after: bool,
}
impl WriterHooks for FaultHook {
    fn before(
        &mut self,
        context: &CommitContext,
        writer: &mut ArchiveWriter,
    ) -> playtime_runner::Result<()> {
        if matches(context, self.boundary) {
            writer.set_observer(Box::new(FailAt {
                step: self.step,
                after: self.after,
            }));
        }
        Ok(())
    }
}

#[test]
fn commit_failures_gate_dispatch_and_leave_incomplete_evidence_without_retry() {
    for boundary in [
        Boundary::FirstIntent,
        Boundary::ModelIntent,
        Boundary::Settlement,
        Boundary::Checkpoint,
        Boundary::Terminal,
    ] {
        for after in [false, true] {
            let temp = Temp::new();
            let prepared = prepared(&temp, &scenario());
            let report = run_prepared(
                temp.archive(),
                &prepared,
                &mut clock(),
                DriverOptions {
                    writer_hooks: Box::new(FaultHook {
                        boundary,
                        step: WriteStep::MarkerRename,
                        after,
                    }),
                    ..DriverOptions::default()
                },
            )
            .unwrap();
            assert!(!report.terminal_committed, "{boundary:?} after={after}");
            assert!(report.pending_publication.is_none());
            assert!(!report.diagnostics.is_empty());
            let models = report
                .dispatches
                .iter()
                .filter(|item| matches!(item.effect.kind, EffectKind::Model(_)))
                .count();
            let tools = report
                .dispatches
                .iter()
                .filter(|item| matches!(item.effect.kind, EffectKind::Tool(_)))
                .count();
            match boundary {
                Boundary::FirstIntent | Boundary::ModelIntent => {
                    assert_eq!(models, 0);
                    assert_eq!(tools, 0);
                }
                Boundary::Settlement => {
                    assert_eq!(models, 1);
                    assert_eq!(tools, 0);
                }
                Boundary::Checkpoint => assert_eq!(models, 2),
                Boundary::Terminal => assert_eq!(models, 4),
            }
            let live =
                ArchiveQueries::live(temp.archive(), StorageLimits::default(), &report.confirmed)
                    .unwrap();
            assert!(live.run().unwrap().incomplete_evidence);
            let recovered =
                ArchiveQueries::offline(temp.archive(), StorageLimits::default()).unwrap();
            if matches!(boundary, Boundary::ModelIntent) && after {
                assert_eq!(recovered.run().unwrap().unsettled_model_intents.len(), 1);
                assert!(recovered.run().unwrap().incomplete_evidence);
            }
            // A visible marker after lost acknowledgment is not promoted into
            // report.confirmed, even when offline inspection can recover more.
            assert!(
                recovered.snapshot().frontier().committed_sequence
                    >= report.confirmed.frontier().committed_sequence
            );
        }
    }
}

struct CancelAfterIntent {
    cancellation: Cancellation,
}
impl WriterHooks for CancelAfterIntent {
    fn after(&mut self, context: &CommitContext, _: &CommittedFrontier) -> ReceiptDelivery {
        if matches(context, Boundary::ModelIntent) {
            self.cancellation.cancel();
        }
        ReceiptDelivery::Deliver
    }
}
#[test]
fn cancellation_after_intent_commit_before_dispatch_releases_only_the_unstarted_reservation() {
    let temp = Temp::new();
    let prepared = prepared(&temp, &scenario());
    let cancellation = Cancellation::default();
    let report = run_prepared(
        temp.archive(),
        &prepared,
        &mut clock(),
        DriverOptions {
            cancellation: cancellation.clone(),
            writer_hooks: Box::new(CancelAfterIntent { cancellation }),
            ..DriverOptions::default()
        },
    )
    .unwrap();
    assert!(report.terminal_committed, "{:?}", report.diagnostics);
    let result = report.result.as_ref().unwrap();
    assert_eq!(result.reason, StopReason::Interrupted);
    assert_eq!(result.usage.budget_charged_tokens, 0);
    assert!(
        !report
            .dispatches
            .iter()
            .any(|item| matches!(item.effect.kind, EffectKind::Model(_)))
    );
    let queries = ArchiveQueries::offline(temp.archive(), StorageLimits::default()).unwrap();
    assert!(queries.run().unwrap().unsettled_model_intents.is_empty());
    let history = queries.history(0, 1000, true).unwrap();
    assert_eq!(
        history
            .entries
            .iter()
            .filter(|entry| matches!(
                &entry.fact,
                ArchiveFact::Control {
                    event: BridgeEvent::RequestNotDispatched(_)
                }
            ))
            .count(),
        1
    );
    assert!(
        !history
            .entries
            .iter()
            .any(|entry| entry.envelope.kind == "request_finished")
    );
}

struct CheckpointRace {
    clock: VirtualClock,
    cancellation: Option<Cancellation>,
    at_ms: u64,
    lose: bool,
    fired: bool,
}
impl WriterHooks for CheckpointRace {
    fn after(&mut self, context: &CommitContext, _: &CommittedFrontier) -> ReceiptDelivery {
        if context.checkpoint_effect.is_some() && !self.fired {
            self.fired = true;
            if let Some(cancel) = &self.cancellation {
                cancel.cancel();
            }
            self.clock.advance_to(self.at_ms);
            if self.lose {
                return ReceiptDelivery::Lose;
            }
        }
        ReceiptDelivery::Deliver
    }
}
#[test]
fn checkpoint_receipts_on_both_sides_of_deadline_and_cancel_have_consistent_terminal_references() {
    for (at_ms, cancel, expected, sessions) in [
        (20_299, false, StopReason::AgentFinished, 2),
        (20_300, false, StopReason::InfrastructureFailure, 1),
        (300_000, false, StopReason::WallLimit, 1),
        (20_300, true, StopReason::Interrupted, 1),
    ] {
        let temp = Temp::new();
        let prepared = prepared(&temp, &scenario());
        let mut clock = clock();
        let cancellation = Cancellation::default();
        let hook = CheckpointRace {
            clock: clock.clone(),
            cancellation: cancel.then_some(cancellation.clone()),
            at_ms,
            lose: false,
            fired: false,
        };
        let report = run_prepared(
            temp.archive(),
            &prepared,
            &mut clock,
            DriverOptions {
                cancellation,
                writer_hooks: Box::new(hook),
                ..DriverOptions::default()
            },
        )
        .unwrap();
        assert!(
            report.terminal_committed,
            "at {at_ms}, cancel {cancel}: {:?}",
            report.diagnostics
        );
        let result = report.result.as_ref().unwrap();
        assert_eq!(result.reason, expected);
        assert_eq!(result.sessions_started, sessions);
        assert_eq!(result.last_checkpoint.as_ref().unwrap().ordinal, sessions);
        let queries = ArchiveQueries::offline(temp.archive(), StorageLimits::default()).unwrap();
        let state = queries.run().unwrap();
        assert_eq!(state.first_stop, Some(expected));
        assert_eq!(
            state.outcome.unwrap().last_checkpoint,
            queries.snapshot().inspection().checkpoints.last().cloned()
        );
    }
}

#[test]
fn a_lost_checkpoint_receipt_never_becomes_a_controller_acknowledgment_or_terminal_commit() {
    let temp = Temp::new();
    let prepared = prepared(&temp, &scenario());
    let mut clock = clock();
    let hook = CheckpointRace {
        clock: clock.clone(),
        cancellation: None,
        at_ms: 20_300,
        lose: true,
        fired: false,
    };
    let report = run_prepared(
        temp.archive(),
        &prepared,
        &mut clock,
        DriverOptions {
            writer_hooks: Box::new(hook),
            ..DriverOptions::default()
        },
    )
    .unwrap();
    assert!(!report.terminal_committed);
    assert!(report.result.as_ref().unwrap().last_checkpoint.is_none());
    let queries = ArchiveQueries::offline(temp.archive(), StorageLimits::default()).unwrap();
    assert_eq!(queries.checkpoints().unwrap().len(), 1);
    assert!(queries.run().unwrap().incomplete_evidence);
    let live =
        ArchiveQueries::live(temp.archive(), StorageLimits::default(), &report.confirmed).unwrap();
    assert!(live.checkpoints().unwrap().is_empty());
}

struct BlockCheckpoint {
    release: mpsc::Receiver<()>,
    entered: Arc<Mutex<bool>>,
}
impl WriterHooks for BlockCheckpoint {
    fn before(
        &mut self,
        context: &CommitContext,
        _: &mut ArchiveWriter,
    ) -> playtime_runner::Result<()> {
        if context.checkpoint_effect.is_some() {
            *self.entered.lock().unwrap() = true;
            self.release.recv().unwrap();
        }
        Ok(())
    }
}
#[test]
fn unresolved_storage_retains_ownership_and_can_commit_after_the_driver_returns_incomplete() {
    let temp = Temp::new();
    let prepared = prepared(&temp, &scenario());
    let (release, receiver) = mpsc::channel();
    let entered = Arc::new(Mutex::new(false));
    let report = run_prepared(
        temp.archive(),
        &prepared,
        &mut clock(),
        DriverOptions {
            publication_wait: Duration::from_millis(100),
            writer_hooks: Box::new(BlockCheckpoint {
                release: receiver,
                entered: entered.clone(),
            }),
            ..DriverOptions::default()
        },
    )
    .unwrap();
    assert!(*entered.lock().unwrap());
    assert!(!report.terminal_committed);
    let pending = report.pending_publication.unwrap();
    assert!(pending.checkpoint_effect().is_some());
    assert!(ArchiveQueries::offline(temp.archive(), StorageLimits::default()).is_err());
    release.send(()).unwrap();
    let start = std::time::Instant::now();
    let receipt = loop {
        if let Some(result) = pending.poll() {
            break result.unwrap();
        }
        assert!(start.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(1));
    };
    let queries = ArchiveQueries::live(temp.archive(), StorageLimits::default(), &receipt).unwrap();
    assert_eq!(queries.checkpoints().unwrap().len(), 1);
    assert!(queries.run().unwrap().incomplete_evidence);
}
