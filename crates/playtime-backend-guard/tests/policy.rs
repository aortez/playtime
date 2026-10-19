mod common;
use common::{identity, lease, private, request};
use playtime_backend_guard::{
    Dispatch, Guard, Journal, Result, capability, ports::*, random_id, wire::*,
};
use playtime_protocol::worker::ControlId;
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio_util::sync::CancellationToken;

struct VirtualClock {
    boot: Mutex<ControlId>,
    ms: AtomicU64,
}
impl Clock for VirtualClock {
    fn now(&self) -> Stamp {
        Stamp {
            boot_id: self.boot.lock().unwrap().clone(),
            ms: self.ms.load(Ordering::SeqCst),
        }
    }
}
struct Evidence {
    identity: BackendIdentity,
    operation: ControlId,
}
impl BackendAuthority for Evidence {
    fn identity(&self) -> &BackendIdentity {
        &self.identity
    }
}
impl TerminationAuthority for Evidence {
    fn operation_id(&self) -> &ControlId {
        &self.operation
    }
}
struct Port {
    identity: Mutex<BackendIdentity>,
    sends: AtomicUsize,
    stops: Mutex<Vec<StopOperation>>,
    terminal: AtomicBool,
    terminated: AtomicBool,
    wrong_proof: AtomicBool,
    frame_bytes: AtomicUsize,
    frames: AtomicUsize,
}
#[async_trait::async_trait]
impl Backend for Port {
    async fn verify_read_only(&self, expected: &BackendIdentity) -> Result<VerifiedBackend> {
        if expected != &*self.identity.lock().unwrap() {
            return Err(Failure::Blocked);
        }
        VerifiedBackend::from_trusted_port(Arc::new(Evidence {
            identity: expected.clone(),
            operation: random_id()?,
        }))
    }
    async fn execute(
        &self,
        backend: BackendIdentity,
        request: RequestSpec,
        _: Vec<u8>,
        mut sink: ObservationSink,
        cancel: CancellationToken,
    ) -> Result<VerifiedTerminal> {
        self.sends.fetch_add(1, Ordering::SeqCst);
        for _ in 0..self.frames.load(Ordering::SeqCst) {
            sink.send(vec![b'x'; self.frame_bytes.load(Ordering::SeqCst)])
                .await?;
        }
        if !self.terminal.load(Ordering::SeqCst) {
            cancel.cancelled().await;
            return Err(Failure::Blocked);
        }
        Ok(VerifiedTerminal::from_trusted_port(backend, request))
    }
}
#[async_trait::async_trait]
impl Supervisor for Port {
    async fn stop(&self, operation: &StopOperation) -> Result<ManagerObservation> {
        if operation.backend != *self.identity.lock().unwrap() {
            return Ok(ManagerObservation::IdentityMismatch {});
        }
        self.stops.lock().unwrap().push(operation.clone());
        Ok(ManagerObservation::Submitted {
            job_id: operation.operation_id.clone(),
        })
    }
    async fn terminated(&self, operation: &StopOperation) -> Result<Option<HeldTermination>> {
        if !self.terminated.load(Ordering::SeqCst) {
            return Ok(None);
        }
        let mut identity = self.identity.lock().unwrap().clone();
        if self.wrong_proof.load(Ordering::SeqCst) {
            identity.invocation_id = random_id()?;
        }
        Ok(Some(HeldTermination::from_supervisor(Arc::new(Evidence {
            identity,
            operation: operation.operation_id.clone(),
        }))))
    }
}
struct Test {
    root: PathBuf,
    guard: Guard,
    port: Arc<Port>,
    clock: Arc<VirtualClock>,
    lease: LeaseSpec,
    cap: Capability,
}
impl Test {
    async fn new(cleanup: u64) -> Self {
        let root =
            std::env::temp_dir().join(format!("gp-{}", &random_id().unwrap().as_str()[..12]));
        private(&root);
        let identity = identity();
        let clock = Arc::new(VirtualClock {
            boot: Mutex::new(identity.boot_id.clone()),
            ms: AtomicU64::new(1_000),
        });
        let port = Arc::new(Port {
            identity: Mutex::new(identity.clone()),
            sends: AtomicUsize::new(0),
            stops: Mutex::new(Vec::new()),
            terminal: AtomicBool::new(false),
            terminated: AtomicBool::new(false),
            wrong_proof: AtomicBool::new(false),
            frame_bytes: AtomicUsize::new(1),
            frames: AtomicUsize::new(0),
        });
        let guard = Guard::start(
            Journal::create(&root.join("state")).unwrap(),
            Some(identity.clone()),
            port.clone(),
            port.clone(),
            clock.clone(),
        )
        .await
        .unwrap();
        let mut lease = lease(&identity);
        lease.execution_deadline_ms = 10_000;
        lease.cleanup_deadline_ms = cleanup;
        let cap = capability().unwrap();
        guard.acquire(lease.clone(), cap.clone()).await.unwrap();
        Self {
            root,
            guard,
            port,
            clock,
            lease,
            cap,
        }
    }
    async fn dispatch(&self) -> playtime_backend_guard::Dispatch {
        self.guard
            .dispatch(request(&self.lease, 1), self.cap.clone(), b"x".to_vec())
            .await
            .unwrap()
    }
    async fn until(&self, predicate: impl Fn(&Status) -> bool) -> Status {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let status = self.guard.status();
                if predicate(&status) {
                    return status;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap()
    }
}
impl Drop for Test {
    fn drop(&mut self) {
        self.clock.ms.store(1_000_000, Ordering::SeqCst);
        self.guard.tick();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[tokio::test]
#[ignore = "guard gate: ./scripts/test-rust-backend-guard.sh"]
async fn virtual_clock_proves_fixed_ninety_and_one_twenty_second_bounds() {
    for cleanup in [200_000, 20_000] {
        let t = Test::new(cleanup).await;
        let stream = t.dispatch().await;
        t.clock.ms.store(10_000, Ordering::SeqCst);
        t.guard.tick();
        let stopped = t.until(|s| s.observation.is_some()).await;
        let stop = stopped.stop.unwrap();
        assert_eq!(stop.reason, StopReason::Deadline);
        assert_eq!(stop.decided_ms, 10_000);
        assert_eq!(stop.supervisor_deadline_ms, 100_000.min(cleanup));
        assert_eq!(stop.observation_deadline_ms, 130_000.min(cleanup));
        t.clock
            .ms
            .store(stop.observation_deadline_ms + 1, Ordering::SeqCst);
        t.guard.tick();
        t.guard
            .revoke(
                t.lease.lease_id.clone(),
                t.cap.clone(),
                random_id().unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(t.guard.status().stop, Some(stop));
        assert!(!t.guard.status().termination_recorded);
        assert!(
            t.guard
                .release(
                    t.lease.lease_id.clone(),
                    t.cap.clone(),
                    random_id().unwrap()
                )
                .await
                .is_err()
        );
        drop(stream);
    }
}

#[tokio::test]
#[ignore = "guard gate: ./scripts/test-rust-backend-guard.sh"]
async fn replacement_generation_requires_held_exit_and_revokes_old_capabilities() {
    let t = Test::new(200_000).await;
    let old_request = request(&t.lease, 1);
    let stream = t
        .guard
        .dispatch(old_request.clone(), t.cap.clone(), b"x".to_vec())
        .await
        .unwrap();
    t.port.terminated.store(true, Ordering::SeqCst);
    t.guard
        .revoke(
            t.lease.lease_id.clone(),
            t.cap.clone(),
            random_id().unwrap(),
        )
        .await
        .unwrap();
    t.until(|s| s.termination_recorded).await;
    t.guard
        .release(
            t.lease.lease_id.clone(),
            t.cap.clone(),
            random_id().unwrap(),
        )
        .await
        .unwrap();
    let mut replacement = t.lease.backend.clone();
    replacement.generation = 2;
    replacement.invocation_id = random_id().unwrap();
    *t.port.identity.lock().unwrap() = replacement.clone();
    let verified = t.port.verify_read_only(&replacement).await.unwrap();
    t.guard.prepare_generation(verified).await.unwrap();
    assert!(
        t.guard
            .dispatch(old_request, t.cap.clone(), b"x".to_vec())
            .await
            .is_err()
    );
    assert!(
        t.guard
            .dispatch(request(&t.lease, 2), t.cap.clone(), b"x".to_vec())
            .await
            .is_err()
    );
    assert!(
        t.guard
            .acquire(t.lease.clone(), t.cap.clone())
            .await
            .unwrap()
            .lease
            .unwrap()
            .released
    );
    let mut next = lease(&replacement);
    next.execution_deadline_ms = 10_000;
    next.cleanup_deadline_ms = 200_000;
    t.guard
        .acquire(next.clone(), capability().unwrap())
        .await
        .unwrap();
    assert_eq!(t.guard.status().lease.unwrap().spec, next);
    drop(stream);
}

#[tokio::test]
#[ignore = "guard gate: ./scripts/test-rust-backend-guard.sh"]
async fn stale_boot_replacement_and_unsupported_termination_never_release() {
    for wrong_boot in [true, false] {
        let t = Test::new(200_000).await;
        let stream = t.dispatch().await;
        if wrong_boot {
            *t.clock.boot.lock().unwrap() = random_id().unwrap();
        } else {
            t.port.wrong_proof.store(true, Ordering::SeqCst);
            t.port.terminated.store(true, Ordering::SeqCst);
        }
        t.guard
            .revoke(
                t.lease.lease_id.clone(),
                t.cap.clone(),
                random_id().unwrap(),
            )
            .await
            .unwrap();
        t.until(|s| s.gate == Gate::Blocked).await;
        assert!(!t.guard.status().termination_recorded);
        assert!(
            t.guard
                .release(
                    t.lease.lease_id.clone(),
                    t.cap.clone(),
                    random_id().unwrap()
                )
                .await
                .is_err()
        );
        if wrong_boot {
            assert!(
                t.port.stops.lock().unwrap().is_empty(),
                "never target a reused unit after boot change"
            );
        }
        drop(stream);
    }
}

#[tokio::test]
#[ignore = "guard gate: ./scripts/test-rust-backend-guard.sh"]
async fn exact_binding_lengths_and_overflow_are_checked_before_mutation() {
    let t = Test::new(200_000).await;
    for mut r in [
        request(&t.lease, 0),
        request(&t.lease, 1),
        request(&t.lease, u64::MAX),
    ] {
        if r.ordinal == 1 {
            r.body_bytes = u64::MAX;
        }
        assert!(
            t.guard
                .dispatch(r, t.cap.clone(), b"x".to_vec())
                .await
                .is_err()
        );
    }
    assert!(
        t.guard
            .dispatch(request(&t.lease, 1), capability().unwrap(), b"x".to_vec())
            .await
            .is_err()
    );
    assert!(
        t.guard
            .dispatch(request(&t.lease, 1), t.cap.clone(), b"y".to_vec())
            .await
            .is_err()
    );
    let mut bad = lease(&t.lease.backend);
    bad.cleanup_deadline_ms = u64::MAX;
    assert!(t.guard.acquire(bad, capability().unwrap()).await.is_err());
    assert!(t.guard.status().request.is_none());
    assert_eq!(t.port.sends.load(Ordering::SeqCst), 0);
}

#[tokio::test]
#[ignore = "guard gate: ./scripts/test-rust-backend-guard.sh"]
async fn response_frame_total_and_forwarding_queue_are_bounded() {
    for (frame_bytes, frames, read) in [
        (FRAME_BYTES + 1, 1, true),
        (FRAME_BYTES, 9, true),
        (FRAME_BYTES, 3, false),
    ] {
        let t = Test::new(200_000).await;
        t.port.frame_bytes.store(frame_bytes, Ordering::SeqCst);
        t.port.frames.store(frames, Ordering::SeqCst);
        t.port.terminated.store(true, Ordering::SeqCst);
        let Dispatch::Stream(mut stream) = t.dispatch().await else {
            panic!("new stream expected");
        };
        if read {
            let mut bytes = 0;
            while let Some(frame) =
                tokio::time::timeout(Duration::from_secs(3), stream.frames.recv())
                    .await
                    .unwrap()
            {
                bytes += frame.bytes.len();
            }
            assert!(bytes <= RESPONSE_BYTES);
        } else {
            // The third frame cannot enter the 1 MiB queue. Advancing the one
            // fixed clock expires it, without sleeping for a wall-time timeout.
            t.clock.ms.store(10_000, Ordering::SeqCst);
            t.guard.tick();
        }
        t.until(|s| s.termination_recorded).await;
        assert_eq!(
            t.guard.status().request.unwrap().state,
            RequestState::MayHaveDispatched {}
        );
    }
}

#[tokio::test]
#[ignore = "guard gate: ./scripts/test-rust-backend-guard.sh"]
async fn dropping_an_old_completed_stream_does_not_cancel_the_next_request() {
    let t = Test::new(200_000).await;
    t.port.terminal.store(true, Ordering::SeqCst);
    let old = t.dispatch().await;
    t.until(|s| {
        s.request
            .as_ref()
            .is_some_and(|r| matches!(r.state, RequestState::Terminal { .. }))
            && s.gate == Gate::Ready
    })
    .await;
    t.port.terminal.store(false, Ordering::SeqCst);
    let current = t
        .guard
        .dispatch(request(&t.lease, 2), t.cap.clone(), b"x".to_vec())
        .await
        .unwrap();
    drop(old);
    assert!(t.guard.status().stop.is_none());
    assert_eq!(t.guard.status().gate, Gate::InFlight);
    t.port.terminated.store(true, Ordering::SeqCst);
    drop(current);
    t.until(|s| s.termination_recorded).await;
}
