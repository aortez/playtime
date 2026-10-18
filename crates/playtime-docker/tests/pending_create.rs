//! The daemon can publish a create after the local CLI has already been reaped.
//! These synthetic schedules deliberately differ from mutate-then-lose-reply.
mod support;
use playtime_docker::{transport::*, *};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use support::*;
use tokio::time::Instant;

struct DeferredCreate {
    inner: Arc<Fake>,
    role: Role,
    reply: Option<Outcome>,
    pending: Mutex<Option<Proposal>>,
}
impl DeferredCreate {
    fn publish(&self) {
        let p = self.pending.lock().unwrap().take().unwrap();
        let mut s = self.inner.state.lock().unwrap();
        if p.role == Role::Volume {
            s.volumes.insert(p.name.clone(), volume(&p));
        } else {
            let id = format!("{:064x}", 999);
            s.containers.insert(id.clone(), container(&p, &id, false));
        }
    }
}
#[async_trait::async_trait]
impl Transport for DeferredCreate {
    async fn execute(
        &self,
        operation: Operation,
        deadline: Instant,
        cancel: CancellationToken,
    ) -> Outcome {
        if let Operation::CreateContainer(p) | Operation::CreateVolume(p) = &operation
            && p.role == self.role
        {
            assert!(self.inner.journal.proposals.lock().unwrap().contains(p));
            self.inner
                .state
                .lock()
                .unwrap()
                .seen
                .push(operation.clone());
            if self
                .reply
                .as_ref()
                .is_none_or(|r| r.dispatch != Dispatch::NotStarted)
            {
                assert!(self.pending.lock().unwrap().replace(p.clone()).is_none());
            }
            return match &self.reply {
                Some(reply) => reply.clone(),
                None => std::future::pending().await,
            };
        }
        self.inner.execute(operation, deadline, cancel).await
    }
}
async fn sandbox(transport: Arc<DeferredCreate>) -> Sandbox {
    let engine = Engine::connect(transport.clone(), deadline(), CancellationToken::new())
        .await
        .unwrap();
    let ready = engine
        .readiness(&lock(), deadline(), CancellationToken::new())
        .await
        .unwrap();
    Sandbox::new(
        ready,
        Ownership {
            service: "synthetic-service".into(),
            run: "synthetic-run".into(),
            grant: "synthetic-grant".into(),
            invocation: "synthetic-invocation".into(),
            nonce: "0123456789abcdef0123456789abcdef".into(),
            daemon: engine.identity().clone(),
        },
        Limits::smoke(),
        WorkDeadline::after(Duration::from_secs(120)).unwrap(),
        transport.inner.journal.clone(),
    )
    .unwrap()
}
fn transport(role: Role, reply: Option<Outcome>) -> Arc<DeferredCreate> {
    Arc::new(DeferredCreate {
        inner: Arc::new(Fake {
            state: Mutex::new(State::default()),
            journal: Arc::new(Journal::default()),
        }),
        role,
        reply,
        pending: Mutex::new(None),
    })
}
async fn fail_create(sandbox: &mut Sandbox, role: Role) {
    if role == Role::Agent {
        sandbox
            .initialize(deadline(), CancellationToken::new())
            .await
            .unwrap();
        assert!(
            sandbox
                .start_session(1, deadline(), CancellationToken::new())
                .await
                .is_err()
        );
    } else {
        assert!(
            sandbox
                .initialize(deadline(), CancellationToken::new())
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn early_absence_cannot_complete_a_pending_create_or_remove_its_volume() {
    for role in [Role::Volume, Role::Holder, Role::Agent] {
        for reply in [
            lost(Fault::Io),
            lost(Fault::Cancelled),
            lost(Fault::Deadline),
            Outcome::completed(b"malformed create acknowledgement".to_vec()),
        ] {
            let transport = transport(role, Some(reply));
            let mut sandbox = sandbox(transport.clone()).await;
            fail_create(&mut sandbox, role).await;
            // Neither local reaping nor repeated immediate absence establishes
            // daemon completion. Retain the original proposal for reconciliation.
            for _ in 0..2 {
                let cleanup = sandbox.cleanup(deadline(), CancellationToken::new()).await;
                assert!(!cleanup.complete(), "{role:?}: {cleanup:?}");
                assert!(cleanup.resources.iter().any(|r| matches!(r,
                    ResourceOutcome::Residual { role: actual, .. } if *actual == role)));
                assert!(
                    !transport
                        .inner
                        .seen()
                        .iter()
                        .any(|op| matches!(op, Operation::RemoveVolume(_)))
                );
            }
            transport.publish();
            assert!(transport.inner.resources() > 0);
            // Once that exact owned resource is visible, it can be removed and
            // its absence established without ever replaying create.
            assert!(
                sandbox
                    .cleanup(deadline(), CancellationToken::new())
                    .await
                    .complete()
            );
            assert_eq!(transport.inner.resources(), 0);
            assert_eq!(
                transport
                    .inner
                    .seen()
                    .iter()
                    .filter(|op| matches!(op,
                Operation::CreateContainer(p) | Operation::CreateVolume(p) if p.role == role))
                    .count(),
                1
            );
        }
    }
}

#[tokio::test]
async fn pending_agent_create_blocks_quiescence_and_another_session() {
    let transport = transport(Role::Agent, Some(lost(Fault::Io)));
    let mut sandbox = sandbox(transport.clone()).await;
    fail_create(&mut sandbox, Role::Agent).await;
    for _ in 0..2 {
        assert!(
            sandbox
                .quiesce(deadline(), CancellationToken::new())
                .await
                .is_err()
        );
        assert!(sandbox.is_fenced());
        assert!(
            sandbox
                .start_session(2, deadline(), CancellationToken::new())
                .await
                .is_err()
        );
    }
    assert_eq!(transport.inner.resources(), 2, "retain holder and volume");
    transport.publish();
    sandbox
        .quiesce(deadline(), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(transport.inner.resources(), 2);
    assert!(
        sandbox
            .cleanup(deadline(), CancellationToken::new())
            .await
            .complete()
    );
}

#[tokio::test]
async fn dropped_create_futures_retain_pending_publication() {
    for role in [Role::Volume, Role::Holder, Role::Agent] {
        let transport = transport(role, None);
        let mut sandbox = sandbox(transport.clone()).await;
        if role == Role::Agent {
            sandbox
                .initialize(deadline(), CancellationToken::new())
                .await
                .unwrap();
        }
        // All fixture preflight calls are ready. The first branch reaches the
        // deferred mutation and stays pending; the second drops that future.
        tokio::select! {
            biased;
            _ = async {
                if role == Role::Agent {
                    sandbox.start_session(1, deadline(), CancellationToken::new()).await
                } else {
                    sandbox.initialize(deadline(), CancellationToken::new()).await
                }
            } => panic!("create must still be pending"),
            _ = std::future::ready(()) => {},
        }
        assert!(transport.pending.lock().unwrap().is_some());
        assert!(
            !sandbox
                .cleanup(deadline(), CancellationToken::new())
                .await
                .complete()
        );
        transport.publish();
        assert!(
            sandbox
                .cleanup(deadline(), CancellationToken::new())
                .await
                .complete()
        );
        assert_eq!(transport.inner.resources(), 0);
    }
}

#[tokio::test]
async fn explicit_non_dispatch_proof_does_not_leave_a_pending_create() {
    for role in [Role::Volume, Role::Holder, Role::Agent] {
        let transport = transport(
            role,
            Some(Outcome {
                dispatch: Dispatch::NotStarted,
                local: LocalExit::NotStarted,
                fault: Some(Fault::Cancelled),
                stdout: vec![],
                stderr: vec![],
            }),
        );
        let mut sandbox = sandbox(transport.clone()).await;
        fail_create(&mut sandbox, role).await;
        assert!(transport.pending.lock().unwrap().is_none());
        let cleanup = sandbox.cleanup(deadline(), CancellationToken::new()).await;
        assert!(cleanup.complete());
        assert!(cleanup.resources.iter().any(|r| matches!(r,
            ResourceOutcome::NotDispatched { role: actual, .. } if *actual == role)));
        assert_eq!(transport.inner.resources(), 0);
    }
}
