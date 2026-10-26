//! The v2 mapping shares the same bounded queue, writer and receipt handling.
use super::*;
use playtime_engine::{Capture, RequestContext};
use playtime_protocol::execution::{self, CaptureIndex, CaptureRef, Fact, RequestIdentity};

pub(super) fn request_identity(context: &RequestContext) -> RequestIdentity {
    RequestIdentity {
        effect_id: NonZeroU64::new(context.effect_id).unwrap(),
        request_id: context.request_id.clone(),
        call: NonZeroU64::new(context.call).unwrap(),
        scope: context.scope.clone(),
        purpose: context.purpose,
    }
}

impl Encoder {
    pub(super) fn link_compaction(&self, record: &mut playtime_engine::Record) -> Result<()> {
        use playtime_core::{RequestId, RequestPurpose};
        use playtime_protocol::compaction::{EvidenceLinks, RequestSource, linked_kind};
        if !linked_kind(&record.kind) {
            return Ok(());
        }
        let source = |request_id: RequestId, purpose| RequestSource {
            observation: self.model_sources.get(&request_id).cloned(),
            request_id,
            purpose,
        };
        let mut requests = Vec::new();
        if record.kind == "compaction_planned" {
            for key in ["source_requests", "retained_requests"] {
                let ids: Vec<RequestId> = serde_json::from_value(record.data[key].clone())?;
                requests.extend(ids.into_iter().map(|id| source(id, RequestPurpose::Work)));
            }
            record.data["previous_summary_request"] =
                serde_json::to_value(&self.previous_summary_request)?;
            if let Some(id) = &self.previous_summary_request {
                requests.push(source(id.clone(), RequestPurpose::Compaction));
            }
        } else {
            let (key, purpose) = if record.kind == "compaction_applied" {
                ("summary_request_id", RequestPurpose::Compaction)
            } else {
                ("request_id", RequestPurpose::Work)
            };
            requests.push(source(
                serde_json::from_value(record.data[key].clone())?,
                purpose,
            ));
        }
        record.data["evidence"] = serde_json::to_value(EvidenceLinks {
            schema_version: 1,
            requests,
            plan: if record.kind == "compaction_applied" {
                self.compaction_plan.clone()
            } else {
                None
            },
        })?;
        Ok(())
    }

    pub fn use_execution_v2(&mut self) {
        self.execution_v2 = true;
    }

    pub(super) fn execution_event(
        &mut self,
        time: &UtcTime,
        fact: Fact,
        objects: Vec<FrozenInput>,
    ) -> Result<()> {
        // Logical kinds are driver barriers; the wire kind remains versioned.
        let (kind, effect_id) = match &fact {
            Fact::EffectIntent(intent) => ("effect_intent", Some(intent.effect_id.get())),
            Fact::RunFinished { .. } => ("run_finished", None),
            _ => (execution::KIND, None),
        };
        let line = self.encode(
            time,
            execution::KIND,
            serde_json::to_value(execution::Record::new(fact))?,
        )?;
        self.push(EncodedEvent {
            log_cost: line.len() as u64,
            line,
            objects,
            sequence: self.next_sequence,
            kind: kind.into(),
            effect_id,
        })
    }

    pub(super) fn execution_control(
        &mut self,
        time: &UtcTime,
        update: ControlUpdate,
    ) -> Result<()> {
        let fact = match update.fact {
            ControlFact::WorkspaceStarted { workspace } => Fact::WorkspaceStarted { workspace },
            ControlFact::EffectPending(info) => {
                if !self.intents.insert(info.id) {
                    return Err(Error::Invalid("duplicate effect intent".into()));
                }
                let sources = self
                    .sources
                    .values()
                    .filter(|(_, identity)| {
                        identity.effect_id.is_some_and(|id| id.get() == info.id)
                            || (info.identity.tool_request_sequence.is_some()
                                && identity.tool_request_sequence
                                    == info.identity.tool_request_sequence)
                    })
                    .map(|(location, _)| location.clone())
                    .collect();
                Fact::EffectIntent(execution::EffectIntent {
                    effect_id: NonZeroU64::new(info.id).unwrap(),
                    effect_kind: info.kind.into(),
                    request: info.request.as_ref().map(request_identity),
                    tool_request_sequence: info.identity.tool_request_sequence,
                    maximum_tokens: info.maximum_tokens.and_then(NonZeroU64::new),
                    deadline_ms: info.deadline_ms,
                    sources,
                })
            }
            ControlFact::EffectNotDispatched(info) => Fact::EffectNotDispatched {
                effect_id: NonZeroU64::new(info.id).unwrap(),
                request: info.request.as_ref().map(request_identity),
                tool_request_sequence: info.identity.tool_request_sequence,
            },
            ControlFact::RequestSettled { request, usage, .. } => Fact::RequestSettled {
                request: request_identity(&request),
                usage,
            },
            ControlFact::RequestNotDispatched {
                request,
                released_tokens,
            } => Fact::RequestNotDispatched {
                request: request_identity(&request),
                released_tokens: NonZeroU64::new(released_tokens).unwrap(),
            },
            ControlFact::ExecResult {
                request,
                effect_id,
                tool_request_sequence,
                outcome,
            } => Fact::CommandFinished {
                request: request_identity(&request),
                effect_id: NonZeroU64::new(effect_id).unwrap(),
                tool_request_sequence: NonZeroU64::new(tool_request_sequence).unwrap(),
                outcome,
            },
            ControlFact::ObservationOmitted {
                kind,
                size_bytes,
                identity,
                request,
            } => Fact::ObservationOmitted {
                controller_sequence: None,
                observation_kind: kind,
                elapsed_ms: update.elapsed_ms,
                scope: playtime_core::RequestScope::Run,
                identity,
                request: request.as_ref().map(request_identity),
                size_bytes,
                reason: "controller_log_limit".into(),
            },
            ControlFact::CaptureCommitted { .. } => {
                if self.last_capture.is_none() {
                    return Err(Error::Invalid(
                        "capture acknowledgment lacks writer receipt".into(),
                    ));
                }
                return Ok(());
            }
            _ => {
                return Err(Error::Invalid(
                    "session-dependent fact in run-scoped profile".into(),
                ));
            }
        };
        self.execution_event(time, fact, Vec::new())
    }

    pub fn capture_contents(
        &mut self,
        time: &UtcTime,
        effect_id: u64,
        capture: &Capture,
        workspace: &[u8],
        files: &BTreeMap<&str, &[u8]>,
        committed_sequence: u64,
    ) -> Result<Publication> {
        if !self.execution_v2 || self.has_queue() || self.last_capture.is_some() {
            return Err(Error::Invalid(
                "capture cannot overtake observations or repeat publication".into(),
            ));
        }
        let snapshot = &capture.snapshot;
        if reference(snapshot.workspace.path.as_str(), workspace) != snapshot.workspace {
            return Err(Error::Invalid(
                "capture bytes differ from controller reference".into(),
            ));
        }
        if files.keys().any(|path| {
            snapshot
                .files
                .get(*path)
                .is_none_or(|m| !matches!(m.entry_type.as_str(), "0" | "file") || m.link.is_some())
        }) {
            return Err(Error::Invalid("unaccounted capture content".into()));
        }
        let mut objects = vec![FrozenInput {
            reference: snapshot.workspace.clone(),
            bytes: workspace.to_vec(),
        }];
        let mut versions = Vec::new();
        for (index, (path, metadata)) in snapshot.files.iter().enumerate() {
            let content = if matches!(metadata.entry_type.as_str(), "0" | "file")
                && metadata.link.is_none()
            {
                let bytes = files
                    .get(path.as_str())
                    .filter(|bytes| {
                        metadata.size_bytes == bytes.len() as u64
                            && metadata.sha256.as_ref() == Some(&sha256(bytes))
                    })
                    .ok_or_else(|| {
                        Error::Invalid("regular file bytes differ from capture metadata".into())
                    })?;
                FileContent::Retained {
                    content: object(
                        &format!("artifacts/final/files/{index}"),
                        bytes.to_vec(),
                        &mut objects,
                    ),
                }
            } else {
                FileContent::Unavailable {
                    reason: "nonregular_entry_metadata_only".into(),
                }
            };
            versions.push(FileVersion {
                path: path.clone(),
                size_bytes: metadata.size_bytes,
                mode: metadata.mode,
                entry_type: metadata.entry_type.clone(),
                sha256: metadata.sha256.clone(),
                link: metadata.link.clone(),
                content,
            });
        }
        let map = ObservationSourceMap {
            schema_version: 1,
            run_id: self.run_id.clone(),
            entries: self
                .sources
                .values()
                .filter(|(location, _)| location.archive_sequence.get() <= committed_sequence)
                .map(|(location, _)| location.clone())
                .collect(),
        };
        let bytes = serde_json::to_vec(&map)?;
        if bytes.len() as u64 > self.limits.manifest_bytes {
            return Err(Error::Capacity(
                "capture source map exceeds manifest limit".into(),
            ));
        }
        let source_map = object("artifacts/final/source-map.json", bytes, &mut objects);
        let index = CaptureIndex {
            schema_version: execution::VERSION,
            run_id: self.run_id.clone(),
            workspace: snapshot.workspace.clone(),
            files: versions,
            source_map,
        };
        let bytes = serde_json::to_vec(&index)?;
        if bytes.len() as u64 > self.limits.manifest_bytes {
            return Err(Error::Capacity(
                "capture index exceeds manifest limit".into(),
            ));
        }
        let published = CaptureRef {
            workspace: snapshot.workspace.clone(),
            index: object("artifacts/final/capture-index-v2.json", bytes, &mut objects),
        };
        self.execution_event(
            time,
            Fact::CapturePublished {
                capture: published.clone(),
            },
            objects,
        )?;
        let mut batch = self.batch().unwrap();
        batch.class = BatchClass::Finalization;
        batch.capture = Some((effect_id, published));
        Ok(batch)
    }
}
