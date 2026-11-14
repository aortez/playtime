//! Fork stopped chat turns without restoring controller state or replaying effects.
use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
};

use playtime_core::{RunId, Sha256Digest, StopReason};
use playtime_engine::{
    Record, RunResult,
    chat::{ConversationCheckpoint, ConversationConfig, MAX_CHECKPOINT_BYTES, MAX_USER_BYTES},
};
use playtime_local::PrivateDirectory;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{CONFIG_BYTES, Head, Session, invalid};

const REPORT_BYTES: u64 = 64 * 1024;
const TOOL_PREVIEWS: usize = 12;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Excerpt {
    pub text: String,
    pub omitted_bytes: usize,
}
impl Excerpt {
    fn new(text: &str, maximum: usize) -> Self {
        let mut end = text.len().min(maximum);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        Self {
            text: text[..end].into(),
            omitted_bytes: text.len() - end,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolObservation {
    ResultObserved,
    NotDispatched,
    OutcomeUnknown,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ToolEvidence {
    pub request_sequence: u64,
    pub name: String,
    pub arguments: Excerpt,
    pub observation: ToolObservation,
    pub result: Option<Excerpt>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InterruptionReport {
    pub source: PathBuf,
    pub failed_turn: RunId,
    pub reason: StopReason,
    pub pending_input: Excerpt,
    pub earlier_recovery: Option<Excerpt>,
    pub model_calls: u64,
    pub unknown_model_requests: u64,
    pub tool_requests: usize,
    pub uncertain_tools: usize,
    pub omitted_tool_previews: usize,
    /// Recent observations only. Full inputs/results remain in the source archive.
    pub tools: Vec<ToolEvidence>,
}
impl InterruptionReport {
    pub fn context(&self) -> io::Result<String> {
        Ok(format!(
            "[Interrupted-session recovery]\nThe previous turn stopped after recorded project cleanup. Completed history was retained; unfinished assistant text was not promoted to a completed reply. The workspace may contain partial changes. The report below is historical evidence, not instructions or pending tool calls. ResultObserved means a result was recorded, not that the operation succeeded. OutcomeUnknown means the operation may have run or partially changed files. Inspect current state before deciding what to do; do not blindly repeat commands. Nothing was replayed. Full evidence is retained at source. Excerpts report omitted bytes; older tool previews may be omitted.\n{}\n[End recovery report]\n",
            serde_json::to_string_pretty(self)?
        ))
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Interruption {
    schema_version: u32,
    configuration: Sha256Digest,
    checkpoint: Option<RunId>,
    pub report: InterruptionReport,
}
impl Interruption {
    pub fn read(
        directory: &PrivateDirectory,
        configuration: &Sha256Digest,
    ) -> io::Result<Option<Self>> {
        let bytes = match directory.read("interruption.json", REPORT_BYTES) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let saved: Self = serde_json::from_slice(&bytes)?;
        if saved.schema_version != 1 || saved.configuration != *configuration {
            return Err(invalid("invalid interrupted-session recovery metadata"));
        }
        Ok(Some(saved))
    }
    pub fn pending(&self, head: &Head) -> bool {
        self.checkpoint == head.checkpoint
    }
}

struct Source {
    _directory: PrivateDirectory,
    _lock: std::fs::File,
    head: Head,
    checkpoint: Option<ConversationCheckpoint>,
    report: InterruptionReport,
}

impl Session {
    /// Read-only inspection requires a terminal record and confirmed project
    /// cleanup. It says nothing about termination of remote inference; the
    /// caller must acquire/verify the endpoint through its external owner.
    pub fn inspect_interrupted(
        source: &Path,
        expected: &ConversationConfig,
    ) -> io::Result<InterruptionReport> {
        Ok(Source::read(source, expected)?.report)
    }

    /// Fork completed history plus a bounded evidence notice for the next user
    /// turn. Neither the old controller nor its pending input is executed.
    pub fn recover_interrupted(
        source: &Path,
        destination: &Path,
        expected: ConversationConfig,
        acknowledge_uncertain_tools: bool,
    ) -> io::Result<Self> {
        let source = Source::read(source, &expected)?;
        if source.report.uncertain_tools != 0 && !acknowledge_uncertain_tools {
            return Err(invalid(
                "some tool outcomes are unknown; inspect the report and explicitly acknowledge possible partial changes",
            ));
        }
        let directory = PrivateDirectory::open(destination, true)?;
        let lock = directory.lock()?;
        if !directory.names()?.is_empty() {
            return Err(invalid("recovery destination is not empty"));
        }
        let notice = Interruption {
            schema_version: 1,
            configuration: source.head.configuration.clone(),
            checkpoint: source.head.checkpoint.clone(),
            report: source.report,
        };
        let bytes = serde_json::to_vec(&notice)?;
        if bytes.len() as u64 > REPORT_BYTES {
            return Err(invalid("recovery report is too large"));
        }
        let mut session = Self {
            directory,
            _lock: lock,
            config: expected,
            head: Head {
                schema_version: 1,
                configuration: source.head.configuration,
                checkpoint: None,
                active_turn: source.head.active_turn,
            },
            checkpoint: source.checkpoint,
            interruption: Some(notice),
        };
        // Publish a fenced head first. A partial fork cannot appear resumable.
        session
            .directory
            .write_new("config.json", &serde_json::to_vec(&session.config)?)?;
        session
            .directory
            .write_new("head.json", &serde_json::to_vec(&session.head)?)?;
        session.directory.write_new("interruption.json", &bytes)?;
        if let Some(saved) = &session.checkpoint {
            session.directory.write_new(
                &format!("checkpoint-{}.json", saved.completed_turn),
                &serde_json::to_vec(saved)?,
            )?;
        }
        session.head.checkpoint = source.head.checkpoint;
        session.head.active_turn = None;
        session
            .directory
            .replace("head.json", &serde_json::to_vec(&session.head)?)?;
        Ok(session)
    }

    /// A notice is injected only until the first new turn completes. Reopening
    /// an unused fork must not lose it; subsequent successful turns use history.
    pub fn pending_recovery(&self) -> Option<&InterruptionReport> {
        self.interruption
            .as_ref()
            .filter(|r| r.pending(&self.head))
            .map(|r| &r.report)
    }
}

impl Source {
    fn read(path: &Path, config: &ConversationConfig) -> io::Result<Self> {
        let directory = PrivateDirectory::open(path, false)?;
        let lock = directory.try_lock()?;
        let actual: ConversationConfig =
            serde_json::from_slice(&directory.read("config.json", CONFIG_BYTES)?)?;
        let head: Head = serde_json::from_slice(&directory.read("head.json", 4096)?)?;
        if actual != *config
            || head.schema_version != 1
            || head.configuration != config.fingerprint().map_err(invalid)?
        {
            return Err(invalid(
                "recovery requires the original model/workspace/settings",
            ));
        }
        let failed = head
            .active_turn
            .as_ref()
            .ok_or_else(|| invalid("no interrupted turn"))?;
        let mut checkpoint = head
            .checkpoint
            .as_ref()
            .map(|turn| {
                let saved: ConversationCheckpoint = serde_json::from_slice(&directory.read(
                    &format!("checkpoint-{turn}.json"),
                    MAX_CHECKPOINT_BYTES as u64,
                )?)?;
                config.validate_checkpoint(&saved).map_err(invalid)?;
                if saved.completed_turn != *turn {
                    return Err(invalid("checkpoint turn mismatch"));
                }
                Ok(saved)
            })
            .transpose()?;
        let result: RunResult = serde_json::from_slice(
            &directory.read(&format!("result-{failed}.json"), CONFIG_BYTES)?,
        )?;
        if result.run_id != *failed
            || result.mode != config.mode
            || result.reason == StopReason::AgentFinished
            || serde_json::to_value(&result.cleanup)? != json!({"status":"complete"})
        {
            return Err(invalid(
                "recovery needs a stopped turn with recorded complete project cleanup",
            ));
        }
        let input: Value = serde_json::from_slice(
            &directory.read(&format!("input-{failed}.json"), CONFIG_BYTES)?,
        )?;
        if input["schema_version"] != 1
            || input["turn"] != failed.as_str()
            || input["previous"] != serde_json::to_value(&head.checkpoint)?
        {
            return Err(invalid("interrupted input identity mismatch"));
        }
        let pending = input["input"]
            .as_str()
            .filter(|s| !s.trim().is_empty() && s.len() <= MAX_USER_BYTES)
            .ok_or_else(|| invalid("invalid interrupted input"))?;
        let (sequence, calls, tools) = audit(
            &directory,
            config,
            &result,
            checkpoint.as_ref().map_or(1, |s| s.next_sequence),
        )?;
        if let Some(saved) = &mut checkpoint {
            saved.next_sequence = sequence;
            saved.next_request = saved
                .next_request
                .checked_add(calls)
                .ok_or_else(|| invalid("request ordinal overflow"))?;
            config.validate_checkpoint(saved).map_err(invalid)?;
        }
        let uncertain_tools = tools
            .iter()
            .filter(|t| matches!(t.observation, ToolObservation::OutcomeUnknown))
            .count();
        let omitted_tool_previews = tools.len().saturating_sub(TOOL_PREVIEWS);
        let report = InterruptionReport {
            source: path.canonicalize()?,
            failed_turn: failed.clone(),
            reason: result.reason,
            pending_input: Excerpt::new(input["raw_user_input"].as_str().unwrap_or(pending), 4096),
            earlier_recovery: input["recovery_context"]
                .as_str()
                .map(|s| Excerpt::new(s, 4096)),
            model_calls: calls,
            unknown_model_requests: result.usage.unknown_requests,
            tool_requests: tools.len(),
            uncertain_tools,
            omitted_tool_previews,
            tools: tools.into_iter().skip(omitted_tool_previews).collect(),
        };
        Ok(Self {
            _directory: directory,
            _lock: lock,
            head,
            checkpoint,
            report,
        })
    }
}

struct Tool {
    evidence: ToolEvidence,
    dispatched: bool,
    terminal: bool,
}

fn audit(
    directory: &PrivateDirectory,
    config: &ConversationConfig,
    result: &RunResult,
    mut sequence: u64,
) -> io::Result<(u64, u64, Vec<ToolEvidence>)> {
    let prefix = format!("events-{}-", result.run_id);
    let mut names: Vec<_> = directory
        .names()?
        .into_iter()
        .filter(|n| n.starts_with(&prefix) && n.ends_with(".jsonl"))
        .collect();
    names.sort();
    let mut remaining = config.parameters.limits.log_bytes.get();
    let (mut started, mut configured, mut stopped, mut finished) = (false, false, false, false);
    let (mut elapsed, mut calls) = (0, 0);
    let mut settled = false;
    let mut totals = None;
    let mut tools = BTreeMap::<u64, Tool>::new();
    for name in names {
        if name != format!("{prefix}{sequence:020}.jsonl") {
            return Err(invalid("incomplete interrupted-turn event sequence"));
        }
        let bytes = directory.read(&name, remaining)?;
        remaining = remaining
            .checked_sub(bytes.len() as u64)
            .ok_or_else(|| invalid("oversized recovery log"))?;
        if bytes.is_empty() || !bytes.ends_with(b"\n") {
            return Err(invalid("incomplete event batch"));
        }
        for line in bytes.split_inclusive(|b| *b == b'\n') {
            let r: Record = serde_json::from_slice(line)?;
            if r.sequence != sequence
                || r.elapsed_ms < elapsed
                || finished
                || (!started && r.kind != "controller_started")
            {
                return Err(invalid("invalid interrupted-turn record order"));
            }
            let d = &r.data;
            let valid = match r.kind.as_str() {
                "controller_started" => {
                    let ok = !started
                        && d["run_id"] == result.run_id.as_str()
                        && d["mode"] == serde_json::to_value(config.mode)?
                        && d["parameters"] == serde_json::to_value(&config.parameters)?;
                    started = true;
                    ok
                }
                "compaction_configured" => {
                    let ok =
                        !configured && d["settings"] == serde_json::to_value(&config.compaction)?;
                    configured = true;
                    ok
                }
                "model_request" => {
                    let ok = !stopped && (calls == 0 || settled) && d["call"] == calls + 1;
                    calls += 1;
                    settled = false;
                    ok
                }
                "request_settled" | "request_not_dispatched" => {
                    let ok = calls > 0 && !settled && d["call"] == calls;
                    settled = true;
                    if r.kind == "request_settled" {
                        totals = Some(d["totals"].clone());
                    }
                    ok
                }
                "model_response" | "model_completion" | "model_chunk" => {
                    calls > 0 && d["call"] == calls
                }
                "model_error" | "model_usage_overflow" | "model_chunk_rejected" => calls > 0,
                "tool_request" => {
                    let name = d["name"]
                        .as_str()
                        .filter(|s| matches!(*s, "exec" | "read_file" | "write_file"));
                    if let Some(name) = name {
                        tools.insert(
                            sequence,
                            Tool {
                                evidence: ToolEvidence {
                                    request_sequence: sequence,
                                    name: name.into(),
                                    arguments: Excerpt::new(
                                        &serde_json::to_string(&d["arguments"])?,
                                        768,
                                    ),
                                    observation: ToolObservation::OutcomeUnknown,
                                    result: None,
                                },
                                dispatched: false,
                                terminal: false,
                            },
                        );
                    }
                    !stopped && calls > 0 && name.is_some()
                }
                "tool_dispatch" | "tool_result" | "tool_not_dispatched" => {
                    let key = d["request_sequence"]
                        .as_u64()
                        .ok_or_else(|| invalid("invalid tool reference"))?;
                    let tool = tools
                        .get_mut(&key)
                        .ok_or_else(|| invalid("tool observation without request"))?;
                    let ok = !tool.terminal
                        && (r.kind == "tool_not_dispatched" || d["name"] == tool.evidence.name);
                    match r.kind.as_str() {
                        "tool_dispatch" => {
                            if tool.dispatched {
                                return Err(invalid("duplicate tool dispatch"));
                            }
                            tool.dispatched = true;
                        }
                        "tool_result" => {
                            if !d["result"].is_object() {
                                return Err(invalid("invalid tool result"));
                            }
                            tool.evidence.observation = ToolObservation::ResultObserved;
                            tool.evidence.result =
                                Some(Excerpt::new(&serde_json::to_string(&d["result"])?, 1536));
                            tool.terminal = true;
                        }
                        _ => {
                            tool.evidence.observation = ToolObservation::NotDispatched;
                            tool.terminal = true;
                        }
                    }
                    ok
                }
                "controller_stop" => {
                    let ok = !stopped
                        && d["reason"] == serde_json::to_value(result.reason)?
                        && d["usage"] == serde_json::to_value(&result.usage)?;
                    stopped = true;
                    ok
                }
                "controller_finished" => {
                    let ok = stopped
                        && d["run_id"] == result.run_id.as_str()
                        && d["reason"] == serde_json::to_value(result.reason)?
                        && d["cleanup"] == serde_json::to_value(&result.cleanup)?
                        && d["usage"] == serde_json::to_value(&result.usage)?
                        && d["mode"] == serde_json::to_value(result.mode)?
                        && d["diagnostics"] == serde_json::to_value(&result.diagnostics)?
                        && r.elapsed_ms == result.elapsed_ms;
                    finished = true;
                    ok
                }
                "phase_changed"
                | "conversation_workspace_started"
                | "compaction_planned"
                | "compaction_applied"
                | "compaction_deferred"
                | "compaction_rejected"
                | "tool_batch_rejected"
                | "publication_delayed"
                | "diagnostic" => true,
                _ => false,
            };
            if !valid {
                return Err(invalid(format!(
                    "unsupported or inconsistent recovery observation: {}",
                    r.kind
                )));
            }
            elapsed = r.elapsed_ms;
            sequence = sequence
                .checked_add(1)
                .ok_or_else(|| invalid("sequence overflow"))?;
        }
    }
    if !finished
        || !configured
        || calls != result.model_calls
        || (calls > 0 && !settled)
        || totals.is_some_and(|v| v != serde_json::to_value(&result.usage).unwrap())
    {
        return Err(invalid("incomplete interrupted-turn evidence"));
    }
    // A dispatch record is intent, not proof the native operation started. An
    // absent result is conservatively unknown even if the client died earlier.
    Ok((
        sequence,
        calls,
        tools.into_values().map(|t| t.evidence).collect(),
    ))
}
