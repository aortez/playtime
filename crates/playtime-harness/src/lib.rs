//! Private standalone conversations around the shared bounded controller.
//! Effect execution belongs to an adapter; this crate executes no command or
//! model request. The owner must service deadlines, cancellation and cleanup.

use std::{fs::File, io, path::Path};

use playtime_core::{RunId, Sha256Digest};
use playtime_engine::{
    Controller, Effect, EffectKind, IdSource, Input, RunResult,
    chat::{ConversationCheckpoint, ConversationConfig, ConversationTurn, MAX_CHECKPOINT_BYTES},
};
use playtime_local::{PrivateDirectory, Token};
use serde::{Deserialize, Serialize};

mod interruption;
pub use interruption::{InterruptionReport, ToolObservation};
mod recovery;
pub use recovery::RecoveredCompaction;

const CONFIG_BYTES: u64 = 1024 * 1024;

fn invalid(message: impl ToString) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_string())
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Head {
    schema_version: u32,
    configuration: Sha256Digest,
    checkpoint: Option<RunId>,
    active_turn: Option<RunId>,
}

/// An exclusively held private session directory. Keep it outside every tool
/// write root. Only completed turns are resumable; an active marker survives a
/// crash and prevents an uncertain tool call from being silently replayed.
pub struct Session {
    directory: PrivateDirectory,
    _lock: File,
    config: ConversationConfig,
    head: Head,
    checkpoint: Option<ConversationCheckpoint>,
    interruption: Option<interruption::Interruption>,
}

impl Session {
    pub fn create(path: &Path, config: ConversationConfig) -> io::Result<Self> {
        let configuration = config.fingerprint().map_err(invalid)?;
        let bytes = serde_json::to_vec(&config)?;
        if bytes.len() as u64 > CONFIG_BYTES {
            return Err(invalid("session configuration is too large"));
        }
        let directory = PrivateDirectory::open(path, true)?;
        let lock = directory.lock()?;
        if !directory.names()?.is_empty() {
            return Err(invalid("session directory is not empty"));
        }
        let head = Head {
            schema_version: 1,
            configuration,
            checkpoint: None,
            active_turn: None,
        };
        directory.write_new("config.json", &bytes)?;
        directory.write_new("head.json", &serde_json::to_vec(&head)?)?;
        Ok(Self {
            directory,
            _lock: lock,
            config,
            head,
            checkpoint: None,
            interruption: None,
        })
    }

    pub fn open(path: &Path, expected: &ConversationConfig) -> io::Result<Self> {
        Self::open_inner(path, expected, false)
    }

    /// Validate a saved session without waiting for a live console's lease.
    pub fn try_open(path: &Path, expected: &ConversationConfig) -> io::Result<Self> {
        Self::open_inner(path, expected, true)
    }

    fn open_inner(
        path: &Path,
        expected: &ConversationConfig,
        nonblocking: bool,
    ) -> io::Result<Self> {
        let directory = PrivateDirectory::open(path, false)?;
        let lock = if nonblocking {
            directory.try_lock()?
        } else {
            directory.lock()?
        };
        let config: ConversationConfig =
            serde_json::from_slice(&directory.read("config.json", CONFIG_BYTES)?)?;
        let head: Head = serde_json::from_slice(&directory.read("head.json", 4096)?)?;
        if head.schema_version != 1
            || config != *expected
            || head.configuration != config.fingerprint().map_err(invalid)?
        {
            return Err(invalid(
                "session configuration differs from the selected model/workspace/policy",
            ));
        }
        if let Some(turn) = &head.active_turn {
            return Err(invalid(format!(
                "turn {turn} is unfinished or uncommitted; inspect its records before starting a new session; automatic recovery is disabled"
            )));
        }
        let checkpoint = head
            .checkpoint
            .as_ref()
            .map(|turn| {
                let saved: ConversationCheckpoint = serde_json::from_slice(&directory.read(
                    &format!("checkpoint-{turn}.json"),
                    MAX_CHECKPOINT_BYTES as u64,
                )?)?;
                config.validate_checkpoint(&saved).map_err(invalid)?;
                if saved.completed_turn != *turn {
                    return Err(invalid("checkpoint turn identity mismatch"));
                }
                Ok(saved)
            })
            .transpose()?;
        let interruption = interruption::Interruption::read(&directory, &head.configuration)?;
        Ok(Self {
            directory,
            _lock: lock,
            config,
            head,
            checkpoint,
            interruption,
        })
    }

    pub fn checkpoint(&self) -> Option<&ConversationCheckpoint> {
        self.checkpoint.as_ref()
    }

    pub fn begin<I: IdSource>(mut self, input: String, ids: I, now_ms: u64) -> io::Result<Turn<I>> {
        let run_id =
            RunId::new(format!("turn-{}", Token::generate()?.expose())).map_err(invalid)?;
        let raw_input = input;
        if raw_input.trim().is_empty() || raw_input.len() > playtime_engine::chat::MAX_USER_BYTES {
            return Err(invalid(
                "a new nonempty user message of at most 256 KiB is required",
            ));
        }
        let recovery_context = self.pending_recovery().map(|r| r.context()).transpose()?;
        let input = recovery_context.as_ref().map_or_else(
            || raw_input.clone(),
            |notice| format!("{notice}\n[New user message]\n{raw_input}"),
        );
        let controller = Controller::chat(
            self.config.clone(),
            ConversationTurn {
                input: input.clone(),
                previous: self.checkpoint.clone(),
            },
            run_id.clone(),
            ids,
            now_ms,
        )
        .map_err(invalid)?;
        self.directory.write_new(
            &format!("input-{run_id}.json"),
            &serde_json::to_vec(&serde_json::json!({
                "schema_version":1, "turn":run_id, "previous":self.head.checkpoint, "input":input,
                "raw_user_input":raw_input, "recovery_context":recovery_context,
            }))?,
        )?;
        self.head.active_turn = Some(run_id.clone());
        // Commit the fence before exposing even the workspace-start effect.
        self.directory
            .replace("head.json", &serde_json::to_vec(&self.head)?)?;
        let next_sequence = self.checkpoint.as_ref().map_or(1, |s| s.next_sequence);
        let mut turn = Turn {
            session: self,
            controller,
            run_id,
            next_sequence,
            storage_failed: false,
            result_saved: false,
        };
        turn.flush()?;
        Ok(turn)
    }
}

/// The caller must dispatch only the current effect, record `Dispatched` before
/// execution, and recheck it afterwards. A persistence error stops work, but the
/// caller must still execute and report the pending cleanup effect.
#[must_use = "service the turn's effects and cleanup before releasing it"]
pub struct Turn<I: IdSource> {
    session: Session,
    controller: Controller<I>,
    run_id: RunId,
    next_sequence: u64,
    storage_failed: bool,
    result_saved: bool,
}

impl<I: IdSource> Turn<I> {
    pub fn pending(&self) -> Option<&Effect> {
        self.controller
            .pending()
            .filter(|e| !self.storage_failed || matches!(e.kind, EffectKind::Cleanup))
    }
    pub fn next_wakeup_ms(&self) -> Option<u64> {
        self.controller.next_wakeup_ms()
    }
    pub fn result(&self) -> Option<&RunResult> {
        self.controller.result()
    }
    pub fn id(&self) -> &RunId {
        &self.run_id
    }

    pub fn advance(&mut self, now_ms: u64, input: Input) -> io::Result<()> {
        self.controller.advance(now_ms, input).map_err(invalid)?;
        if self.storage_failed {
            self.controller.take_records();
            self.controller.take_control_updates();
            return Ok(());
        }
        if let Err(error) = self.flush() {
            self.storage_failed = true;
            // No subsequent write can promote this turn, even if the failed
            // sync actually reached disk. The durable active marker stays set.
            self.controller
                .advance(now_ms, Input::PersistenceFailed(error.to_string()))
                .map_err(invalid)?;
            self.controller.take_records();
            self.controller.take_control_updates();
            return Err(error);
        }
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        let records = self.controller.take_records();
        self.controller.take_control_updates();
        if !records.is_empty() {
            let start = self.next_sequence;
            let mut bytes = Vec::new();
            for record in records {
                if record.sequence != self.next_sequence {
                    return Err(invalid("conversation record sequence mismatch"));
                }
                serde_json::to_writer(&mut bytes, &record)?;
                bytes.push(b'\n');
                self.next_sequence = self
                    .next_sequence
                    .checked_add(1)
                    .ok_or_else(|| invalid("conversation sequence overflow"))?;
            }
            self.session
                .directory
                .write_new(&format!("events-{}-{start:020}.jsonl", self.run_id), &bytes)?;
        }
        if let Some(result) = self.controller.result().filter(|_| !self.result_saved) {
            self.session.directory.write_new(
                &format!("result-{}.json", self.run_id),
                &serde_json::to_vec(result)?,
            )?;
            self.result_saved = true;
        }
        Ok(())
    }

    pub fn finish(mut self) -> io::Result<Session> {
        if self.storage_failed {
            return Err(invalid("session persistence failed; turn remains fenced"));
        }
        let saved = self
            .controller
            .conversation_checkpoint()
            .ok_or_else(|| invalid("turn is not cleanly completed; no resumable checkpoint"))?;
        self.session
            .config
            .validate_checkpoint(&saved)
            .map_err(invalid)?;
        if saved.next_sequence != self.next_sequence {
            return Err(invalid("checkpoint has uncommitted observations"));
        }
        self.session.directory.write_new(
            &format!("checkpoint-{}.json", self.run_id),
            &serde_json::to_vec(&saved)?,
        )?;
        self.session.head.checkpoint = Some(self.run_id);
        self.session.head.active_turn = None;
        self.session
            .directory
            .replace("head.json", &serde_json::to_vec(&self.session.head)?)?;
        self.session.checkpoint = Some(saved);
        Ok(self.session)
    }
}
