use playtime_backend_guard::{
    Dispatch, Guard, Journal, capability, digest, ports::*, random_id, wire::*,
};
use playtime_backend_host::{
    deadline::Register,
    ollama::{ManagerAuthority, OllamaBackend, validate_body},
};
use playtime_ollama as o1;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Clone)]
struct Transport {
    pending: Arc<Mutex<VecDeque<o1::HttpResponse>>>,
    seen: Arc<Mutex<Vec<o1::Request>>>,
}
#[async_trait::async_trait]
impl o1::Transport for Transport {
    async fn send(&self, r: o1::Request) -> o1::Result<o1::HttpResponse> {
        self.seen.lock().unwrap().push(r);
        Ok(self
            .pending
            .lock()
            .unwrap()
            .pop_front()
            .expect("no retries or implicit extra metadata"))
    }
}
fn response(body: Vec<u8>) -> o1::HttpResponse {
    o1::HttpResponse {
        status: 200,
        content_encoding: None,
        body: Box::pin(futures_util::stream::iter(
            body.chunks(7).map(|p| Ok(p.to_vec())).collect::<Vec<_>>(),
        )),
    }
}
struct Authority(BackendIdentity);
impl BackendAuthority for Authority {
    fn identity(&self) -> &BackendIdentity {
        &self.0
    }
}
struct Ports {
    identity: BackendIdentity,
    register: Mutex<Register>,
}
#[async_trait::async_trait]
impl ManagerAuthority for Ports {
    async fn verify(
        &self,
        expected: &BackendIdentity,
    ) -> playtime_backend_guard::Result<VerifiedBackend> {
        assert_eq!(expected, &self.identity);
        VerifiedBackend::from_trusted_port(Arc::new(Authority(self.identity.clone())))
    }
}
#[async_trait::async_trait]
impl Supervisor for Ports {
    async fn stop(&self, _: &StopOperation) -> playtime_backend_guard::Result<ManagerObservation> {
        Ok(ManagerObservation::Running {})
    }
    async fn terminated(
        &self,
        _: &StopOperation,
    ) -> playtime_backend_guard::Result<Option<HeldTermination>> {
        Ok(None)
    }
}
#[async_trait::async_trait]
impl Enforcement for Ports {
    async fn select_stop(
        &self,
        op: &StopOperation,
    ) -> playtime_backend_guard::Result<StopOperation> {
        self.register
            .lock()
            .unwrap()
            .select_stop(op.reason, SystemClock::new()?.now().ms, Some(op))?
            .ok_or(Failure::Blocked)
    }
    async fn arm_lease(&self, l: &LeaseSpec) -> playtime_backend_guard::Result<()> {
        self.register
            .lock()
            .unwrap()
            .arm_lease(l, SystemClock::new()?.now().ms)
    }
    async fn arm_request(&self, r: &RequestSpec) -> playtime_backend_guard::Result<()> {
        self.register
            .lock()
            .unwrap()
            .arm_request(r, SystemClock::new()?.now().ms)
    }
    async fn terminal(&self, r: &RequestSpec) -> playtime_backend_guard::Result<()> {
        self.register
            .lock()
            .unwrap()
            .terminal(r, SystemClock::new()?.now().ms)
    }
    async fn release(
        &self,
        l: &playtime_protocol::worker::ControlId,
    ) -> playtime_backend_guard::Result<()> {
        self.register.lock().unwrap().release(l, false)
    }
}
fn body() -> Vec<u8> {
    br#"{"keep_alive":"5m","messages":[{"content":"test","role":"user"}],"model":"gpt-oss:20b","options":{"num_ctx":32768,"num_predict":32,"seed":1},"stream":true,"think":"low","tools":[]}"#.to_vec()
}
fn request(bytes: &[u8], lease: &LeaseSpec) -> RequestSpec {
    RequestSpec {
        lease_id: lease.lease_id.clone(),
        generation: 1,
        ordinal: 1,
        request_id: random_id().unwrap(),
        effect_id: random_id().unwrap(),
        body_sha256: digest(bytes),
        body_bytes: bytes.len() as u64,
        original_max_tokens: 32,
        deadline_ms: lease.execution_deadline_ms,
    }
}
fn identity() -> BackendIdentity {
    BackendIdentity {
        backend_id: random_id().unwrap(),
        guard_id: random_id().unwrap(),
        boot_id: SystemClock::new().unwrap().now().boot_id,
        unit: "fixture.service".into(),
        invocation_id: random_id().unwrap(),
        cgroup: "/fixture".into(),
        generation: 1,
        profile_sha256: digest(b"profile"),
    }
}
fn lease(identity: BackendIdentity) -> LeaseSpec {
    let now = SystemClock::new().unwrap().now().ms;
    LeaseSpec {
        operation_id: random_id().unwrap(),
        lease_id: random_id().unwrap(),
        backend: identity,
        service_id: random_id().unwrap(),
        run_id: playtime_core::RunId::new("native-fixture").unwrap(),
        manifest_sha256: digest(b"m"),
        worker_id: random_id().unwrap(),
        grant_id: random_id().unwrap(),
        owner_revision: 1,
        execution_deadline_ms: now + 10_000,
        cleanup_deadline_ms: now + 190_000,
    }
}

#[test]
fn fixed_request_rejects_management_settings_allowance_duplicates_and_noncanonical_bytes() {
    let bytes = body();
    let l = lease(identity());
    let spec = request(&bytes, &l);
    let settings = o1::EffectiveSettings::default();
    validate_body(&bytes, &spec, &settings).unwrap();
    for (from, to) in [
        ("gpt-oss:20b", "other:model"),
        ("\"stream\":true", "\"stream\":false"),
        ("\"num_predict\":32", "\"num_predict\":33"),
        ("\"keep_alive\":\"5m\"", "\"keep_alive\":0"),
        ("\"seed\":1", "\"seed\":1,\"seed\":1"),
        ("\"tools\":[]", "\"tools\":[],\"url\":\"http://evil\""),
    ] {
        let changed = String::from_utf8(bytes.clone())
            .unwrap()
            .replace(from, to)
            .into_bytes();
        let mut bad = spec.clone();
        bad.body_bytes = changed.len() as u64;
        bad.body_sha256 = digest(&changed);
        assert!(validate_body(&changed, &bad, &settings).is_err(), "{from}");
    }
    let mut noncanonical = bytes.clone();
    noncanonical.push(b' ');
    let mut bad = spec;
    bad.body_bytes = noncanonical.len() as u64;
    bad.body_sha256 = digest(&noncanonical);
    assert!(validate_body(&noncanonical, &bad, &settings).is_err());
}

#[tokio::test]
async fn prepared_metadata_exact_bytes_partial_output_terminal_and_no_replay() {
    for (tail, success) in [
        (
            "{\"model\":\"gpt-oss:20b\",\"done\":true,\"done_reason\":\"stop\",\"eval_count\":2,\"prompt_eval_count\":3}\n",
            true,
        ),
        (
            "{\"model\":\"gpt-oss:20b\",\"done\":true,\"done_reason\":\"stop\"}\n",
            false,
        ),
        ("{broken", false),
        ("", false),
    ] {
        let lock =
            o1::ModelLock::parse(include_bytes!("../../../models/gpt-oss-20b.lock.json")).unwrap();
        let partial=b"{\"model\":\"gpt-oss:20b\",\"message\":{\"role\":\"assistant\",\"content\":\"visible\"},\"done\":false}\n";
        let stream = [partial.as_slice(), tail.as_bytes()].concat();
        let metadata = [
            serde_json::json!({"version":o1::Profile::VERSION}),
            serde_json::json!({"models":[{"name":o1::Profile::MODEL,"digest":o1::Profile::DIGEST}]}),
            lock.definition.clone(),
        ];
        let transport = Transport {
            pending: Arc::new(Mutex::new(
                metadata
                    .into_iter()
                    .map(|v| response(serde_json::to_vec(&v).unwrap()))
                    .chain([response(stream.clone())])
                    .collect(),
            )),
            seen: Arc::default(),
        };
        let identity = identity();
        let ports = Arc::new(Ports {
            identity: identity.clone(),
            register: Mutex::new(Register::new(identity.clone())),
        });
        let backend = OllamaBackend::prepare(
            transport.clone(),
            lock,
            identity.clone(),
            o1::EffectiveSettings::default(),
            ports.clone(),
            Arc::new(SystemClock::new().unwrap()),
        )
        .await
        .unwrap();
        assert_eq!(
            transport.seen.lock().unwrap().len(),
            3,
            "preparation only fixed metadata allowlist"
        );
        let root =
            std::env::temp_dir().join(format!("pt-native-{}", random_id().unwrap().as_str()));
        let guard = Guard::start_enforced(
            Journal::create(&root).unwrap(),
            Some(identity.clone()),
            Arc::new(backend),
            ports.clone(),
            Arc::new(SystemClock::new().unwrap()),
            Some(ports.clone()),
        )
        .await
        .unwrap();
        let lease = lease(identity);
        let cap = capability().unwrap();
        guard.acquire(lease.clone(), cap.clone()).await.unwrap();
        let bytes = body();
        let spec = request(&bytes, &lease);
        let Dispatch::Stream(mut owner) = guard
            .dispatch(spec.clone(), cap.clone(), bytes.clone())
            .await
            .unwrap()
        else {
            panic!("new stream")
        };
        let mut observed = vec![];
        tokio::time::timeout(Duration::from_secs(2), async {
            while let Some(frame) = owner.frames.recv().await {
                observed.extend(frame.bytes);
            }
        })
        .await
        .unwrap();
        while owner.completion.borrow().is_none() {
            owner.completion.changed().await.unwrap();
        }
        assert_eq!(owner.completion.borrow().as_ref().unwrap().is_ok(), success);
        assert!(observed.starts_with(partial));
        if success {
            assert_eq!(observed, stream);
        }
        let retry = guard.dispatch(spec, cap, bytes.clone()).await.unwrap();
        assert!(matches!(retry, Dispatch::Existing(_)));
        let seen = transport.seen.lock().unwrap();
        assert_eq!(seen.len(), 4);
        assert_eq!(seen[3].operation, o1::Operation::Chat);
        assert_eq!(seen[3].body.as_ref().unwrap(), &bytes);
        drop(seen);
        drop(owner);
        drop(guard);
        // Failed requests deliberately retain ownership; keep private evidence
        // until process exit rather than racing the detached stop writer.
    }
}
