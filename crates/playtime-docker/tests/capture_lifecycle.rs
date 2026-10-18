mod support;
use playtime_docker::{capture::*, transport::*, *};
use playtime_protocol::storage::StorageLimits;
use std::{sync::Arc, time::Duration};
use support::*;
use tokio::time::Instant;
fn profile() -> CaptureProfile {
    CaptureProfile::prepare(
        CaptureLimits::default(),
        8 << 20,
        &StorageLimits::default(),
        ArchiveUsage::default(),
    )
    .unwrap()
}
#[tokio::test]
async fn candidate_requires_complete_transport_and_the_original_quiescence_bound() {
    let mut f = Fixture::running().await;
    let stop = Instant::now() + Duration::from_secs(30);
    let candidate = {
        let mut q = f
            .sandbox
            .quiesce(stop, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(q.deadline(), stop);
        q.capture(&profile(), CancellationToken::new())
            .await
            .unwrap()
    };
    assert_eq!(candidate.workspace().entries().len(), 6);
    assert_eq!(candidate.memory().status, "missing");
    assert_eq!(candidate.helpers(), &helper_digests());
    assert!(!f.sandbox.is_fenced());
    let seen = f.fake.seen();
    let remove = seen
        .iter()
        .position(|o| matches!(o, Operation::RemoveContainer(_)))
        .unwrap();
    let export = seen
        .iter()
        .position(|o| matches!(o, Operation::Export { .. }))
        .unwrap();
    assert!(remove < export);
    assert_eq!(f.fake.state.lock().unwrap().deadlines[export], stop);
    f.sandbox
        .start_session(2, deadline(), CancellationToken::new())
        .await
        .unwrap();
    assert!(f.cleanup().await.complete());
}
#[tokio::test]
async fn failed_helpers_cannot_make_candidates_or_clear_uncertain_exporters() {
    for operation in ["workspace_export", "outgoing_memory"] {
        for injection in [
            Injection::Lost(Fault::Cancelled),
            Injection::Lost(Fault::Deadline),
            Injection::Lost(Fault::Io),
            Injection::Malformed,
        ] {
            let uncertain = matches!(injection, Injection::Lost(_));
            let mut f = Fixture::running().await;
            f.fake.inject(operation, injection);
            {
                let mut q = f
                    .sandbox
                    .quiesce(deadline(), CancellationToken::new())
                    .await
                    .unwrap();
                assert!(
                    q.capture(&profile(), CancellationToken::new())
                        .await
                        .is_err()
                );
            }
            assert!(f.sandbox.is_fenced());
            assert!(
                f.sandbox
                    .start_session(2, deadline(), CancellationToken::new())
                    .await
                    .is_err()
            );
            if uncertain {
                assert!(
                    f.sandbox
                        .quiesce(deadline(), CancellationToken::new())
                        .await
                        .is_err()
                );
            }
            // Holder stays mounted on every capture failure until explicit final cleanup.
            assert_eq!(f.fake.state.lock().unwrap().containers.len(), 1);
            assert_eq!(f.fake.state.lock().unwrap().volumes.len(), 1);
            assert!(f.cleanup().await.complete());
        }
    }
}
struct Pending {
    fake: Arc<Fake>,
    operation: &'static str,
}
#[async_trait::async_trait]
impl Transport for Pending {
    async fn execute(&self, op: Operation, d: Instant, c: CancellationToken) -> Outcome {
        if op.name() == self.operation {
            std::future::pending().await
        } else {
            self.fake.execute(op, d, c).await
        }
    }
}
#[tokio::test]
async fn dropping_capture_fences_holder_and_retains_volume_until_exact_cleanup() {
    for operation in ["workspace_export", "outgoing_memory"] {
        let f = Fixture::new().await;
        let engine = Engine::connect(
            Arc::new(Pending {
                fake: f.fake.clone(),
                operation,
            }),
            deadline(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let ready = engine
            .readiness(&lock(), deadline(), CancellationToken::new())
            .await
            .unwrap();
        let owner = Ownership {
            service: "capture".into(),
            run: "synthetic".into(),
            grant: "synthetic".into(),
            invocation: "synthetic".into(),
            nonce: Ownership::random_nonce().unwrap(),
            daemon: engine.identity().clone(),
        };
        let mut sandbox = Sandbox::new(
            ready,
            owner,
            Limits::smoke(),
            WorkDeadline::after(Duration::from_secs(120)).unwrap(),
            f.journal.clone(),
        )
        .unwrap();
        sandbox
            .initialize(deadline(), CancellationToken::new())
            .await
            .unwrap();
        sandbox
            .start_session(1, deadline(), CancellationToken::new())
            .await
            .unwrap();
        {
            let mut q = sandbox
                .quiesce(deadline(), CancellationToken::new())
                .await
                .unwrap();
            let p = profile();
            tokio::select! {biased; _=q.capture(&p,CancellationToken::new())=>panic!("helper should be pending"), _=std::future::ready(())=>{}}
        }
        assert!(
            sandbox
                .quiesce(deadline(), CancellationToken::new())
                .await
                .is_err()
        );
        assert!(
            sandbox
                .start_session(2, deadline(), CancellationToken::new())
                .await
                .is_err()
        );
        assert_eq!(f.fake.state.lock().unwrap().containers.len(), 1);
        assert!(
            sandbox
                .cleanup(deadline(), CancellationToken::new())
                .await
                .complete()
        );
    }
}
#[tokio::test]
async fn expired_or_cancelled_or_conflicting_mount_cannot_capture() {
    let mut f = Fixture::running().await;
    {
        let mut q = f
            .sandbox
            .quiesce(deadline(), CancellationToken::new())
            .await
            .unwrap();
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        assert!(q.capture(&profile(), cancelled).await.is_err());
    }
    assert!(
        !f.fake
            .seen()
            .iter()
            .any(|op| matches!(op, Operation::Export { .. }))
    );
    assert!(f.cleanup().await.complete());
    let mut f = Fixture::running().await;
    {
        let mut q = f
            .sandbox
            .quiesce(
                Instant::now() + Duration::from_millis(5),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(
            q.capture(&profile(), CancellationToken::new())
                .await
                .is_err()
        );
    }
    assert!(f.cleanup().await.complete());
    let mut f = Fixture::running().await;
    {
        let mut q = f
            .sandbox
            .quiesce(deadline(), CancellationToken::new())
            .await
            .unwrap();
        {
            let mut state = f.fake.state.lock().unwrap();
            let (_, other) = state.containers.first_key_value().unwrap();
            let other = other.clone();
            state.containers.insert("b".repeat(64), other);
        }
        assert!(
            q.capture(&profile(), CancellationToken::new())
                .await
                .is_err()
        );
    }
    assert!(
        !f.fake
            .seen()
            .iter()
            .any(|op| matches!(op, Operation::Export { .. }))
    );
    // Synthetic unrelated mount remains, never broadly deleted.
    assert!(!f.cleanup().await.complete());
}
#[test]
fn helpers_are_literal_isolated_argv_with_limits_only_on_stdin() {
    for op in [
        Operation::Export {
            id: "a".repeat(64),
            profile: profile(),
        },
        Operation::Memory {
            id: "a".repeat(64),
            limit: 8,
        },
    ] {
        let argv = op.argv();
        assert_eq!(
            &argv[..7],
            [
                "container",
                "exec",
                "--user",
                "1000:1000",
                "--workdir",
                "/",
                "--interactive"
            ]
        );
        assert_eq!(&argv[8..13], [PYTHON, "-I", "-S", "-B", "-c"]);
        assert!(argv[13] == EXPORTER || argv[13] == MEMORY_READER);
    }
}

struct CancelAt {
    fake: Arc<Fake>,
    operation: &'static str,
}
#[async_trait::async_trait]
impl Transport for CancelAt {
    async fn execute(&self, op: Operation, d: Instant, c: CancellationToken) -> Outcome {
        let at = op.name() == self.operation;
        let result = self.fake.execute(op, d, c.clone()).await;
        if at {
            c.cancel();
        }
        result
    }
}
#[tokio::test]
async fn cancellation_after_export_or_memory_never_returns_a_candidate() {
    for operation in ["workspace_export", "outgoing_memory"] {
        let f = Fixture::new().await;
        let engine = Engine::connect(
            Arc::new(CancelAt {
                fake: f.fake.clone(),
                operation,
            }),
            deadline(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let ready = engine
            .readiness(&lock(), deadline(), CancellationToken::new())
            .await
            .unwrap();
        let owner = Ownership {
            service: "capture".into(),
            run: "synthetic".into(),
            grant: "synthetic".into(),
            invocation: "synthetic".into(),
            nonce: Ownership::random_nonce().unwrap(),
            daemon: engine.identity().clone(),
        };
        let mut sandbox = Sandbox::new(
            ready,
            owner,
            Limits::smoke(),
            WorkDeadline::after(Duration::from_secs(120)).unwrap(),
            f.journal.clone(),
        )
        .unwrap();
        sandbox
            .initialize(deadline(), CancellationToken::new())
            .await
            .unwrap();
        sandbox
            .start_session(1, deadline(), CancellationToken::new())
            .await
            .unwrap();
        {
            let mut q = sandbox
                .quiesce(deadline(), CancellationToken::new())
                .await
                .unwrap();
            assert_eq!(
                q.capture(&profile(), CancellationToken::new())
                    .await
                    .unwrap_err()
                    .failure,
                Failure::Cancelled
            );
        }
        if operation == "workspace_export" {
            assert!(
                !f.fake
                    .seen()
                    .iter()
                    .any(|op| matches!(op, Operation::Memory { .. }))
            );
        }
        assert!(
            sandbox
                .cleanup(deadline(), CancellationToken::new())
                .await
                .complete()
        );
    }
}
