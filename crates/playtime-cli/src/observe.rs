use crate::output::{Failure, Output};
use playtime_client::{Client, Error, Observation, RetryPolicy, state::CursorReceipt};
use playtime_core::{RunId, StopReason};
use playtime_protocol::api::*;
use serde_json::json;
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::time::{Instant, sleep, timeout_at};
use tokio_util::sync::CancellationToken;

pub fn terminal(view: &RunView) -> bool {
    view.orchestration.state == OrchestrationState::Terminal
}
/// The daemon stopped monitoring a run that isn't Terminal, so nothing changes
/// until `playtime runs release`. Reconciling with `control_uncertain` is
/// still being resolved and doesn't count.
pub fn awaiting_release(view: &RunView) -> bool {
    view.orchestration.state == OrchestrationState::Reconciling
        && matches!(
            view.orchestration.reason.as_deref(),
            Some("worker_unavailable" | "coordinator_restarted" | "execution_failed")
        )
}
/// What the operator has to do after an observer returns, if anything.
pub fn next_step(view: &RunView) -> Option<String> {
    awaiting_release(view).then(|| format!("playtime runs release {} --reason TEXT", view.run_id))
}
pub fn outcome_exit(view: &RunView, cancellation: bool) -> i32 {
    if awaiting_release(view) {
        return 9;
    }
    if !terminal(view) || view.cleanup != Cleanup::Complete {
        return 10;
    }
    if cancellation {
        return if view.cancellation.as_ref().is_none_or(|intent| {
            intent.enforcement == Enforcement::Stopped && intent.uncertainty.is_none()
        }) {
            0
        } else {
            10
        };
    }
    if !matches!(view.evidence, EvidenceAvailability::Available) {
        return 10;
    }
    let Some(outcome) = &view.outcome else {
        return 10;
    };
    if !view
        .last_confirmed_frontier
        .as_ref()
        .is_some_and(|f| outcome.event_sequence > 0 && outcome.event_sequence <= f.prefix.sequence)
    {
        return 10;
    }
    if matches!(
        outcome.reason,
        StopReason::AgentFinished
            | StopReason::SessionLimit
            | StopReason::TokenLimit
            | StopReason::WallLimit
            | StopReason::LogLimit
    ) {
        0
    } else {
        10
    }
}
pub async fn bounded<T>(
    deadline: Instant,
    task: impl std::future::Future<Output = Result<T, Failure>>,
) -> Result<T, Failure> {
    timeout_at(deadline, task).await.map_err(|_| {
        Failure::new(
            8,
            "observer_deadline",
            "observer deadline expired; the run was not cancelled",
        )
    })?
}
fn transient(e: &Error) -> bool {
    matches!(e, Error::Transport | Error::Timeout)
        || matches!(
            e.api_code(),
            Some(
                "backend_unavailable"
                    | "ownership_unresolved"
                    | "request_timeout"
                    | "too_many_observers"
            )
        )
}
pub async fn wait(client: &Client, run: &RunId, deadline: Instant) -> Result<RunView, Failure> {
    bounded(deadline, async {
        let mut outage: Option<Instant> = None;
        loop {
            let read = if let Some(start) = outage {
                timeout_at(start + Duration::from_secs(60), client.run(run))
                    .await
                    .map_err(|_| Failure::from(Error::RetryBudgetExhausted))?
            } else {
                client.run(run).await
            };
            match read {
                Ok(view) => {
                    outage = None;
                    if terminal(&view) || awaiting_release(&view) {
                        return Ok(view);
                    }
                }
                Err(error) if transient(&error) => {
                    let start = outage.get_or_insert_with(Instant::now);
                    if start.elapsed() >= Duration::from_secs(60) {
                        return Err(Error::RetryBudgetExhausted.into());
                    }
                }
                Err(error) => return Err(error.into()),
            }
            sleep(Duration::from_millis(100)).await;
        }
    })
    .await
}
pub async fn persist(
    path: Option<&Path>,
    client: &Client,
    run: &RunId,
    cursor: String,
) -> Result<(), Failure> {
    if let Some(path) = path {
        let receipt = CursorReceipt {
            schema_version: 1,
            service_id: client.service_id().clone(),
            run_id: run.clone(),
            cursor,
        };
        let path = path.to_owned();
        tokio::task::spawn_blocking(move || receipt.write(&path))
            .await
            .map_err(|_| Failure::new(1, "cursor_task", "cursor task failed"))??;
    }
    Ok(())
}
pub async fn load_cursor(
    path: Option<&Path>,
    client: &Client,
    run: &RunId,
) -> Result<Option<String>, Failure> {
    let Some(path) = path else {
        return Ok(None);
    };
    let path = path.to_owned();
    let service = client.service_id().clone();
    let run = run.clone();
    Ok(
        tokio::task::spawn_blocking(move || CursorReceipt::read(&path, &service, &run))
            .await
            .map_err(|_| Failure::new(1, "cursor_task", "cursor task failed"))??
            .map(|r| r.cursor),
    )
}
/// A complete frame is written and flushed, then its cursor is saved, then
/// transport acknowledgment advances. A crash in between may redeliver.
pub async fn follow(
    client: &Client,
    run: &RunId,
    mut cursor: Option<String>,
    cursor_file: Option<PathBuf>,
    output: Output,
    deadline: Instant,
    cancel: &CancellationToken,
) -> Result<RunView, Failure> {
    bounded(deadline, async {
        loop {
            let mut following = client.follow(
                run,
                cursor.clone(),
                RetryPolicy {
                    max_reconnects: 100,
                    max_elapsed: Duration::from_secs(60),
                    initial_backoff: Duration::from_millis(100),
                    max_backoff: Duration::from_secs(5),
                },
            );
            loop {
                match following.next(cancel).await {
                    Ok(Observation::Record(record)) => {
                        output
                            .emit(
                                "event",
                                json!({"event":record.event,"cursor":record.cursor}),
                            )
                            .await?;
                        persist(cursor_file.as_deref(), client, run, record.cursor.clone()).await?;
                        following.acknowledge(&record)?;
                        cursor = Some(record.cursor);
                    }
                    Ok(Observation::End(end)) => {
                        output.emit("notice", json!({"stream_end":end})).await?;
                        return wait(client, run, deadline).await;
                    }
                    Ok(Observation::Disconnected) => return Err(Error::RetryBudgetExhausted.into()),
                    Err(error) if error.api_code() == Some("evidence_not_ready") => break,
                    Err(Error::RetryBudgetExhausted) => {
                        // EOF/outage never proves completion; status may now
                        // reconcile a terminal run whose stream was unavailable.
                        let view = client.run(run).await?;
                        if terminal(&view) {
                            return Ok(view);
                        }
                        return Err(Error::RetryBudgetExhausted.into());
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            let view = client.run(run).await?;
            if terminal(&view) {
                return Ok(view);
            }
            sleep(Duration::from_millis(100)).await;
        }
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn terminal_status_requires_committed_normal_outcome_and_confirmed_cleanup() {
        let contract: serde_json::Value = serde_json::from_str(include_str!(
            "../../playtime-server/tests/fixtures/http-contract.json"
        ))
        .unwrap();
        let mut view: RunView = serde_json::from_value(contract["completed_run"].clone()).unwrap();
        assert_eq!(outcome_exit(&view, false), 10);
        view.cleanup = Cleanup::Complete;
        for reason in [
            StopReason::AgentFinished,
            StopReason::SessionLimit,
            StopReason::TokenLimit,
            StopReason::WallLimit,
            StopReason::LogLimit,
        ] {
            view.outcome.as_mut().unwrap().reason = reason;
            assert_eq!(outcome_exit(&view, false), 0); // Unknown usage stays visible.
        }
        view.outcome.as_mut().unwrap().reason = StopReason::Interrupted;
        assert_eq!(outcome_exit(&view, false), 10);
        assert_eq!(outcome_exit(&view, true), 0); // Already quiescent; no intent needed.
        view.evidence = EvidenceAvailability::Unavailable {
            reason: "missing".into(),
        };
        assert_eq!(outcome_exit(&view, false), 10);
        view.orchestration.state = OrchestrationState::Reconciling;
        assert_eq!(outcome_exit(&view, true), 10);
        // Only a run the daemon stopped monitoring waits for a release, not
        // one whose control is still being resolved.
        assert!(next_step(&view).is_none());
        view.orchestration.reason = Some("control_uncertain".into());
        assert!(!awaiting_release(&view));
        for reason in [
            "worker_unavailable",
            "coordinator_restarted",
            "execution_failed",
        ] {
            view.orchestration.reason = Some(reason.into());
            assert!(awaiting_release(&view));
            assert_eq!(outcome_exit(&view, true), 9);
            assert_eq!(outcome_exit(&view, false), 9);
            assert_eq!(
                next_step(&view).unwrap(),
                "playtime runs release fixture-run-00000001 --reason TEXT"
            );
        }
    }
}
