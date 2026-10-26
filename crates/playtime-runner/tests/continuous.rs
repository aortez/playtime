//! Real controller/writer/read path, with CPU-only effects. No command executes.
mod common;
use common::*;
use playtime_core::{ArchivePath, RequestPurpose, RunId, StopReason};
use playtime_engine::{Capture, Checkpoint, Effect, EffectKind, Input, Outcome, WorkspaceSnapshot};
use playtime_runner::{
    docker::{CompletedResponse, ContinuousProfile},
    *,
};
use playtime_storage::{CommittedFrontier, StorageLimits, content_ref};
use serde_json::json;
use std::collections::VecDeque;

fn profile() -> ContinuousProfile {
    ContinuousProfile::freeze(
        playtime_docker::transport::Selection {
            executable: "/synthetic/docker".into(),
            socket: "/synthetic/socket".into(),
            executable_sha256: "a".repeat(64),
        },
        playtime_docker::DaemonIdentity {
            id: "synthetic".into(),
            name: "synthetic".into(),
            cli_version: "28".into(),
            server_version: "28".into(),
            api_version: "1.51".into(),
            kernel: "synthetic".into(),
        },
        None,
    )
    .unwrap()
}

#[test]
fn native_preparation_rejects_changed_bounds_harness_tools_and_seeds() {
    let frozen = serde_json::to_value(profile()).unwrap();
    for (pointer, changed) in [
        ("/harness", json!("memento_turns_v1")),
        ("/config/settings/trigger_percent", json!(80)),
        ("/config/parameters/limits/wall_seconds", json!(301)),
        ("/seeds/continuous-probe.sh", json!("unreviewed script")),
        ("/work_responses/0/finish_reason", json!("length")),
    ] {
        let mut invalid = frozen.clone();
        *invalid.pointer_mut(pointer).unwrap() = changed;
        assert!(
            playtime_runner::docker::NativeProfile::decode(&serde_json::to_vec(&invalid).unwrap())
                .is_err(),
            "{pointer}"
        );
    }
    assert!(
        playtime_runner::docker::NativeProfile::decode(&serde_json::to_vec(&frozen).unwrap())
            .is_ok()
    );
}
struct Effects {
    work: VecDeque<CompletedResponse>,
    summaries: VecDeque<CompletedResponse>,
    queue: VecDeque<(u64, Input)>,
    active: Option<u64>,
    fault: u8,
}
impl playtime_runner::Effects for Effects {
    fn active(&self) -> Option<u64> {
        self.active
    }
    fn next_input_ms(&self) -> Option<u64> {
        self.queue.front().map(|p| p.0)
    }
    fn pop_input(&mut self) -> Option<(u64, Input)> {
        self.active = None;
        self.queue.pop_front()
    }
    fn cancel(&mut self) {
        self.queue.clear();
        self.active = None;
    }
    fn dispatch(&mut self, now: u64, effect: &Effect, _: &CommittedFrontier) -> Result<()> {
        assert!(self.active.is_none());
        self.active = Some(effect.id);
        let outcome = match &effect.kind {
            EffectKind::StartWorkspace => Outcome::WorkspaceStarted,
            EffectKind::Model(request) => {
                let mut reply = match request.purpose {
                    RequestPurpose::Work => self.work.pop_front().unwrap(),
                    RequestPurpose::Compaction => self.summaries.pop_front().unwrap(),
                    _ => panic!("unexpected handoff"),
                };
                if request.purpose == RequestPurpose::Compaction {
                    match self.fault {
                        1 => reply.finish_reason = "length".into(),
                        2 => {
                            return {
                                self.queue.push_back((
                                    now + 1,
                                    Input::Complete {
                                        effect_id: effect.id,
                                        outcome: Outcome::Model(reply.response),
                                    },
                                ));
                                Ok(())
                            };
                        }
                        3 => {
                            self.queue.push_back((now + 1, Input::Cancel));
                            return Ok(());
                        }
                        _ => {}
                    }
                }
                Outcome::ModelCompleted {
                    response: reply.response,
                    finish_reason: reply.finish_reason,
                }
            }
            EffectKind::Tool(_) => Outcome::Tool(
                json!({"exit_code":0,"stdout":"checked workspace evidence ".repeat(100),"stderr":"","timed_out":false}),
            ),
            EffectKind::CaptureWorkspace => Outcome::WorkspaceCaptured(WorkspaceSnapshot {
                workspace: content_ref(
                    ArchivePath::new("artifacts/final/workspace.json").unwrap(),
                    b"{}",
                ),
                files: Default::default(),
            }),
            EffectKind::PublishCapture(_) => Outcome::Published,
            EffectKind::Cleanup => Outcome::Cleaned(vec![]),
            _ => panic!("session effect in continuous profile"),
        };
        self.queue.push_back((
            now + 1,
            Input::Complete {
                effect_id: effect.id,
                outcome,
            },
        ));
        Ok(())
    }
    fn checkpoint<'a>(&'a self, _: &Checkpoint) -> Result<CheckpointBytes<'a>> {
        panic!("memento checkpoint")
    }
    fn capture<'a>(&'a self, _: &Capture) -> Result<CheckpointBytes<'a>> {
        Ok(CheckpointBytes::Contents {
            workspace: b"{}",
            files: Default::default(),
        })
    }
}
fn run_native(temp: &Temp, fault: u8, hooks: Box<dyn WriterHooks>) -> RunReport {
    let profile = profile();
    let prepared = prepare_continuous_with_inputs(
        &temp.archive(),
        RunId::new("continuous").unwrap(),
        &profile,
        capture_demo_worker().unwrap(),
        vec![],
    )
    .unwrap();
    assert!(PreparedExecution::acquire(temp.archive(), &prepared).is_err());
    PreparedExecution::acquire_docker(
        temp.archive(),
        &prepared,
        Box::new(Effects {
            work: profile.work_responses.into(),
            summaries: profile.summary_responses.into(),
            queue: VecDeque::new(),
            active: None,
            fault,
        }),
    )
    .unwrap()
    .execute(
        &mut clock(),
        DriverOptions {
            writer_hooks: hooks,
            ..Default::default()
        },
    )
    .unwrap()
}
#[test]
fn native_profile_uses_v2_compaction_evidence_and_final_capture() {
    let temp = Temp::new();
    let report = run_native(&temp, 0, Box::new(NoWriterHooks));
    assert!(report.terminal_committed, "{:?}", report.diagnostics);
    assert_eq!(
        report.result.as_ref().unwrap().reason,
        StopReason::AgentFinished
    );
    let queries = ArchiveQueries::offline(temp.archive(), StorageLimits::default()).unwrap();
    assert_eq!(queries.inputs().schema_version, 2);
    assert!(queries.checkpoints().unwrap().is_empty());
    assert_eq!(queries.captures().unwrap().len(), 1);
    let totals = queries.request_accounting().unwrap();
    assert_eq!(totals.by_purpose[&RequestPurpose::Work].admitted_calls, 49);
    assert!(totals.by_purpose[&RequestPurpose::Compaction].admitted_calls >= 2);
    let mut after = 0;
    let mut applied = 0;
    let mut completed = 0;
    loop {
        let page = queries.compaction_history(after, 7).unwrap();
        for entry in page.entries {
            let playtime_protocol::compaction::EvidenceEntry::Recorded { kind, value, .. } = entry
            else {
                panic!("unexpected omission");
            };
            if playtime_protocol::compaction::linked_kind(&kind) {
                let links: playtime_protocol::compaction::EvidenceLinks =
                    serde_json::from_value(value["evidence"].clone()).unwrap();
                assert!(links.requests.iter().all(|s| s.observation.is_some()));
                if kind == "compaction_applied" {
                    assert!(links.plan.is_some());
                    applied += 1;
                }
                if kind == "completion_declared" {
                    completed += 1;
                }
            }
        }
        if !page.has_more {
            break;
        }
        assert!(page.next_after_sequence > after);
        after = page.next_after_sequence;
    }
    assert_eq!(
        applied,
        totals.by_purpose[&RequestPurpose::Compaction].admitted_calls
    );
    assert_eq!(completed, 1);
}
struct LoseCapture;
impl WriterHooks for LoseCapture {
    fn after(&mut self, context: &CommitContext, _: &CommittedFrontier) -> ReceiptDelivery {
        if context.capture_effect.is_some() {
            ReceiptDelivery::Lose
        } else {
            ReceiptDelivery::Deliver
        }
    }
}
#[test]
fn failed_summaries_and_cancel_preserve_history_and_finalize_without_success() {
    for (fault, reason) in [
        (1, StopReason::CompactionFailed),
        (2, StopReason::CompactionFailed),
        (3, StopReason::Interrupted),
    ] {
        let temp = Temp::new();
        let report = run_native(&temp, fault, Box::new(NoWriterHooks));
        assert!(report.terminal_committed, "{:?}", report.diagnostics);
        let result = report.result.unwrap();
        assert_eq!(result.reason, reason);
        assert!(result.last_capture.is_some());
        let queries = ArchiveQueries::offline(temp.archive(), StorageLimits::default()).unwrap();
        let summary =
            &queries.request_accounting().unwrap().by_purpose[&RequestPurpose::Compaction];
        assert_eq!(summary.admitted_calls, 1);
        if fault == 3 {
            assert_eq!(summary.usage.unknown_requests, 1);
        }
        let mut after = 0;
        loop {
            let page = queries.compaction_history(after, 100).unwrap();
            assert!(page.entries.iter().all(|e| !matches!(e, playtime_protocol::compaction::EvidenceEntry::Recorded { kind, .. } if kind == "compaction_applied" || kind == "completion_declared")));
            if !page.has_more {
                break;
            }
            after = page.next_after_sequence;
        }
    }
    let temp = Temp::new();
    let report = run_native(&temp, 0, Box::new(LoseCapture));
    assert!(!report.terminal_committed);
    assert_eq!(
        report.result.as_ref().unwrap().reason,
        StopReason::InfrastructureFailure
    );
    assert!(report.result.unwrap().last_capture.is_none());
}
