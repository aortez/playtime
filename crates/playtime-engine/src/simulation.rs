//! Scripted adapters. Command strings are matched as data and never executed.

use std::collections::{BTreeMap, VecDeque};

use playtime_core::{
    ArchivePath, InvalidValue, RequestId, RunId, RunMode, RunParameters, SessionId, Sha256Digest,
};
use playtime_protocol::archive::ContentRef;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::*;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    pub schema_version: u32,
    pub mode: RunMode,
    pub parameters: RunParameters,
    pub protocol: String,
    pub prompt: String,
    pub inventory: String,
    #[serde(default)]
    pub initial_files: BTreeMap<String, String>,
    pub models: Vec<ModelScript>,
    #[serde(default)]
    pub execs: Vec<ExecScript>,
    #[serde(default)]
    pub cancel_at_ms: Option<u64>,
    #[serde(default)]
    pub lifecycle: Lifecycle,
    #[serde(default)]
    pub fail_record_sequence: Option<u64>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelScript {
    #[serde(default)]
    pub chunks: Vec<TimedChunk>,
    /// None models an inference operation that never completes.
    #[serde(default)]
    pub completion: Option<TimedCompletion>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TimedChunk {
    pub after_ms: u64,
    pub chunk: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TimedCompletion {
    pub after_ms: u64,
    pub outcome: ModelEnding,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum ModelEnding {
    Response(ModelResponse),
    Error(ModelFailure),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecScript {
    pub command: String,
    #[serde(default)]
    pub delay_ms: u64,
    #[serde(default)]
    pub never: bool,
    pub result: Value,
    /// Mutations occur at dispatch, allowing partial work before cancellation.
    #[serde(default)]
    pub workspace_changes: BTreeMap<String, Option<String>>,
    #[serde(default)]
    pub tmp_changes: BTreeMap<String, Option<String>>,
    #[serde(default)]
    pub processes: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Lifecycle {
    #[serde(default)]
    pub start: Vec<LifecycleStep>,
    #[serde(default)]
    pub snapshot: Vec<LifecycleStep>,
    #[serde(default)]
    pub publish: Vec<LifecycleStep>,
    #[serde(default)]
    pub cleanup: LifecycleStep,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LifecycleStep {
    #[serde(default)]
    pub delay_ms: u64,
    #[serde(default)]
    pub never: bool,
    #[serde(default)]
    pub failure: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SessionState {
    pub ordinal: u32,
    pub workspace: BTreeMap<String, String>,
    pub temporary_files: BTreeMap<String, String>,
    pub processes: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Dispatch {
    pub at_ms: u64,
    pub effect: Effect,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Simulation {
    pub schema_version: u32,
    pub mode: RunMode,
    pub result: RunResult,
    pub records: Vec<Record>,
    pub dispatches: Vec<Dispatch>,
    pub session_states: Vec<SessionState>,
    pub workspace: BTreeMap<String, String>,
    pub checkpoints: Vec<Checkpoint>,
    /// Exact synthetic snapshot bytes, not tar archives or executable workers.
    pub artifacts: BTreeMap<String, Vec<u8>>,
}

#[derive(Default)]
pub struct SequentialIds;

impl IdSource for SequentialIds {
    fn session(&mut self, ordinal: u32) -> SessionId {
        SessionId::new(format!("s{ordinal}")).unwrap()
    }
    fn request(&mut self, ordinal: u64) -> RequestId {
        RequestId::new(format!("r{ordinal}")).unwrap()
    }
}

fn digest(bytes: &[u8]) -> Sha256Digest {
    Sha256Digest::new(
        Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>(),
    )
    .unwrap()
}

fn index(files: &BTreeMap<String, String>) -> FileIndex {
    files
        .iter()
        .map(|(name, text)| {
            (
                name.clone(),
                FileInfo {
                    size_bytes: text.len() as u64,
                    mode: 0o644,
                    entry_type: "0".into(),
                    sha256: Some(digest(text.as_bytes())),
                    link: None,
                },
            )
        })
        .collect()
}

fn snapshot(
    path: &str,
    files: &BTreeMap<String, String>,
    artifacts: &mut BTreeMap<String, Vec<u8>>,
    limit: u64,
) -> Snapshot {
    let bytes = serde_json::to_vec(files).expect("JSON text workspace");
    let workspace = ContentRef {
        path: ArchivePath::new(path).unwrap(),
        sha256: digest(&bytes),
        size_bytes: bytes.len() as u64,
    };
    artifacts.insert(path.into(), bytes);
    Snapshot {
        workspace,
        files: index(files),
        memory: memory(files, limit),
    }
}

fn memory(files: &BTreeMap<String, String>, limit: u64) -> Memory {
    files
        .get("MEMENTO.md")
        .map_or_else(
            || Memory {
                status: "missing".into(),
                content: String::new(),
            },
            |text| Memory::loaded(text.clone()),
        )
        .bounded(limit)
}

fn path(name: &str) -> Result<String, InvalidValue> {
    let name = name.strip_prefix("/work/").unwrap_or(name);
    ArchivePath::new(name).map(|_| name.into())
}

fn validate_files(files: &BTreeMap<String, String>, limit: u64) -> Result<(), InvalidValue> {
    let mut bytes = 0_u64;
    for (name, text) in files {
        if path(name)? != *name {
            return Err(InvalidValue(
                "scenario file keys must be relative workspace names",
            ));
        }
        bytes = bytes
            .checked_add(text.len() as u64)
            .ok_or(InvalidValue("workspace size overflow"))?;
    }
    if bytes > limit {
        return Err(InvalidValue("simulated workspace byte limit exceeded"));
    }
    Ok(())
}

fn at(now: u64, delay: u64) -> Result<u64, InvalidValue> {
    now.checked_add(delay)
        .ok_or(InvalidValue("script time overflow"))
}

type Scheduled = VecDeque<(u64, Input)>;

fn scheduled(
    effect: &Effect,
    now: u64,
    step: LifecycleStep,
    outcome: Outcome,
) -> Result<Scheduled, InvalidValue> {
    if step.never {
        return Ok(VecDeque::new());
    }
    let outcome = step.failure.map_or(outcome, Outcome::Failed);
    Ok([(
        at(now, step.delay_ms)?,
        Input::Complete {
            effect_id: effect.id,
            outcome,
        },
    )]
    .into())
}

fn bounded_result(mut result: Value, maximum: u64) -> Value {
    let mut remaining = maximum as usize;
    for key in ["error", "stderr", "stdout", "content"] {
        if let Some(text) = result.get(key).and_then(Value::as_str) {
            let mut end = text.len().min(remaining);
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            let truncated = end < text.len();
            let clipped = text[..end].to_owned();
            remaining -= end;
            result[key] = json!(clipped);
            if truncated {
                result["output_truncated"] = json!(true);
            }
        }
    }
    result
}

struct Workshop {
    files: BTreeMap<String, String>,
    temporary: BTreeMap<String, String>,
    processes: Vec<String>,
    sessions: Vec<SessionState>,
    artifacts: BTreeMap<String, Vec<u8>>,
    execs: VecDeque<ExecScript>,
}

impl Workshop {
    fn tool(
        &mut self,
        request: &ToolRequest,
        parameters: &RunParameters,
    ) -> Result<(LifecycleStep, Value), InvalidValue> {
        let arguments = &request.arguments;
        let immediate = |value| Ok((LifecycleStep::default(), value));
        match request.name.as_str() {
            "exec" => {
                let script = self
                    .execs
                    .pop_front()
                    .ok_or(InvalidValue("no scripted result for exec"))?;
                if arguments["command"] != script.command {
                    return Err(InvalidValue("scripted exec command does not match request"));
                }
                let mut files = self.files.clone();
                let mut temporary = self.temporary.clone();
                for (changes, target) in [
                    (script.workspace_changes, &mut files),
                    (script.tmp_changes, &mut temporary),
                ] {
                    for (name, content) in changes {
                        let name = path(&name)?;
                        if let Some(content) = content {
                            target.insert(name, content);
                        } else {
                            target.remove(&name);
                        }
                    }
                }
                if validate_files(&files, parameters.workshop.workspace_bytes.get()).is_err()
                    || validate_files(&temporary, parameters.workshop.tmp_bytes.get()).is_err()
                    || self.processes.len() + script.processes.len()
                        > parameters.workshop.pids.get() as usize
                {
                    return immediate(
                        json!({"error": "scripted workshop resource limit exceeded"}),
                    );
                }
                self.files = files;
                self.temporary = temporary;
                self.processes.extend(script.processes);
                Ok((
                    LifecycleStep {
                        delay_ms: script.delay_ms,
                        never: script.never,
                        failure: None,
                    },
                    bounded_result(script.result, parameters.limits.output_bytes.get()),
                ))
            }
            "write_file" => {
                let name = match path(arguments["path"].as_str().unwrap()) {
                    Ok(name) => name,
                    Err(error) => return immediate(json!({"error": error.to_string()})),
                };
                let content = arguments["content"].as_str().unwrap();
                let mut files = self.files.clone();
                if arguments["append"] == true {
                    files.entry(name).or_default().push_str(content);
                } else {
                    files.insert(name, content.into());
                }
                if let Err(error) =
                    validate_files(&files, parameters.workshop.workspace_bytes.get())
                {
                    return immediate(json!({"error": error.to_string()}));
                }
                self.files = files;
                immediate(json!({"bytes_written": content.len(), "path": arguments["path"]}))
            }
            "read_file" => {
                let name = match path(arguments["path"].as_str().unwrap()) {
                    Ok(name) => name,
                    Err(error) => return immediate(json!({"error": error.to_string()})),
                };
                let Some(text) = self.files.get(&name) else {
                    return immediate(json!({"error": "missing file"}));
                };
                let offset = match arguments.get("offset_bytes") {
                    None => 0,
                    Some(value) => match value
                        .as_u64()
                        .and_then(|offset| usize::try_from(offset).ok())
                    {
                        Some(offset) => offset,
                        None => return immediate(json!({"error": "invalid byte offset"})),
                    },
                };
                let Some(text_slice) = text.get(offset..) else {
                    return immediate(json!({"error": "invalid UTF-8 byte offset"}));
                };
                immediate(bounded_result(
                    json!({"content": text_slice, "offset_bytes": offset, "size_bytes": text.len()}),
                    parameters.limits.output_bytes.get(),
                ))
            }
            _ => Err(InvalidValue(
                "controller dispatched an unsupported workshop tool",
            )),
        }
    }
}

/// Owns all simulated work, with an explicit cancellation boundary. Dispatch
/// mutates only in-memory files; command strings are never executed on the host.
/// A durable driver can commit intents before calling `dispatch`, then advance
/// the same controller incrementally using these scheduled inputs.
pub struct ScriptedAdapters {
    config: ControllerConfig,
    workshop: Workshop,
    models: VecDeque<ModelScript>,
    starts: VecDeque<LifecycleStep>,
    snapshots: VecDeque<LifecycleStep>,
    publications: VecDeque<LifecycleStep>,
    cleanup: LifecycleStep,
    active: Option<u64>,
    schedule: Scheduled,
}

impl ScriptedAdapters {
    pub fn new(scenario: &Scenario) -> Result<Self, InvalidValue> {
        if scenario.schema_version != 1 {
            return Err(InvalidValue("unsupported scenario version"));
        }
        if scenario.mode != RunMode::Simulation {
            return Err(InvalidValue(
                "scripted scenarios must be labeled simulation",
            ));
        }
        scenario.parameters.validate()?;
        validate_files(
            &scenario.initial_files,
            scenario.parameters.workshop.workspace_bytes.get(),
        )?;
        let mut workshop = Workshop {
            files: scenario.initial_files.clone(),
            temporary: BTreeMap::new(),
            processes: Vec::new(),
            sessions: Vec::new(),
            artifacts: BTreeMap::new(),
            execs: scenario.execs.clone().into(),
        };
        let initial = snapshot(
            "inputs/simulation-workspace.json",
            &workshop.files,
            &mut workshop.artifacts,
            scenario.parameters.limits.memento_bytes.get(),
        );
        let config = ControllerConfig {
            parameters: scenario.parameters.clone(),
            protocol: scenario.protocol.clone(),
            prompt: scenario.prompt.clone(),
            inventory: scenario.inventory.clone(),
            initial_workspace: initial.workspace,
            initial_files: initial.files,
        };

        Ok(Self {
            config,
            workshop,
            models: scenario.models.clone().into(),
            starts: scenario.lifecycle.start.clone().into(),
            snapshots: scenario.lifecycle.snapshot.clone().into(),
            publications: scenario.lifecycle.publish.clone().into(),
            cleanup: scenario.lifecycle.cleanup.clone(),
            active: None,
            schedule: Scheduled::new(),
        })
    }

    pub fn config(&self) -> &ControllerConfig {
        &self.config
    }
    pub fn artifacts(&self) -> &BTreeMap<String, Vec<u8>> {
        &self.workshop.artifacts
    }
    pub fn files(&self) -> &BTreeMap<String, String> {
        &self.workshop.files
    }
    pub fn session_states(&self) -> &[SessionState] {
        &self.workshop.sessions
    }
    pub fn active(&self) -> Option<u64> {
        self.active
    }
    pub fn next_input_ms(&self) -> Option<u64> {
        self.schedule.front().map(|(time, _)| *time)
    }

    pub fn pop_input(&mut self) -> Option<(u64, Input)> {
        let input = self.schedule.pop_front()?;
        if matches!(input.1, Input::Complete { .. }) {
            self.active = None;
        }
        Some(input)
    }

    /// Cancellation is synchronous: there are no worker threads or remote calls
    /// behind a scripted effect. Workspace mutations already made remain visible.
    pub fn cancel(&mut self) {
        self.schedule.clear();
        self.active = None;
    }

    pub fn dispatch(&mut self, now: u64, effect: &Effect) -> Result<(), InvalidValue> {
        if self.active.is_some() {
            return Err(InvalidValue(
                "cancel or complete the previous scripted effect first",
            ));
        }
        self.schedule = match &effect.kind {
            EffectKind::StartWorkspace
            | EffectKind::CaptureWorkspace
            | EffectKind::PublishCapture(_) => {
                return Err(InvalidValue(
                    "session-neutral effects require the explicit test profile",
                ));
            }
            EffectKind::Model(_) => {
                let script = self.models.pop_front().unwrap_or(ModelScript {
                    chunks: Vec::new(),
                    completion: Some(TimedCompletion {
                        after_ms: 0,
                        outcome: ModelEnding::Error(ModelFailure::Infrastructure(
                            "model script exhausted".into(),
                        )),
                    }),
                });
                let mut scheduled = Scheduled::new();
                let mut time = now;
                for chunk in script.chunks {
                    time = at(time, chunk.after_ms)?;
                    scheduled.push_back((
                        time,
                        Input::Chunk {
                            effect_id: effect.id,
                            chunk: chunk.chunk,
                        },
                    ));
                }
                if let Some(completion) = script.completion {
                    time = at(time, completion.after_ms)?;
                    let outcome = match completion.outcome {
                        ModelEnding::Response(response) => Outcome::Model(response),
                        ModelEnding::Error(error) => Outcome::ModelError(error),
                    };
                    scheduled.push_back((
                        time,
                        Input::Complete {
                            effect_id: effect.id,
                            outcome,
                        },
                    ));
                }
                scheduled
            }
            EffectKind::StartSession { ordinal, .. } => {
                self.workshop.temporary.clear();
                self.workshop.processes.clear();
                self.workshop.sessions.push(SessionState {
                    ordinal: *ordinal,
                    workspace: self.workshop.files.clone(),
                    temporary_files: self.workshop.temporary.clone(),
                    processes: self.workshop.processes.clone(),
                });
                scheduled(
                    effect,
                    now,
                    self.starts.pop_front().unwrap_or_default(),
                    Outcome::Started(memory(
                        &self.workshop.files,
                        self.config.parameters.limits.memento_bytes.get(),
                    )),
                )?
            }
            EffectKind::Tool(request) => match self.workshop.tool(request, &self.config.parameters)
            {
                Ok((step, result)) => scheduled(effect, now, step, Outcome::Tool(result))?,
                Err(error) => scheduled(
                    effect,
                    now,
                    LifecycleStep::default(),
                    Outcome::Failed(error.to_string()),
                )?,
            },
            EffectKind::Snapshot { session_id } => {
                self.workshop.temporary.clear();
                self.workshop.processes.clear();
                let snapshot = snapshot(
                    &format!("sessions/{session_id}/simulation-workspace.json"),
                    &self.workshop.files,
                    &mut self.workshop.artifacts,
                    self.config.parameters.limits.memento_bytes.get(),
                );
                scheduled(
                    effect,
                    now,
                    self.snapshots.pop_front().unwrap_or_default(),
                    Outcome::Snapshot(snapshot),
                )?
            }
            EffectKind::PublishCheckpoint(_) => scheduled(
                effect,
                now,
                self.publications.pop_front().unwrap_or_default(),
                Outcome::Published,
            )?,
            EffectKind::Cleanup => scheduled(
                effect,
                now,
                self.cleanup.clone(),
                Outcome::Cleaned(Vec::new()),
            )?,
        };
        self.active = Some(effect.id);
        Ok(())
    }
}

pub fn run(scenario: Scenario) -> Result<Simulation, InvalidValue> {
    let mut adapters = ScriptedAdapters::new(&scenario)?;
    let mut controller = Controller::new(
        adapters.config().clone(),
        RunId::new("simulation").unwrap(),
        RunMode::Simulation,
        SequentialIds,
        0,
    )?;
    let mut cancel = scenario.cancel_at_ms;
    let mut fail_record = scenario.fail_record_sequence;
    let mut records = Vec::new();
    let mut dispatches = Vec::new();
    let mut checkpoints = Vec::new();
    let mut now = 0;
    let mut dispatched = None;
    loop {
        controller.take_control_updates();
        let new_records = controller.take_records();
        if let Some(index) = new_records
            .iter()
            .position(|record| Some(record.sequence) == fail_record)
        {
            records.extend(new_records[..index].iter().cloned());
            fail_record = None;
            controller.advance(
                now,
                Input::PersistenceFailed("scripted event persistence failure".into()),
            )?;
            continue;
        }
        records.extend(new_records);
        if let Some(result) = controller.result() {
            return Ok(Simulation {
                schema_version: 1,
                mode: RunMode::Simulation,
                result: result.clone(),
                records,
                dispatches,
                session_states: adapters.workshop.sessions,
                workspace: adapters.workshop.files,
                checkpoints,
                artifacts: adapters.workshop.artifacts,
            });
        }
        if cancel.is_some_and(|time| time <= now) {
            cancel = None;
            controller.advance(now, Input::Cancel)?;
            continue;
        }
        let effect = controller
            .pending()
            .expect("controller yields an effect or result")
            .clone();
        if dispatched != Some(effect.id) {
            controller.advance(
                now,
                Input::Dispatched {
                    effect_id: effect.id,
                },
            )?;
            if controller
                .pending()
                .is_none_or(|pending| pending.id != effect.id)
            {
                continue;
            }
            dispatched = Some(effect.id);
            dispatches.push(Dispatch {
                at_ms: now,
                effect: effect.clone(),
            });
            // Abandon the previous schedule when the controller replaces an
            // effect. A real driver must also cancel/join its external work.
            adapters.cancel();
            adapters.dispatch(now, &effect)?;
        }
        let wakeup = controller
            .next_wakeup_ms()
            .expect("pending effect has a timer");
        let next = adapters.next_input_ms().unwrap_or(wakeup).min(wakeup);
        if cancel.is_some_and(|time| time <= next) {
            now = cancel.take().unwrap();
            controller.advance(now, Input::Cancel)?;
        } else if wakeup <= next {
            now = wakeup;
            controller.advance(now, Input::Tick)?;
        } else {
            let (time, input) = adapters.pop_input().unwrap();
            now = time;
            let published = matches!(
                input,
                Input::Complete {
                    outcome: Outcome::Published,
                    ..
                }
            );
            controller.advance(now, input)?;
            if published && let EffectKind::PublishCheckpoint(checkpoint) = effect.kind {
                checkpoints.push(*checkpoint);
            }
        }
    }
}
