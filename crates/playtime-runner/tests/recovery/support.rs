#![allow(dead_code)]
use playtime_protocol::{
    archive::ContentRef,
    events::{EventEnvelope, decode_event},
    recovery::*,
    storage::*,
};
use playtime_runner::recovery::*;
use playtime_storage::{StoredEvent, content_ref, sha256};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};
pub const ATTEMPT: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
pub fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../playtime-protocol/tests/fixtures/recovery")
}
pub fn bytes(path: &str) -> Vec<u8> {
    fs::read(root().join(path)).unwrap()
}
pub fn stored(bytes: Vec<u8>) -> StoredEvent {
    let event = decode_event(std::str::from_utf8(&bytes).unwrap()).unwrap();
    StoredEvent {
        envelope: event.envelope().clone(),
        sha256: sha256(&bytes),
        bytes,
    }
}
pub fn event(sequence: u64, kind: &str, data: Value) -> StoredEvent {
    let envelope = EventEnvelope {
        schema_version: 1,
        run_id: playtime_core::RunId::new("recovery-synthetic").unwrap(),
        sequence: sequence.try_into().unwrap(),
        time: playtime_core::UtcTime::new("2026-10-18T00:00:00Z").unwrap(),
        kind: kind.into(),
        data,
    };
    let mut bytes = serde_json::to_vec(&envelope).unwrap();
    bytes.push(b'\n');
    stored(bytes)
}
pub struct Case {
    pub files: BTreeMap<String, Vec<u8>>,
    pub events: Vec<StoredEvent>,
    pub attempt: RecoveryAttemptV1,
    pub bounds: ConfirmedBoundsV1,
    pub base: EventFrontier,
    pub marker: Vec<u8>,
    pub inventory: ContentRef,
    pub inventory_bytes: Vec<u8>,
    pub limits: StorageLimits,
    pub begin: BeginHistory,
    pub file_bytes: u64,
}
impl PlanningContents for Case {
    fn read(&self, reference: &ContentRef, limit: u64) -> playtime_storage::Result<Vec<u8>> {
        let bytes = self.files.get(reference.path.as_str()).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "fixture content missing")
        })?;
        if bytes.len() as u64 > limit {
            return Err(std::io::Error::other("fixture read bound").into());
        }
        Ok(bytes.clone())
    }
}
impl Case {
    pub fn golden() -> Self {
        let hashes: BTreeMap<String, ContentRef> =
            serde_json::from_slice(&bytes("hashes.json")).unwrap();
        let files: BTreeMap<_, _> = hashes.keys().map(|p| (p.clone(), bytes(p))).collect();
        let plan: RecoveryPlanV1 = decode(
            &bytes(&format!("artifacts/recovery/{ATTEMPT}/plan.json")),
            4 * 1024 * 1024,
        )
        .unwrap();
        Self {
            events: files["events.jsonl"]
                .split_inclusive(|b| *b == b'\n')
                .map(|l| stored(l.to_vec()))
                .collect(),
            attempt: plan.attempt,
            bounds: plan.prior_bounds,
            marker: files["events.commit.json"].clone(),
            inventory_bytes: files[plan.initial_inventory.path.as_str()].clone(),
            inventory: plan.initial_inventory,
            limits: StorageLimits::default(),
            begin: BeginHistory::AuthorizedOrUncertain,
            file_bytes: plan.base.committed_bytes,
            base: plan.base,
            files,
        }
    }
    pub fn input(&self) -> PlanningInput<'_> {
        PlanningInput {
            attempt: &self.attempt,
            acquired_generation: 2,
            base: &self.base,
            confirmed: &self.bounds,
            descriptor_bytes: &self.files["archive.json"],
            manifest_bytes: &self.files["inputs/manifest.json"],
            marker_bytes: &self.marker,
            initial_inventory: &self.inventory,
            inventory_bytes: &self.inventory_bytes,
            inventory_entries: 0,
            event_file_bytes: self.file_bytes,
            begin_history: self.begin,
            limits: &self.limits,
            contents: self,
        }
    }
    pub fn decision(&self) -> RecoveryDecision {
        plan_recovery(&self.input(), self.events.iter().cloned().map(Ok)).unwrap()
    }
    pub fn replace_events(&mut self, events: Vec<StoredEvent>) {
        self.events = events;
        let bytes: Vec<_> = self.events.iter().flat_map(|e| e.bytes.clone()).collect();
        self.base.committed_sequence = self.events.len() as u64;
        self.base.committed_bytes = bytes.len() as u64;
        self.base.prefix_sha256 = sha256(&bytes);
        let marker = CommitMarker {
            schema_version: 1,
            run_id: self.base.archive.run_id.clone(),
            committed_sequence: self.base.committed_sequence,
            committed_bytes: self.base.committed_bytes,
            prefix_sha256: self.base.prefix_sha256.clone(),
        };
        self.marker = serde_json::to_vec(&marker).unwrap();
        self.base.marker = content_ref(
            playtime_core::ArchivePath::new("events.commit.json").unwrap(),
            &self.marker,
        );
        self.bounds = ConfirmedBoundsV1 {
            schema_version: 1,
            worker: None,
            public: None,
        };
        self.file_bytes = bytes.len() as u64;
        self.inventory_bytes=serde_json::to_vec(&json!({"schema_version":1,"attempt_id":ATTEMPT,"archive":self.base.archive,"base":self.base,"entries":[]})).unwrap();
        self.inventory = content_ref(self.inventory.path.clone(), &self.inventory_bytes);
    }
    pub fn facts(&mut self, facts: &Value) {
        let mut events = vec![self.events[0].clone()];
        for fact in facts.as_array().unwrap() {
            let text = |i: usize| fact[i].as_str().unwrap();
            let number = |i: usize| fact[i].as_u64().unwrap_or_else(|| text(i).parse().unwrap());
            let (kind, data) = match text(0) {
                "intent" => (
                    "effect_intent",
                    json!({"schema_version":1,"effect_id":number(1),"effect_kind":"model","session_id":text(2),"request_id":text(3),"tool_request_sequence":null,"maximum_tokens":number(4),"deadline_ms":1000,"sources":[]}),
                ),
                "known" => (
                    "request_finished",
                    json!({"session_id":text(1),"request_id":text(2),"usage":{"status":"known","generated_tokens":number(3)}}),
                ),
                "unknown" => (
                    "request_finished",
                    json!({"session_id":text(1),"request_id":text(2),"usage":{"status":"unknown","reserved_tokens":number(3)}}),
                ),
                "request_not_dispatched" => (
                    "request_not_dispatched",
                    json!({"schema_version":1,"effect_id":number(1),"session_id":text(2),"request_id":text(3),"released_tokens":number(4)}),
                ),
                "effect_not_dispatched" => (
                    "effect_not_dispatched",
                    json!({"schema_version":1,"effect_id":number(1),"session_id":text(2),"identity":{"effect_id":number(1),"request_id":text(3),"tool_request_sequence":null}}),
                ),
                _ => panic!("unknown review fact"),
            };
            events.push(event(events.len() as u64 + 1, kind, data));
        }
        self.replace_events(events);
    }
    pub fn final_plan(&self) -> RecoveryPlanV1 {
        match self.decision() {
            RecoveryDecision::Finalize(plan) => *plan,
            other => panic!("expected finalization, got {other:?}"),
        }
    }
    pub fn change_contract(&mut self, edit: impl FnOnce(&mut Value)) {
        let mut contract: Value = serde_json::from_slice(&self.files[EXECUTION_CONTRACT]).unwrap();
        edit(&mut contract);
        let bytes = serde_json::to_vec(&contract).unwrap();
        let mut manifest: playtime_protocol::archive::InputManifest =
            serde_json::from_slice(&self.files["inputs/manifest.json"]).unwrap();
        *manifest
            .files
            .iter_mut()
            .find(|r| r.path.as_str() == EXECUTION_CONTRACT)
            .unwrap() = content_ref(
            playtime_core::ArchivePath::new(EXECUTION_CONTRACT).unwrap(),
            &bytes,
        );
        self.files.insert(EXECUTION_CONTRACT.into(), bytes);
        self.rebind_manifest(manifest);
    }
    pub fn rebind_manifest(&mut self, manifest: playtime_protocol::archive::InputManifest) {
        let bytes = serde_json::to_vec(&manifest).unwrap();
        self.base.archive.input_manifest =
            content_ref(self.base.archive.input_manifest.path.clone(), &bytes);
        self.files.insert("inputs/manifest.json".into(), bytes);
        let mut descriptor: ArchiveDescriptor =
            serde_json::from_slice(&self.files["archive.json"]).unwrap();
        descriptor.input_manifest = self.base.archive.input_manifest.clone();
        let bytes = serde_json::to_vec(&descriptor).unwrap();
        self.base.archive.descriptor =
            content_ref(self.base.archive.descriptor.path.clone(), &bytes);
        self.files.insert("archive.json".into(), bytes);
        self.attempt.binding.archive = self.base.archive.clone();
        self.attempt.binding.worker.manifest_sha256 =
            self.base.archive.input_manifest.sha256.clone();
        let mut events = self.events.clone();
        if let Some(first) = events.first_mut() {
            first.envelope.data["input_manifest"] =
                serde_json::to_value(&self.base.archive.input_manifest).unwrap();
            let mut bytes = serde_json::to_vec(&first.envelope).unwrap();
            bytes.push(b'\n');
            *first = stored(bytes);
        }
        self.replace_events(events);
    }
}
pub fn blocked(decision: RecoveryDecision, expected: RecoveryBlockedReasonV1) {
    match decision {
        RecoveryDecision::Blocked { reason, .. } => assert_eq!(reason, expected),
        other => panic!("expected {expected:?}, got {other:?}"),
    }
}
