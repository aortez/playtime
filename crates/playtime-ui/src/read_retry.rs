//! Retry only reads, preserving the caller's exact run/cursor and request.
use playtime_client::{Error, RetryPolicy};
use std::future::Future;
use tokio::time::{Instant, sleep_until, timeout_at};

pub(crate) async fn read<T, F, R, N, S>(
    policy: RetryPolicy,
    mut request: F,
    mut notify: N,
) -> Result<T, Error>
where
    F: FnMut() -> R,
    R: Future<Output = Result<T, Error>>,
    N: FnMut(bool) -> S,
    S: Future<Output = ()>,
{
    let mut deadline = None;
    let mut retries = 0;
    let mut last_waiting = None;
    let mut backoff = policy.initial_backoff.min(policy.max_backoff);
    loop {
        let result = match deadline {
            Some(deadline) => timeout_at(deadline, request())
                .await
                .map_err(|_| Error::RetryBudgetExhausted)?,
            None => request().await,
        };
        let waiting = match result {
            Ok(value) => return Ok(value),
            Err(error) if error.api_code() == Some("evidence_not_ready") => true,
            Err(error) if error.retryable() => false,
            Err(error) => return Err(error),
        };
        let until = *deadline.get_or_insert_with(|| Instant::now() + policy.max_elapsed);
        if retries >= policy.max_reconnects || Instant::now() >= until {
            return Err(Error::RetryBudgetExhausted);
        }
        if last_waiting != Some(waiting) {
            notify(waiting).await;
            last_waiting = Some(waiting);
        }
        retries += 1;
        sleep_until((Instant::now() + backoff).min(until)).await;
        if Instant::now() >= until {
            return Err(Error::RetryBudgetExhausted);
        }
        backoff = backoff.saturating_mul(2).min(policy.max_backoff);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, time::Duration};

    fn policy() -> RetryPolicy {
        RetryPolicy {
            max_elapsed: Duration::from_secs(60),
            max_reconnects: 100,
            ..Default::default()
        }
    }

    fn evidence_wait() -> Error {
        Error::Api {
            status: 409,
            response: playtime_protocol::v1::ErrorResponse {
                code: "evidence_not_ready".into(),
                message: "not published".into(),
                request_id: None,
                details: Default::default(),
            },
        }
    }

    #[tokio::test(start_paused = true)]
    async fn archive_waits_share_the_budget_and_report_transport_changes() {
        let attempts = Cell::new(0);
        let categories = std::cell::RefCell::new(Vec::new());
        let start = Instant::now();
        let result = read(
            policy(),
            || {
                attempts.set(attempts.get() + 1);
                let attempt = attempts.get();
                async move {
                    Err::<(), _>(if attempt % 3 == 0 {
                        Error::Transport
                    } else {
                        evidence_wait()
                    })
                }
            },
            |waiting| {
                categories.borrow_mut().push(waiting);
                async {}
            },
        )
        .await;
        assert!(matches!(result, Err(Error::RetryBudgetExhausted)));
        assert_eq!(start.elapsed(), Duration::from_secs(60));
        assert!(categories.borrow().starts_with(&[true, false, true]));
        let result = read(
            policy(),
            || async {
                Err::<(), _>(Error::Api {
                    status: 401,
                    response: playtime_protocol::v1::ErrorResponse {
                        code: "unauthorized".into(),
                        message: "unauthorized".into(),
                        request_id: None,
                        details: Default::default(),
                    },
                })
            },
            |_| async { panic!("authorization failure retried") },
        )
        .await;
        assert_eq!(result.unwrap_err().api_code(), Some("unauthorized"));
    }

    #[tokio::test(start_paused = true)]
    async fn bounded_outage_includes_a_hung_retry_and_detaches_promptly() {
        let attempts = Cell::new(0);
        let start = Instant::now();
        let result = read(
            policy(),
            || {
                attempts.set(attempts.get() + 1);
                let first = attempts.get() == 1;
                async move {
                    if first {
                        Err::<(), _>(Error::Transport)
                    } else {
                        std::future::pending().await
                    }
                }
            },
            |_| async {},
        )
        .await;
        assert!(matches!(result, Err(Error::RetryBudgetExhausted)));
        assert_eq!(attempts.get(), 2);
        assert_eq!(start.elapsed(), Duration::from_secs(60));

        let stop = tokio_util::sync::CancellationToken::new();
        let start = Instant::now();
        tokio::select! {
            _ = read(policy(), || async { Err::<(), _>(Error::Transport) }, |_| async {}) => panic!("retry ended before detach"),
            _ = async { tokio::time::sleep(Duration::from_millis(25)).await; stop.cancel(); stop.cancelled().await; } => (),
        }
        assert_eq!(start.elapsed(), Duration::from_millis(25));
    }

    #[tokio::test(start_paused = true)]
    async fn fatal_errors_do_not_retry_and_each_success_resets_the_budget() {
        let result = read(
            policy(),
            || async { Err::<(), _>(Error::IdentityMismatch) },
            |_| async { panic!("fatal error retried") },
        )
        .await;
        assert!(matches!(result, Err(Error::IdentityMismatch)));
        for _ in 0..2 {
            let attempts = Cell::new(0);
            let notifications = Cell::new(0);
            let value = read(
                policy(),
                || {
                    attempts.set(attempts.get() + 1);
                    let retry = attempts.get() < 15;
                    async move { if retry { Err(Error::Timeout) } else { Ok(42) } }
                },
                |_| {
                    notifications.set(notifications.get() + 1);
                    async {}
                },
            )
            .await
            .unwrap();
            assert_eq!(value, 42);
            assert_eq!(attempts.get(), 15);
            assert_eq!(notifications.get(), 1);
        }
    }
}
