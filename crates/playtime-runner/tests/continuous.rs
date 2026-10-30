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
/// A saved definition that selected the continuous harness.
fn saved_continuous_definition() -> playtime_runner::docker::FrozenRecipe {
    use playtime_protocol::api::{CONTINUOUS_COMPACTION_V1, HarnessId};
    use playtime_runner::docker::{FrozenRecipe, Recipe};
    let scenario: playtime_engine::simulation::Scenario =
        serde_json::from_slice(playtime_runner::docker::LIVE_SCENARIO).unwrap();
    let mut definition = FrozenRecipe::current(Recipe {
        id: "continuous-saved".into(),
        name: "Continuous saved".into(),
        prompt: scenario.prompt,
        protocol: scenario.protocol,
        inventory: scenario.inventory,
        parameters: scenario.parameters,
        workspace_entries: 512,
        model: playtime_runner::docker::Sampling {
            thinking: "high".into(),
            temperature: 0.8,
            top_p: 0.95,
            top_k: 40,
            repeat_penalty: 1.0,
        },
        seeds: Default::default(),
    })
    .unwrap();
    definition
        .select_harness(&HarnessId::new(CONTINUOUS_COMPACTION_V1).unwrap())
        .unwrap();
    // Reference allocations remain unchanged; the summary cap fits its response.
    definition
        .compaction
        .as_mut()
        .unwrap()
        .settings
        .summary_tokens = 128;
    definition
}
fn synthetic_host() -> playtime_runner::docker::LiveHost {
    playtime_runner::docker::LiveHost {
        target: None,
        endpoint: "http://127.0.0.1:11434".into(),
        addresses: vec!["127.0.0.1".parse().unwrap()],
        live_lock: "/tmp/synthetic-model.lock".into(),
    }
}
#[test]
fn saved_continuous_definition_selects_only_its_exact_native_profile() {
    use playtime_protocol::api::HarnessView;
    use playtime_runner::docker::{NativeProfile, Profile};
    let definition = saved_continuous_definition();
    let base = profile();
    let host = synthetic_host();
    assert!(
        Profile::freeze_definition(
            definition.clone(),
            base.selection.clone(),
            base.daemon.clone(),
            host.clone()
        )
        .is_err()
    );
    let profile =
        NativeProfile::freeze_definition(definition.clone(), base.selection, base.daemon, host)
            .unwrap();
    let NativeProfile::Continuous(profile) = profile else {
        panic!("wrong policy")
    };
    profile.validate().unwrap();
    // The frozen inventory states this recipe's own workshop capacity.
    assert_eq!(
        profile.config.inventory,
        definition.compaction.as_ref().unwrap().inventory
    );
    assert!(profile.config.inventory.contains(
        "\nYour workspace /work holds 8 MiB and 512 files and directories; /tmp holds 16 MiB. Both last until the run ends.\n"
    ));
    // Then its command limits: the bridge's cap, which leaves 3 of the 5 seconds
    // to return, and the result allowance that one response's calls share.
    assert!(profile.config.inventory.ends_with(
        "\nA command still running after 2 seconds is stopped, along with any background jobs it started. The tool results of one response share 4 KiB; longer output is shortened. For longer work, start a background job with its output in a file, such as `python3 job.py > job.log 2>&1 &`, and check the file in later calls.\n"
    ));
    let mut curiosity = definition.recipe.clone();
    curiosity.parameters.workshop.workspace_bytes = (16 << 20).try_into().unwrap();
    curiosity.parameters.workshop.tmp_bytes = (1 << 30).try_into().unwrap();
    curiosity.workspace_entries = 900;
    curiosity.parameters.limits.command_seconds = 60.try_into().unwrap();
    curiosity.parameters.limits.output_bytes = (16 << 10).try_into().unwrap();
    assert!(
        playtime_runner::docker::CompactionDefinition::new(&curiosity)
            .inventory
            .ends_with("\nYour workspace /work holds 16 MiB and 900 files and directories; /tmp holds 1 GiB. Both last until the run ends.\nA command still running after 57 seconds is stopped, along with any background jobs it started. The tool results of one response share 16 KiB; longer output is shortened. For longer work, start a background job with its output in a file, such as `python3 job.py > job.log 2>&1 &`, and check the file in later calls.\n")
    );
    curiosity.parameters.limits.command_seconds = 303.try_into().unwrap();
    assert!(
        playtime_runner::docker::CompactionDefinition::new(&curiosity)
            .inventory
            .contains("\nA command still running after 5 minutes is stopped")
    );
    curiosity.parameters.workshop.workspace_bytes = (3 << 19).try_into().unwrap();
    assert!(
        playtime_runner::docker::CompactionDefinition::new(&curiosity)
            .inventory
            .contains("/work holds 1536 KiB and 900 files")
    );
    // The frozen protocol states this recipe's own run limits after the shared text.
    assert_eq!(
        profile.config.protocol,
        definition.compaction.as_ref().unwrap().protocol
    );
    assert!(
        profile
            .config
            .protocol
            .starts_with(include_str!("../src/docker/continuous-protocol-v1.txt"))
    );
    let mut protocol = |wall: u64, tokens: u64| {
        curiosity.parameters.limits.wall_seconds = wall.try_into().unwrap();
        curiosity.parameters.limits.generated_tokens = tokens.try_into().unwrap();
        playtime_runner::docker::CompactionDefinition::new(&curiosity).protocol
    };
    assert!(protocol(1800, 300_000).ends_with(
        "\nThis run lasts at most 30 minutes or 300,000 generated tokens, whichever comes first. Calling finish_run ends it early; unused time and tokens are not carried over.\n"
    ));
    for (wall, tokens, limits) in [
        (600, 100_000, "10 minutes or 100,000 generated"),
        (60, 999, "1 minute or 999 generated"),
        (7200, 1_000_000, "2 hours or 1,000,000 generated"),
        (90, 1_500, "90 seconds or 1,500 generated"),
        (1, 12_345, "1 second or 12,345 generated"),
    ] {
        assert!(protocol(wall, tokens).contains(&format!("at most {limits} tokens")));
    }
    assert_eq!(
        profile.config.parameters,
        definition.recipe.parameters.common()
    );
    assert_eq!(profile.config.prompt, definition.recipe.prompt);
    assert_eq!(profile.seeds, definition.recipe.seeds);
    assert_eq!(
        profile.config.protocol,
        definition.compaction.as_ref().unwrap().protocol
    );
    let frozen = serde_json::to_value(&profile).unwrap();
    assert_eq!(
        HarnessView::from_configuration(&frozen).unwrap().unwrap(),
        definition.harness_view().unwrap().harness
    );
    for (pointer, changed) in [
        ("/config/settings/trigger_percent", json!(80)),
        ("/config/parameters/limits/wall_seconds", json!(301)),
        ("/config/protocol", json!("different instructions")),
        ("/selection/executable", json!("relative/docker")),
    ] {
        let mut invalid = frozen.clone();
        *invalid.pointer_mut(pointer).unwrap() = changed;
        assert!(
            NativeProfile::decode(&serde_json::to_vec(&invalid).unwrap()).is_err(),
            "{pointer}"
        );
    }
}
#[test]
fn leaving_out_finish_run_rewrites_the_protocol_and_freezes_the_choice() {
    use playtime_protocol::api::{CONTINUOUS_COMPACTION_V1, HarnessView};
    use playtime_runner::docker::{CompactionDefinition, NativeProfile};
    let mut definition = saved_continuous_definition();
    let mut settings = definition.compaction.as_ref().unwrap().settings.clone();
    settings.allow_finish_run = false;
    definition.apply_compaction_settings(&settings).unwrap();
    let protocol = definition.compaction.as_ref().unwrap().protocol.clone();
    assert_eq!(
        protocol,
        CompactionDefinition::protocol(&definition.recipe, false)
    );
    assert!(protocol.starts_with(include_str!(
        "../src/docker/continuous-protocol-no-finish-v1.txt"
    )));
    assert!(!protocol.contains("finish_run"));
    let mut curiosity = definition.recipe.clone();
    curiosity.parameters.limits.wall_seconds = 600.try_into().unwrap();
    curiosity.parameters.limits.generated_tokens = 100_000.try_into().unwrap();
    assert!(CompactionDefinition::protocol(&curiosity, false).ends_with(
        "\nThis run lasts 10 minutes or 100,000 generated tokens, whichever comes first.\n"
    ));
    // The frozen profile carries the choice to the controller.
    let base = profile();
    let NativeProfile::Continuous(frozen) = NativeProfile::freeze_definition(
        definition.clone(),
        base.selection,
        base.daemon,
        synthetic_host(),
    )
    .unwrap() else {
        panic!("wrong policy")
    };
    frozen.validate().unwrap();
    assert!(!frozen.config.settings.allow_finish_run);
    assert_eq!(frozen.config.protocol, protocol);
    let value = serde_json::to_value(&frozen).unwrap();
    assert_eq!(
        value.pointer("/config/settings/allow_finish_run"),
        Some(&json!(false))
    );
    assert_eq!(
        HarnessView::from_configuration(&value).unwrap().unwrap(),
        definition.harness_view().unwrap().harness
    );
    // Restoring finish_run restores its protocol. Other setting changes keep
    // whatever protocol the definition already froze.
    settings.allow_finish_run = true;
    definition.apply_compaction_settings(&settings).unwrap();
    assert_eq!(
        definition.compaction.as_ref().unwrap().protocol,
        CompactionDefinition::protocol(&definition.recipe, true)
    );
    definition.compaction.as_mut().unwrap().protocol = "Frozen earlier.".into();
    settings.trigger_percent = 60;
    definition.apply_compaction_settings(&settings).unwrap();
    assert_eq!(
        definition.compaction.as_ref().unwrap().protocol,
        "Frozen earlier."
    );
    // Settings saved before the choice existed still mean finish_run is offered,
    // and the default is omitted so their bytes and digests are unchanged.
    let old: playtime_protocol::api::CompactionSettings =
        serde_json::from_value(json!({"schema_version":1,"trigger_percent":75,
            "recent_exchanges":2,"summary_tokens":2048,"instructions":"Summarize."}))
        .unwrap();
    assert!(old.allow_finish_run);
    assert!(
        serde_json::to_value(&old)
            .unwrap()
            .get("allow_finish_run")
            .is_none()
    );
    let catalog = playtime_runner::docker::harness_catalog().unwrap();
    let continuous = catalog
        .harnesses
        .iter()
        .find(|h| h.id.as_str() == CONTINUOUS_COMPACTION_V1);
    assert!(
        continuous
            .unwrap()
            .settings
            .contains(&"allow_finish_run".to_owned())
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
