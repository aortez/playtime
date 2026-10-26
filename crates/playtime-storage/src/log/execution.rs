use super::*;
use playtime_protocol::execution::{
    CaptureRef, EffectIntent, Fact, Record, RequestIdentity, decode_capture_index,
};

#[derive(Clone, Default)]
pub(super) struct ExecutionState {
    pub started: bool,
    workspace_started: bool,
    intents: BTreeMap<u64, EffectIntent>,
    admitted: BTreeMap<RequestId, (RequestIdentity, u64)>,
    settled: BTreeSet<RequestId>,
    claims: BTreeMap<RequestId, RequestIdentity>,
    capture: Option<CaptureRef>,
    model_sources: BTreeMap<RequestId, ObservationLocation>,
    compaction_plan: Option<(u64, ObservationLocation)>,
    previous_summary_request: Option<RequestId>,
}

impl LogState {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn observe_execution(
        &mut self,
        record: &Record,
        bytes: &[u8],
        descriptor: &ArchiveDescriptor,
        limits: &StorageLimits,
        directory: &Directory,
        committed: u64,
        references: &mut Vec<ContentRef>,
    ) -> Result<()> {
        if self.manifest_version != 2 {
            return Err(StorageError::malformed(
                "execution v2 requires a v2 input manifest",
            ));
        }
        if !self.execution.started && (!self.requests.is_empty() || !self.checkpoints.is_empty()) {
            return Err(StorageError::malformed(
                "execution v2 cannot follow legacy model/session evidence",
            ));
        }
        self.execution.started = true;
        match &record.fact {
            Fact::WorkspaceStarted { workspace } => {
                if self.execution.workspace_started {
                    return Err(StorageError::malformed("workspace start repeated"));
                }
                self.execution.workspace_started = true;
                self.typed_reference(workspace, ReferenceRole::Leaf, limits)?;
                references.push(workspace.clone());
            }
            Fact::EffectIntent(intent) => {
                if self.execution.intents.contains_key(&intent.effect_id.get()) {
                    return Err(StorageError::malformed("effect intent repeated"));
                }
                for source in &intent.sources {
                    self.validate_source(source, self.sequence)?;
                }
                if let Some(request) = &intent.request {
                    self.record_claim(request)?;
                }
                if intent.effect_kind == "model" {
                    let request = intent.request.as_ref().unwrap();
                    if request.call.get() != self.execution.admitted.len() as u64 + 1
                        || self.execution.admitted.contains_key(&request.request_id)
                    {
                        return Err(StorageError::malformed(
                            "model call ordinals must be global and contiguous",
                        ));
                    }
                    self.execution.admitted.insert(
                        request.request_id.clone(),
                        (request.clone(), intent.maximum_tokens.unwrap().get()),
                    );
                } else if let Some(request) = &intent.request {
                    self.validate_request(request)?;
                }
                self.execution
                    .intents
                    .insert(intent.effect_id.get(), intent.clone());
            }
            Fact::RequestSettled { request, usage } => {
                let maximum = self.validate_request(request)?;
                if matches!(usage, playtime_core::GenerationUsage::Unknown { reserved_tokens } if reserved_tokens.get() != maximum)
                {
                    return Err(StorageError::malformed(
                        "unknown usage must retain the admitted allowance",
                    ));
                }
                self.settle(request)?;
                self.usage.record(*usage).map_err(StorageError::malformed)?;
            }
            Fact::RequestNotDispatched {
                request,
                released_tokens,
            } => {
                if self.validate_request(request)? != released_tokens.get() {
                    return Err(StorageError::malformed(
                        "released allowance differs from admission",
                    ));
                }
                self.settle(request)?;
            }
            Fact::EffectNotDispatched {
                effect_id,
                request,
                tool_request_sequence,
            } => {
                let intent = self
                    .execution
                    .intents
                    .get(&effect_id.get())
                    .ok_or_else(|| StorageError::malformed("non-dispatch lacks intent"))?;
                if &intent.request != request
                    || &intent.tool_request_sequence != tool_request_sequence
                {
                    return Err(StorageError::malformed(
                        "non-dispatch identity differs from intent",
                    ));
                }
            }
            Fact::CommandFinished {
                request,
                effect_id,
                tool_request_sequence,
                ..
            } => {
                self.validate_request(request)?;
                let intent = self
                    .execution
                    .intents
                    .get(&effect_id.get())
                    .ok_or_else(|| StorageError::malformed("command lacks intent"))?;
                if intent.effect_kind != "tool"
                    || intent.request.as_ref() != Some(request)
                    || intent.tool_request_sequence != Some(*tool_request_sequence)
                {
                    return Err(StorageError::malformed(
                        "command identity differs from intent",
                    ));
                }
            }
            Fact::Observation(source) => {
                if let Some(request) = &source.request {
                    self.record_claim(request)?;
                }
                if self
                    .observations
                    .last_key_value()
                    .is_some_and(|(previous, _)| *previous >= source.controller_sequence.get())
                {
                    return Err(StorageError::malformed(
                        "observation sequences must increase",
                    ));
                }
                let value = match &source.data {
                    ObservationData::Artifact { content } => {
                        self.typed_reference(content, ReferenceRole::ObservationPayload, limits)?;
                        let value =
                            source.resolve(&directory.verify(content, limits.content_bytes)?)?;
                        references.push(content.clone());
                        value
                    }
                    ObservationData::Inline { value } => value.clone(),
                };
                let location = ObservationLocation {
                    controller_sequence: source.controller_sequence,
                    archive_sequence: std::num::NonZeroU64::new(self.sequence + 1).unwrap(),
                    record_sha256: crate::sha256(bytes),
                };
                self.validate_compaction(source, &value)?;
                match source.observation_kind.as_str() {
                    "model_request" => {
                        self.execution.model_sources.insert(
                            source.request.as_ref().unwrap().request_id.clone(),
                            location.clone(),
                        );
                    }
                    "compaction_planned" => {
                        self.execution.compaction_plan =
                            Some((value["ordinal"].as_u64().unwrap(), location.clone()));
                    }
                    "compaction_applied" => {
                        self.execution.previous_summary_request =
                            Some(source.request.as_ref().unwrap().request_id.clone());
                        self.execution.compaction_plan = None;
                    }
                    _ => {}
                }
                self.observations
                    .insert(source.controller_sequence.get(), location);
            }
            Fact::ObservationOmitted {
                request,
                observation_kind,
                ..
            } => {
                if observation_kind == "compaction_planned" {
                    self.execution.compaction_plan = None;
                }
                if let Some(request) = request {
                    self.record_claim(request)?;
                }
            }
            Fact::CapturePublished { capture } => {
                if self.execution.capture.is_some() {
                    return Err(StorageError::malformed("final capture repeated"));
                }
                self.typed_reference(&capture.index, ReferenceRole::CaptureIndex, limits)?;
                let index = decode_capture_index(utf8(
                    &directory.verify(&capture.index, limits.manifest_bytes)?,
                )?)?;
                if index.run_id != descriptor.run_id || index.workspace != capture.workspace {
                    return Err(StorageError::malformed("capture index identity mismatch"));
                }
                if index.files.len() as u64 > limits.references {
                    return Err(StorageError::limit("capture file count bound exceeded"));
                }
                self.typed_reference(&index.source_map, ReferenceRole::SourceMap, limits)?;
                let map = decode_source_map(utf8(
                    &directory.verify(&index.source_map, limits.manifest_bytes)?,
                )?)?;
                if map.run_id != descriptor.run_id {
                    return Err(StorageError::malformed("capture source map run mismatch"));
                }
                if map.entries.len() as u64 > limits.events {
                    return Err(StorageError::limit(
                        "capture source map count bound exceeded",
                    ));
                }
                for source in &map.entries {
                    self.validate_source(source, committed.min(self.sequence))?;
                }
                references.extend([capture.index.clone(), index.source_map.clone()]);
                for leaf in index.content_leaves() {
                    self.typed_reference(leaf, ReferenceRole::Leaf, limits)?;
                    references.push(leaf.clone());
                }
                self.execution.capture = Some(capture.clone());
            }
            Fact::RunFinished {
                usage,
                reason,
                last_capture,
                ..
            } => {
                if *usage != UsageSummary::from(self.usage)
                    || *last_capture != self.execution.capture
                    || self.execution.settled.len() != self.execution.admitted.len()
                    || self
                        .execution
                        .claims
                        .keys()
                        .any(|id| !self.execution.admitted.contains_key(id))
                {
                    return Err(StorageError::malformed(
                        "terminal execution totals, settlements or capture disagree",
                    ));
                }
                if *reason == playtime_core::StopReason::AgentFinished
                    && (!self.execution.workspace_started || last_capture.is_none())
                {
                    return Err(StorageError::malformed(
                        "successful execution requires a committed stopped workspace",
                    ));
                }
                let decoded = decode_line(bytes, limits)?;
                self.terminal = Some(StoredEvent::new(decoded.envelope().clone(), bytes.to_vec()));
            }
        }
        Ok(())
    }
    fn record_claim(&mut self, request: &RequestIdentity) -> Result<()> {
        if self
            .execution
            .claims
            .get(&request.request_id)
            .is_some_and(|previous| previous != request)
        {
            return Err(StorageError::malformed(
                "observation purpose/identity disagrees with request admission",
            ));
        }
        self.execution
            .claims
            .insert(request.request_id.clone(), request.clone());
        Ok(())
    }
    fn validate_compaction(
        &self,
        observation: &playtime_protocol::execution::Observation,
        value: &serde_json::Value,
    ) -> Result<()> {
        let Some(links) = playtime_protocol::compaction::validate(observation, value)? else {
            return Ok(());
        };
        for source in &links.requests {
            let Some((request, _)) = self.execution.admitted.get(&source.request_id) else {
                return Err(StorageError::malformed(
                    "compaction source lacks admitted request",
                ));
            };
            if request.purpose != source.purpose
                || request.scope != playtime_core::RequestScope::Run
                || self.execution.model_sources.get(&source.request_id)
                    != source.observation.as_ref()
            {
                return Err(StorageError::malformed(
                    "compaction source differs from retained request evidence",
                ));
            }
            if let Some(location) = &source.observation {
                self.validate_source(location, self.sequence)?;
            }
        }
        if observation.observation_kind == "compaction_applied" {
            if links.plan.as_ref()
                != self
                    .execution
                    .compaction_plan
                    .as_ref()
                    .map(|(_, location)| location)
                || self
                    .execution
                    .compaction_plan
                    .as_ref()
                    .is_some_and(|(ordinal, _)| value["ordinal"].as_u64() != Some(*ordinal))
            {
                return Err(StorageError::malformed(
                    "compaction replacement differs from retained plan",
                ));
            }
            if let Some(location) = &links.plan {
                self.validate_source(location, self.sequence)?;
            }
        }
        if observation.observation_kind == "compaction_planned" {
            let previous: Option<RequestId> =
                serde_json::from_value(value["previous_summary_request"].clone())
                    .map_err(StorageError::malformed)?;
            if previous != self.execution.previous_summary_request {
                return Err(StorageError::malformed(
                    "compaction prior-summary identity differs",
                ));
            }
        }
        Ok(())
    }
    fn validate_request(&self, request: &RequestIdentity) -> Result<u64> {
        match self.execution.admitted.get(&request.request_id) {
            Some((admitted, maximum)) if admitted == request => Ok(*maximum),
            _ => Err(StorageError::malformed(
                "request purpose/scope/identity differs from admission",
            )),
        }
    }
    fn settle(&mut self, request: &RequestIdentity) -> Result<()> {
        if !self.execution.settled.insert(request.request_id.clone()) {
            return Err(StorageError::malformed("request settled twice"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use playtime_core::{RequestPurpose, RequestScope};
    use playtime_protocol::{
        compaction::{EvidenceLinks, RequestSource},
        execution::Observation,
        observations::ObservationIdentity,
    };
    use serde_json::json;

    #[test]
    fn compaction_links_require_exact_admitted_sources_and_retained_plan() {
        let location = |sequence| ObservationLocation {
            controller_sequence: std::num::NonZeroU64::new(sequence).unwrap(),
            archive_sequence: std::num::NonZeroU64::new(sequence).unwrap(),
            record_sha256: crate::sha256(format!("record-{sequence}").as_bytes()),
        };
        let work = RequestIdentity {
            effect_id: 1.try_into().unwrap(),
            request_id: RequestId::new("work").unwrap(),
            call: 1.try_into().unwrap(),
            scope: RequestScope::Run,
            purpose: RequestPurpose::Work,
        };
        let summary = RequestIdentity {
            effect_id: 2.try_into().unwrap(),
            request_id: RequestId::new("summary").unwrap(),
            call: 2.try_into().unwrap(),
            scope: RequestScope::Run,
            purpose: RequestPurpose::Compaction,
        };
        let mut state = LogState {
            sequence: 4,
            ..Default::default()
        };
        for (id, request) in [(1, &work), (2, &summary)] {
            state
                .execution
                .admitted
                .insert(request.request_id.clone(), (request.clone(), 512));
            state
                .execution
                .model_sources
                .insert(request.request_id.clone(), location(id));
            state.observations.insert(id, location(id));
        }
        state.observations.insert(3, location(3));
        let mut observation = Observation {
            controller_sequence: 5.try_into().unwrap(),
            elapsed_ms: 5,
            scope: RequestScope::Run,
            observation_kind: "compaction_planned".into(),
            identity: Default::default(),
            request: None,
            data: ObservationData::Inline { value: json!({}) },
        };
        let planned = json!({"ordinal":1,"source_requests":["work"],"retained_requests":[],"previous_summary":null,"previous_summary_request":null,
            "before_messages":[],"input_bound":1000,"projected_tokens":1512,"estimate_basis":"input_bytes",
            "evidence": EvidenceLinks { schema_version: 1, requests: vec![RequestSource { request_id: work.request_id.clone(), purpose: RequestPurpose::Work, observation: Some(location(1)) }], plan: None }});
        state.validate_compaction(&observation, &planned).unwrap();
        for (pointer, replacement) in [
            ("/evidence/schema_version", json!(2)),
            (
                "/evidence/requests/0/observation/record_sha256",
                json!("0".repeat(64)),
            ),
            ("/evidence/requests/0/observation", json!(null)),
            ("/evidence/requests/0/purpose", json!("compaction")),
            ("/source_requests/0", json!("missing")),
        ] {
            let mut bad = planned.clone();
            *bad.pointer_mut(pointer).unwrap() = replacement;
            assert!(
                state.validate_compaction(&observation, &bad).is_err(),
                "{pointer}"
            );
        }
        // Absent source bytes are allowed only when the archive actually lacks
        // that observation; admission/accounting still retain their own evidence.
        state.execution.model_sources.remove(&work.request_id);
        let mut omitted = planned.clone();
        omitted["evidence"]["requests"][0]["observation"] = json!(null);
        state.validate_compaction(&observation, &omitted).unwrap();
        state.execution.compaction_plan = Some((1, location(3)));
        observation.observation_kind = "compaction_applied".into();
        observation.identity = ObservationIdentity {
            request_id: Some(summary.request_id.clone()),
            ..Default::default()
        };
        observation.request = Some(summary.clone());
        let applied = json!({"ordinal":1,"summary_request_id":"summary","summary":"checked facts","messages":[],
            "input_bound":1000,"projected_tokens":1512,"estimate_basis":"input_bytes",
            "evidence":EvidenceLinks { schema_version:1, requests:vec![RequestSource { request_id:summary.request_id.clone(), purpose:RequestPurpose::Compaction, observation:Some(location(2)) }], plan:Some(location(3)) }});
        state.validate_compaction(&observation, &applied).unwrap();
        for (pointer, replacement) in [
            ("/ordinal", json!(2)),
            ("/summary_request_id", json!("work")),
            ("/evidence/plan", json!(null)),
            ("/evidence/plan/record_sha256", json!("f".repeat(64))),
        ] {
            let mut bad = applied.clone();
            *bad.pointer_mut(pointer).unwrap() = replacement;
            assert!(
                state.validate_compaction(&observation, &bad).is_err(),
                "{pointer}"
            );
        }
    }
}
