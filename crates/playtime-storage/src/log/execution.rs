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
                if let ObservationData::Artifact { content } = &source.data {
                    self.typed_reference(content, ReferenceRole::ObservationPayload, limits)?;
                    source.resolve(&directory.verify(content, limits.content_bytes)?)?;
                    references.push(content.clone());
                }
                self.observations.insert(
                    source.controller_sequence.get(),
                    ObservationLocation {
                        controller_sequence: source.controller_sequence,
                        archive_sequence: std::num::NonZeroU64::new(self.sequence + 1).unwrap(),
                        record_sha256: crate::sha256(bytes),
                    },
                );
            }
            Fact::ObservationOmitted { request, .. } => {
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
