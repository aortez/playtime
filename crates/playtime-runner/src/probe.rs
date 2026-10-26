//! Feature-gated CPU contract probe, never a catalog harness or worker profile.
//! Commands are matched as data; this adapter cannot execute host command text.
use crate::{
    CheckpointBytes, Effects, Error, PreparedSimulation, Result, capture_demo_worker,
    prepare::{freeze_archive, reference},
};
use playtime_core::{CommonParameters, RunId, RunMode};
use playtime_engine::{
    Capture, Checkpoint, Effect, EffectKind, FileIndex, FileInfo, Input, Outcome, WorkspaceConfig,
    WorkspaceSnapshot,
    simulation::{ExecScript, LifecycleStep, ModelEnding, ModelScript},
};
use playtime_storage::{CommittedFrontier, StorageLimits, sha256};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, VecDeque},
    path::Path,
};

pub const PROFILE: &str = "test-workspace-v2";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    pub schema_version: u32,
    pub profile: String,
    pub parameters: CommonParameters,
    pub protocol: String,
    pub prompt: String,
    pub context_reserve: u64,
    pub initial_files: BTreeMap<String, String>,
    pub models: Vec<ModelScript>,
    pub execs: Vec<ExecScript>,
    #[serde(default)]
    pub cancel_at_ms: Option<u64>,
    #[serde(default)]
    pub fail_record_sequence: Option<u64>,
    #[serde(default)]
    pub start: LifecycleStep,
    #[serde(default)]
    pub capture: LifecycleStep,
    #[serde(default)]
    pub publish: LifecycleStep,
    #[serde(default)]
    pub cleanup: LifecycleStep,
}

impl Scenario {
    pub(crate) fn decode(bytes: &[u8]) -> Result<Self> {
        let value: serde_json::Value = serde_json::from_slice(bytes)?;
        if value["schema_version"] != 2 || value["profile"] != PROFILE {
            return Err(Error::Invalid(
                "unsupported test execution contract version/profile".into(),
            ));
        }
        let scenario: Self = serde_json::from_value(value)?;
        scenario.parameters.validate()?;
        if scenario.context_reserve >= scenario.parameters.limits.context_tokens.get() {
            return Err(Error::Invalid("test context reserve exceeds window".into()));
        }
        Ok(scenario)
    }
    pub(crate) fn config(&self) -> WorkspaceConfig {
        let bytes = serde_json::to_vec(&self.initial_files).unwrap();
        WorkspaceConfig {
            parameters: self.parameters.clone(),
            protocol: self.protocol.clone(),
            prompt: self.prompt.clone(),
            context_reserve: self.context_reserve,
            initial_workspace: reference("inputs/workspace.json", &bytes),
            initial_files: index(&self.initial_files),
        }
    }
}

/// Freeze only the actual executing test image. No supplied production package
/// can be labeled as supporting this test profile.
pub fn prepare(path: &Path, run_id: RunId, scenario: &Scenario) -> Result<PreparedSimulation> {
    let bytes = serde_json::to_vec(scenario)?;
    let verified = Scenario::decode(&bytes)?;
    let config = verified.config();
    let limits = StorageLimits {
        event_log_bytes: config.parameters.limits.log_bytes.get(),
        ..StorageLimits::default()
    };
    freeze_archive(
        path,
        run_id,
        &bytes,
        capture_demo_worker()?,
        Vec::new(),
        false,
        None,
        RunMode::Simulation,
        config.initial_workspace,
        serde_json::to_vec(&verified.initial_files)?,
        limits,
    )
}

fn index(files: &BTreeMap<String, String>) -> FileIndex {
    files
        .iter()
        .map(|(path, text)| {
            (
                path.clone(),
                FileInfo {
                    size_bytes: text.len() as u64,
                    mode: 0o644,
                    entry_type: "file".into(),
                    sha256: Some(sha256(text.as_bytes())),
                    link: None,
                },
            )
        })
        .collect()
}

pub(crate) struct Adapter {
    scenario: Scenario,
    files: BTreeMap<String, String>,
    models: VecDeque<ModelScript>,
    execs: VecDeque<ExecScript>,
    queue: VecDeque<(u64, Input)>,
    active: Option<u64>,
    captured: Option<(WorkspaceSnapshot, Vec<u8>)>,
}
impl Adapter {
    pub fn new(scenario: Scenario) -> Self {
        Self {
            files: scenario.initial_files.clone(),
            models: scenario.models.clone().into(),
            execs: scenario.execs.clone().into(),
            scenario,
            queue: VecDeque::new(),
            active: None,
            captured: None,
        }
    }
    fn complete(
        &mut self,
        now: u64,
        effect: &Effect,
        step: LifecycleStep,
        outcome: Outcome,
    ) -> Result<()> {
        if !step.never {
            let time = now
                .checked_add(step.delay_ms)
                .ok_or_else(|| Error::Invalid("script clock overflow".into()))?;
            self.queue.push_back((
                time,
                Input::Complete {
                    effect_id: effect.id,
                    outcome: step.failure.map_or(outcome, Outcome::Failed),
                },
            ));
        }
        Ok(())
    }
}
impl Effects for Adapter {
    fn active(&self) -> Option<u64> {
        self.active
    }
    fn next_input_ms(&self) -> Option<u64> {
        self.queue.front().map(|(at, _)| *at)
    }
    fn pop_input(&mut self) -> Option<(u64, Input)> {
        let item = self.queue.pop_front()?;
        if matches!(item.1, Input::Complete { .. }) {
            self.active = None;
        }
        Some(item)
    }
    fn cancel(&mut self) {
        self.queue.clear();
        self.active = None;
    }
    fn dispatch(&mut self, now: u64, effect: &Effect, _: &CommittedFrontier) -> Result<()> {
        if self.active.is_some() {
            return Err(Error::Invalid("overlapping probe effects".into()));
        }
        self.active = Some(effect.id);
        match &effect.kind {
            EffectKind::StartWorkspace => self.complete(
                now,
                effect,
                self.scenario.start.clone(),
                Outcome::WorkspaceStarted,
            ),
            EffectKind::Model(_) => {
                let script = self
                    .models
                    .pop_front()
                    .ok_or_else(|| Error::Invalid("probe model script exhausted".into()))?;
                let mut at = now;
                for chunk in script.chunks {
                    at = at
                        .checked_add(chunk.after_ms)
                        .ok_or_else(|| Error::Invalid("script clock overflow".into()))?;
                    self.queue.push_back((
                        at,
                        Input::Chunk {
                            effect_id: effect.id,
                            chunk: chunk.chunk,
                        },
                    ));
                }
                if let Some(completion) = script.completion {
                    self.complete(
                        at,
                        effect,
                        LifecycleStep {
                            delay_ms: completion.after_ms,
                            ..LifecycleStep::default()
                        },
                        match completion.outcome {
                            ModelEnding::Response(response) => Outcome::Model(response),
                            ModelEnding::Error(error) => Outcome::ModelError(error),
                        },
                    )?;
                }
                Ok(())
            }
            EffectKind::Tool(request) => {
                let script = self
                    .execs
                    .pop_front()
                    .ok_or_else(|| Error::Invalid("probe tool script exhausted".into()))?;
                if request.name != "exec"
                    || request.arguments["command"].as_str() != Some(&script.command)
                {
                    return Err(Error::Invalid(
                        "probe command differs from exact scripted text".into(),
                    ));
                }
                for (path, text) in script.workspace_changes {
                    if let Some(text) = text {
                        self.files.insert(path, text);
                    } else {
                        self.files.remove(&path);
                    }
                }
                self.complete(
                    now,
                    effect,
                    LifecycleStep {
                        delay_ms: script.delay_ms,
                        never: script.never,
                        failure: None,
                    },
                    Outcome::Tool(script.result),
                )
            }
            EffectKind::CaptureWorkspace => {
                let bytes = serde_json::to_vec(&self.files)?;
                let snapshot = WorkspaceSnapshot {
                    workspace: reference("artifacts/final/workspace.json", &bytes),
                    files: index(&self.files),
                };
                self.captured = Some((snapshot.clone(), bytes));
                self.complete(
                    now,
                    effect,
                    self.scenario.capture.clone(),
                    Outcome::WorkspaceCaptured(snapshot),
                )
            }
            EffectKind::PublishCapture(_) => self.complete(
                now,
                effect,
                self.scenario.publish.clone(),
                Outcome::Published,
            ),
            EffectKind::Cleanup => self.complete(
                now,
                effect,
                self.scenario.cleanup.clone(),
                Outcome::Cleaned(Vec::new()),
            ),
            _ => Err(Error::Invalid(
                "probe cannot fabricate session/memento effects".into(),
            )),
        }
    }
    fn checkpoint<'a>(&'a self, _: &Checkpoint) -> Result<CheckpointBytes<'a>> {
        Err(Error::Invalid("probe has no memento checkpoint".into()))
    }
    fn capture<'a>(&'a self, capture: &Capture) -> Result<CheckpointBytes<'a>> {
        let (snapshot, bytes) = self
            .captured
            .as_ref()
            .ok_or_else(|| Error::Invalid("no stopped capture".into()))?;
        if snapshot != &capture.snapshot {
            return Err(Error::Invalid("capture identity changed".into()));
        }
        Ok(CheckpointBytes::Contents {
            workspace: bytes,
            files: self
                .files
                .iter()
                .map(|(path, text)| (path.as_str(), text.as_bytes()))
                .collect(),
        })
    }
}
