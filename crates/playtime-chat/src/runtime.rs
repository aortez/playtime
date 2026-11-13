use crate::Result;
use playtime_engine::{Effect, EffectKind, Input, Outcome, RunResult, simulation::SequentialIds};
use playtime_harness::{Session, Turn};
use serde::Serialize;
use serde_json::Value;
use std::{sync::Arc, time::Duration};
use tokio::{
    sync::{Mutex, mpsc},
    time::Instant,
};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Started { turn: String },
    Effect { effect: Effect },
    Chunk { chunk: Value },
    ToolResult { value: Value },
    Stopped { result: Box<RunResult> },
}
pub fn events() -> (mpsc::Sender<Event>, mpsc::Receiver<Event>) {
    mpsc::channel(256)
}

/// Effects own their native bounds. A possibly dispatched failure must never be
/// retried here. `cleanup` must stop only this turn's owned processes and retain
/// uncertainty if cessation cannot be proven. The shared controller owns policy.
#[async_trait::async_trait]
pub trait Effects: Send + 'static {
    async fn execute(
        &mut self,
        effect: &Effect,
        deadline: Instant,
        cancel: CancellationToken,
        chunks: mpsc::Sender<Value>,
    ) -> Outcome;
    async fn cleanup(&mut self, deadline: Instant) -> Vec<String>;
}
pub struct Completed {
    pub session: Option<Session>,
    pub result: RunResult,
}
type ChatTurn = Turn<SequentialIds>;
fn now(start: Instant) -> u64 {
    start.elapsed().as_millis().try_into().unwrap_or(u64::MAX)
}
async fn advance(
    mut turn: ChatTurn,
    start: Instant,
    input: Input,
) -> Result<(ChatTurn, Option<String>)> {
    Ok(tokio::task::spawn_blocking(move || {
        let error = turn.advance(now(start), input).err().map(|e| e.to_string());
        (turn, error)
    })
    .await?)
}
fn emit(tx: &mpsc::Sender<Event>, event: Event, cancel: &CancellationToken) {
    // A disconnected or blocked frontend stops work; it never blocks process
    // deadlines or permits an invisible stream of editing commands.
    if tx.try_send(event).is_err() {
        cancel.cancel();
    }
}

/// Runs one turn, including cleanup on storage/adapter errors. The owner must
/// signal `cancel`, not abort this future. On interruption the saved turn remains
/// fenced; only a successfully committed checkpoint returns a reusable Session.
pub async fn run_turn<E: Effects>(
    session: Session,
    input: String,
    effects: Arc<Mutex<E>>,
    events: mpsc::Sender<Event>,
    cancel: CancellationToken,
) -> Result<Completed> {
    let start = Instant::now();
    let owned = cancel.child_token();
    let _on_drop = owned.clone().drop_guard();
    let result = drive(session, input, effects.clone(), &events, owned, start).await;
    if result.as_ref().is_ok_and(|done| {
        done.result.cleanup == playtime_protocol::events::CleanupOutcome::Complete
    }) {
        return result;
    }
    // Even a failure before begin/dispatch must release the project reservation.
    // The exclusive adapter borrow waits for any cancelled native task; its
    // absolute deadline remains independent of record persistence.
    let cleanup = effects
        .lock()
        .await
        .cleanup(Instant::now() + Duration::from_secs(20))
        .await;
    if !cleanup.is_empty() {
        return Err(format!("owned cleanup unresolved: {}", cleanup.join("; ")).into());
    }
    result
}
async fn drive<E: Effects>(
    session: Session,
    input: String,
    effects: Arc<Mutex<E>>,
    events: &mpsc::Sender<Event>,
    cancel: CancellationToken,
    start: Instant,
) -> Result<Completed> {
    let mut turn =
        tokio::task::spawn_blocking(move || session.begin(input, SequentialIds, 0)).await??;
    emit(
        events,
        Event::Started {
            turn: turn.id().to_string(),
        },
        &cancel,
    );
    let mut storage_error = None;
    let mut cancelled = false;
    loop {
        if cancel.is_cancelled() && !cancelled {
            let (next, error) = advance(turn, start, Input::Cancel).await?;
            turn = next;
            storage_error = storage_error.or(error);
            cancelled = true;
        }
        let Some(effect) = turn.pending().cloned() else {
            break;
        };
        let (next, error) = advance(
            turn,
            start,
            Input::Dispatched {
                effect_id: effect.id,
            },
        )
        .await?;
        turn = next;
        storage_error = storage_error.or(error);
        if turn.pending().is_none_or(|e| e.id != effect.id) {
            continue;
        }
        emit(
            events,
            Event::Effect {
                effect: effect.clone(),
            },
            &cancel,
        );
        let deadline = start + Duration::from_millis(effect.deadline_ms);
        let cleanup = matches!(effect.kind, EffectKind::Cleanup);
        let effect_cancel = if cleanup {
            CancellationToken::new()
        } else {
            cancel.child_token()
        };
        let task_cancel = effect_cancel.clone();
        let _cancel_effect = effect_cancel.clone().drop_guard();
        let (tx, mut rx) = mpsc::channel(64);
        let task_effect = effect.clone();
        let task_effects = effects.clone();
        let task = tokio::spawn(async move {
            task_effects
                .lock()
                .await
                .execute(&task_effect, deadline, task_cancel, tx)
                .await
        });
        // The adapter task services cancellation/deadlines while record fsync
        // runs on a blocking worker. No later effect can pass that commit.
        while let Some(chunk) = rx.recv().await {
            let (next, error) = advance(
                turn,
                start,
                Input::Chunk {
                    effect_id: effect.id,
                    chunk: chunk.clone(),
                },
            )
            .await?;
            turn = next;
            storage_error = storage_error.or(error);
            emit(events, Event::Chunk { chunk }, &cancel);
            if turn.pending().is_none_or(|e| e.id != effect.id) {
                effect_cancel.cancel();
            }
        }
        let outcome = task
            .await
            .map_err(|e| format!("effect worker failed: {e}"))?;
        if cancel.is_cancelled() && !cancelled && !cleanup {
            let (next, error) = advance(turn, start, Input::Cancel).await?;
            turn = next;
            storage_error = storage_error.or(error);
            cancelled = true;
        }
        // Cancellation or a chunk budget can replace this effect with cleanup.
        // Its late reply cannot complete the replacement or settle an unknown
        // request through the ordinary completion path.
        if turn.pending().is_none_or(|e| e.id != effect.id) {
            continue;
        }
        let tool_reply = if let Outcome::Tool(value) = &outcome {
            Some(value.clone())
        } else {
            None
        };
        let (next, error) = advance(
            turn,
            start,
            Input::Complete {
                effect_id: effect.id,
                outcome,
            },
        )
        .await?;
        turn = next;
        storage_error = storage_error.or(error);
        if storage_error.is_none()
            && let Some(value) = tool_reply
        {
            emit(events, Event::ToolResult { value }, &cancel);
        }
    }
    let result = turn
        .result()
        .ok_or("controller stopped without a result")?
        .clone();
    emit(
        events,
        Event::Stopped {
            result: Box::new(result.clone()),
        },
        &cancel,
    );
    if let Some(error) = storage_error {
        return Err(format!("session write failed; turn remains fenced: {error}").into());
    }
    let session = if result.reason == playtime_core::StopReason::AgentFinished
        && result.cleanup == playtime_protocol::events::CleanupOutcome::Complete
    {
        Some(tokio::task::spawn_blocking(move || turn.finish()).await??)
    } else {
        None
    };
    Ok(Completed { session, result })
}
