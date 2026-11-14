//! Explicit recovery of a length-limited, tool-free first compaction. The
//! source stays fenced; only its last completed history enters a new session.

use std::{collections::BTreeMap, io, path::Path};

use playtime_core::StopReason;
use playtime_engine::{Record, RunResult, chat::ConversationConfig};
use playtime_local::PrivateDirectory;
use serde_json::{Value, json};

use crate::{CONFIG_BYTES, Head, Session, invalid};

pub struct RecoveredCompaction {
    pub session: Session,
    /// The user input that never reached a work request. The caller chooses
    /// whether to submit it again; recovery itself executes no effects.
    pub pending_input: String,
}

impl Session {
    /// Fork the last completed conversation after one truncated summary and
    /// confirmed cleanup, increasing only the summary output cap. Reject work,
    /// tool execution, uncertain usage, incomplete logs and other config changes.
    /// The caller must independently own/verify the model and project resources.
    pub fn recover_compaction(
        source: &Path,
        destination: &Path,
        expected: ConversationConfig,
    ) -> io::Result<RecoveredCompaction> {
        let directory = PrivateDirectory::open(source, false)?;
        let _lock = directory.lock()?;
        let config: ConversationConfig =
            serde_json::from_slice(&directory.read("config.json", CONFIG_BYTES)?)?;
        let head: Head = serde_json::from_slice(&directory.read("head.json", 4096)?)?;
        if head.schema_version != 1
            || head.configuration != config.fingerprint().map_err(invalid)?
        {
            return Err(invalid("invalid source session identity"));
        }
        let mut updated = config.clone();
        updated.compaction.summary_tokens = expected.compaction.summary_tokens;
        if updated != expected
            || updated.compaction.summary_tokens <= config.compaction.summary_tokens
        {
            return Err(invalid(
                "compaction recovery may only increase the summary output cap",
            ));
        }
        expected.validate().map_err(invalid)?;
        let failed = head
            .active_turn
            .as_ref()
            .ok_or_else(|| invalid("no failed turn to recover"))?;
        let previous = head
            .checkpoint
            .as_ref()
            .ok_or_else(|| invalid("no completed conversation checkpoint"))?;
        let mut checkpoint: playtime_engine::chat::ConversationCheckpoint =
            serde_json::from_slice(&directory.read(
                &format!("checkpoint-{previous}.json"),
                playtime_engine::chat::MAX_CHECKPOINT_BYTES as u64,
            )?)?;
        config.validate_checkpoint(&checkpoint).map_err(invalid)?;
        if checkpoint.completed_turn != *previous {
            return Err(invalid("source checkpoint identity mismatch"));
        }
        let result: RunResult = serde_json::from_slice(
            &directory.read(&format!("result-{failed}.json"), CONFIG_BYTES)?,
        )?;
        if result.run_id != *failed
            || result.reason != StopReason::CompactionFailed
            || serde_json::to_value(&result.cleanup)? != json!({"status":"complete"})
            || result.model_calls != 1
            || !result.usage.usage_complete
            || result.usage.unknown_requests != 0
            || result.usage.unknown_reserved_tokens != 0
            || result.usage.known_generated_tokens != config.compaction.summary_tokens.get()
            || result.usage.budget_charged_tokens != result.usage.known_generated_tokens
        {
            return Err(invalid(
                "recovery requires one fully accounted summary and confirmed cleanup",
            ));
        }
        let input: Value = serde_json::from_slice(
            &directory.read(&format!("input-{failed}.json"), CONFIG_BYTES)?,
        )?;
        let pending_input = input["input"]
            .as_str()
            .filter(|s| !s.trim().is_empty() && s.len() <= playtime_engine::chat::MAX_USER_BYTES)
            .ok_or_else(|| invalid("invalid pending user input"))?
            .to_owned();
        if input["schema_version"] != 1
            || input["turn"] != failed.as_str()
            || input["previous"] != previous.as_str()
        {
            return Err(invalid("pending input does not match the source head"));
        }
        let next_sequence = verify_records(
            &directory,
            failed.as_str(),
            checkpoint.next_sequence,
            &config,
            &result,
        )?;
        // Keep request/observation ordinals unique even though the rejected
        // summary is omitted from the recovered conversation history.
        checkpoint.next_sequence = next_sequence;
        checkpoint.next_request = checkpoint
            .next_request
            .checked_add(1)
            .ok_or_else(|| invalid("request ordinal overflow"))?;
        checkpoint.configuration = expected.fingerprint().map_err(invalid)?;
        expected.validate_checkpoint(&checkpoint).map_err(invalid)?;
        let destination = PrivateDirectory::open(destination, true)?;
        let lock = destination.lock()?;
        if !destination.names()?.is_empty() {
            return Err(invalid("recovery destination is not empty"));
        }
        let config_bytes = serde_json::to_vec(&expected)?;
        if config_bytes.len() as u64 > CONFIG_BYTES {
            return Err(invalid("session configuration is too large"));
        }
        // The first head is already fenced. A partially published fork must
        // never look like an empty, ready conversation.
        let mut session = Self {
            directory: destination,
            _lock: lock,
            config: expected,
            head: Head {
                schema_version: 1,
                configuration: checkpoint.configuration.clone(),
                checkpoint: None,
                active_turn: Some(failed.clone()),
            },
            checkpoint: None,
        };
        session.directory.write_new("config.json", &config_bytes)?;
        session
            .directory
            .write_new("head.json", &serde_json::to_vec(&session.head)?)?;
        session.directory.write_new("recovery.json", &serde_json::to_vec(&json!({
            "schema_version":1, "kind":"truncated_first_compaction", "source":source.canonicalize()?,
            "source_configuration":head.configuration, "source_checkpoint":previous,
            "failed_turn":failed, "failed_result":result, "pending_input":pending_input,
        }))?)?;
        session.directory.write_new(
            &format!("checkpoint-{previous}.json"),
            &serde_json::to_vec(&checkpoint)?,
        )?;
        session.head.checkpoint = Some(previous.clone());
        session.head.active_turn = None;
        session
            .directory
            .replace("head.json", &serde_json::to_vec(&session.head)?)?;
        session.checkpoint = Some(checkpoint);
        Ok(RecoveredCompaction {
            session,
            pending_input,
        })
    }
}

fn verify_records(
    directory: &PrivateDirectory,
    turn: &str,
    mut sequence: u64,
    config: &ConversationConfig,
    result: &RunResult,
) -> io::Result<u64> {
    let prefix = format!("events-{turn}-");
    let mut names: Vec<_> = directory
        .names()?
        .into_iter()
        .filter(|n| n.starts_with(&prefix) && n.ends_with(".jsonl"))
        .collect();
    names.sort();
    let mut remaining = config.parameters.limits.log_bytes.get();
    let mut counts = BTreeMap::<String, u64>::new();
    let mut last = String::new();
    let mut elapsed = 0;
    for name in names {
        if name != format!("{prefix}{sequence:020}.jsonl") {
            return Err(invalid("incomplete compaction event sequence"));
        }
        let bytes = directory.read(&name, remaining)?;
        remaining = remaining
            .checked_sub(bytes.len() as u64)
            .ok_or_else(|| invalid("oversized recovery log"))?;
        if bytes.is_empty() || !bytes.ends_with(b"\n") {
            return Err(invalid("incomplete compaction event batch"));
        }
        for line in bytes.split_inclusive(|b| *b == b'\n') {
            let r: Record = serde_json::from_slice(line)?;
            if r.sequence != sequence || r.elapsed_ms < elapsed || last == "controller_finished" {
                return Err(invalid("invalid compaction event sequence"));
            }
            if counts.is_empty() && r.kind != "controller_started" {
                return Err(invalid("missing controller start"));
            }
            let d = &r.data;
            let valid = match r.kind.as_str() {
                "controller_started" => {
                    d["run_id"] == turn
                        && d["parameters"] == serde_json::to_value(&config.parameters)?
                }
                "compaction_configured" => {
                    d["settings"] == serde_json::to_value(&config.compaction)?
                }
                "model_request" => {
                    d["call"] == 1
                        && d["purpose"] == "compaction"
                        && d["maximum_tokens"] == config.compaction.summary_tokens.get()
                        && d["tools"] == json!([])
                }
                "model_chunk" => d["call"] == 1,
                "request_settled" => {
                    d["call"] == 1
                        && d["totals"] == serde_json::to_value(&result.usage)?
                        && d["usage"]["status"] == "known"
                }
                "model_response" => {
                    d["call"] == 1
                        && d["response"]["done"] == true
                        && d["response"]["generated_tokens"]
                            == config.compaction.summary_tokens.get()
                }
                "model_completion" => d["call"] == 1 && d["finish_reason"] == "length",
                "controller_stop" => d["reason"] == "compaction_failed",
                "controller_finished" => {
                    d["run_id"] == turn
                        && d["reason"] == "compaction_failed"
                        && d["cleanup"] == serde_json::to_value(&result.cleanup)?
                        && d["usage"] == serde_json::to_value(&result.usage)?
                }
                "conversation_workspace_started" | "phase_changed" | "compaction_planned" => true,
                // Any work/tool request or unrecognized failure needs separate
                // reconciliation; it cannot be retried by this path.
                _ => false,
            };
            if !valid {
                return Err(invalid(format!(
                    "unsupported recovery observation: {}",
                    r.kind
                )));
            }
            *counts.entry(r.kind.clone()).or_default() += 1;
            last = r.kind;
            elapsed = r.elapsed_ms;
            sequence = sequence
                .checked_add(1)
                .ok_or_else(|| invalid("observation sequence overflow"))?;
        }
    }
    let once = [
        "controller_started",
        "compaction_configured",
        "conversation_workspace_started",
        "model_request",
        "compaction_planned",
        "request_settled",
        "model_response",
        "model_completion",
        "controller_stop",
        "controller_finished",
    ];
    if last != "controller_finished" || once.iter().any(|k| counts.get(*k) != Some(&1)) {
        return Err(invalid("incomplete or repeated compaction observations"));
    }
    Ok(sequence)
}
