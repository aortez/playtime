use super::*;
use playtime_core::{GenerationUsage, RequestId, RequestPurpose, UsageTotals};
use playtime_protocol::{
    execution::{Fact, Observation, decode_capture_index},
    v1::UsageSummary,
};

#[derive(Default)]
struct Attempt {
    purpose: Option<RequestPurpose>,
    admitted: bool,
    usage: Option<GenerationUsage>,
    not_dispatched: bool,
}
#[derive(Default)]
struct Totals {
    calls: u64,
    settled: u64,
    released: u64,
    usage: UsageTotals,
}
impl Totals {
    fn summary(self) -> PurposeAccounting {
        PurposeAccounting {
            admitted_calls: self.calls,
            settled_calls: self.settled,
            not_dispatched_calls: self.released,
            usage: self.usage.into(),
        }
    }
    fn record(&mut self, attempt: &Attempt) -> Result<()> {
        self.calls += u64::from(attempt.admitted);
        self.released += u64::from(attempt.not_dispatched);
        if let Some(usage) = attempt.usage {
            self.settled += 1;
            self.usage
                .record(usage)
                .map_err(|error| Error::Invalid(error.to_string()))?;
        }
        Ok(())
    }
}
impl ArchiveQueries {
    /// Bounded inspection at this query's verified frontier. Omitted evidence
    /// remains explicit; reading never creates a publication receipt.
    pub fn compaction_history(
        &self,
        after: u64,
        limit: u32,
    ) -> Result<playtime_protocol::compaction::EvidencePage> {
        use playtime_protocol::compaction::{EvidenceEntry, EvidencePage, evidence_kind};
        let page = self.history(after, limit, true)?;
        let mut entries = Vec::new();
        for entry in page.entries {
            if let ArchiveFact::Execution { event, resolved } = entry.fact {
                match event.fact {
                    Fact::Observation(observation)
                        if evidence_kind(&observation.observation_kind) =>
                    {
                        entries.push(EvidenceEntry::Recorded {
                            location: ObservationLocation {
                                controller_sequence: observation.controller_sequence,
                                archive_sequence: entry.envelope.sequence,
                                record_sha256: entry.record_sha256,
                            },
                            kind: observation.observation_kind,
                            request: observation.request,
                            value: resolved.ok_or_else(|| {
                                Error::Invalid("evidence payload was not resolved".into())
                            })?,
                        });
                    }
                    Fact::ObservationOmitted {
                        controller_sequence,
                        observation_kind,
                        request,
                        reason,
                        ..
                    } if evidence_kind(&observation_kind) => {
                        entries.push(EvidenceEntry::Omitted {
                            archive_sequence: entry.envelope.sequence,
                            controller_sequence,
                            kind: observation_kind,
                            request,
                            reason,
                        });
                    }
                    _ => {}
                }
            }
        }
        Ok(EvidencePage {
            entries,
            next_after_sequence: page.next_after_sequence,
            has_more: page.has_more,
        })
    }

    /// Lossless per-purpose projection for both record families. Legacy
    /// attribution requires the recorded handoff boolean, never a guess.
    pub fn request_accounting(&self) -> Result<RequestAccounting> {
        let mut accounting = AccountingFold::new(self);
        self.visit(|entry| accounting.add(self, entry))?;
        accounting.finish(self)
    }

    pub fn captures(&self) -> Result<Vec<ArchiveCapture>> {
        let mut captures = Vec::new();
        self.visit(|entry| {
            captures.extend(self.capture(entry)?);
            Ok(())
        })?;
        Ok(captures)
    }

    /// The final capture this entry publishes, with its index read.
    pub(super) fn capture(&self, entry: &ArchiveHistoryEntry) -> Result<Option<ArchiveCapture>> {
        let ArchiveFact::Execution { event, .. } = &entry.fact else {
            return Ok(None);
        };
        let Fact::CapturePublished { capture } = &event.fact else {
            return Ok(None);
        };
        let bytes = self.content(&capture.index)?;
        let index = decode_capture_index(
            std::str::from_utf8(&bytes).map_err(|error| Error::Invalid(error.to_string()))?,
        )?;
        Ok(Some(ArchiveCapture {
            capture: capture.clone(),
            index,
        }))
    }
}

/// `request_accounting`'s state while a pass goes through the history.
pub(super) struct AccountingFold {
    attempts: BTreeMap<RequestId, Attempt>,
    admission_evidence: bool,
}

impl AccountingFold {
    pub(super) fn new(queries: &ArchiveQueries) -> Self {
        Self {
            attempts: BTreeMap::new(),
            admission_evidence: queries.reader.manifest().schema_version == 2,
        }
    }

    pub(super) fn add(
        &mut self,
        queries: &ArchiveQueries,
        entry: &ArchiveHistoryEntry,
    ) -> Result<()> {
        match &entry.fact {
            ArchiveFact::Execution { event, .. } => match &event.fact {
                Fact::EffectIntent(intent) if intent.effect_kind == "model" => {
                    let request = intent.request.as_ref().unwrap();
                    let attempt = self.attempts.entry(request.request_id.clone()).or_default();
                    attempt.purpose = Some(request.purpose);
                    attempt.admitted = true;
                    self.admission_evidence = true;
                }
                Fact::Observation(Observation { request, .. })
                | Fact::ObservationOmitted { request, .. } => {
                    if let Some(request) = request {
                        self.attempts
                            .entry(request.request_id.clone())
                            .or_default()
                            .purpose = Some(request.purpose);
                    }
                }
                Fact::RequestSettled { request, usage } => {
                    self.attempts
                        .entry(request.request_id.clone())
                        .or_default()
                        .usage = Some(*usage);
                }
                Fact::RequestNotDispatched { request, .. } => {
                    self.attempts
                        .entry(request.request_id.clone())
                        .or_default()
                        .not_dispatched = true;
                }
                _ => {}
            },
            ArchiveFact::Control {
                event: BridgeEvent::EffectIntent(intent),
            } if intent.effect_kind == "model" => {
                self.attempts
                    .entry(intent.request_id.clone().unwrap())
                    .or_default()
                    .admitted = true;
                self.admission_evidence = true;
            }
            ArchiveFact::Control {
                event: BridgeEvent::RequestNotDispatched(fact),
            } => {
                self.attempts
                    .entry(fact.request_id.clone())
                    .or_default()
                    .not_dispatched = true;
            }
            ArchiveFact::Canonical {
                event:
                    KnownEvent::RequestFinished {
                        request_id, usage, ..
                    },
            } => {
                self.attempts.entry(request_id.clone()).or_default().usage = Some(*usage);
            }
            ArchiveFact::Observation { observation, .. }
                if observation.observation_kind == "model_request" =>
            {
                let value = match &observation.data {
                    ObservationData::Inline { value } => value.clone(),
                    ObservationData::Artifact { content } => {
                        serde_json::from_slice(&queries.content(content)?)?
                    }
                };
                if let (Some(id), Some(handoff)) =
                    (&observation.identity.request_id, value["handoff"].as_bool())
                {
                    let purpose = if handoff {
                        RequestPurpose::Handoff
                    } else {
                        RequestPurpose::Work
                    };
                    let attempt = self.attempts.entry(id.clone()).or_default();
                    if attempt.purpose.is_some_and(|previous| previous != purpose) {
                        return Err(Error::Invalid("conflicting legacy purpose evidence".into()));
                    }
                    attempt.purpose = Some(purpose);
                }
            }
            _ => {}
        }
        Ok(())
    }

    pub(super) fn finish(self, queries: &ArchiveQueries) -> Result<RequestAccounting> {
        let mut by_purpose: BTreeMap<RequestPurpose, Totals> = BTreeMap::new();
        let mut unclassified = Totals::default();
        let mut all = Totals::default();
        for attempt in self.attempts.values() {
            all.record(attempt)?;
            match attempt.purpose {
                Some(purpose) => by_purpose.entry(purpose).or_default().record(attempt)?,
                None => unclassified.record(attempt)?,
            }
        }
        if UsageSummary::from(all.usage) != queries.snapshot.inspection().usage {
            return Err(Error::Invalid(
                "purpose projection does not reconcile with committed usage".into(),
            ));
        }
        Ok(RequestAccounting {
            model_calls: (self.admission_evidence
                && self.attempts.values().all(|attempt| attempt.admitted))
            .then_some(all.calls),
            by_purpose: by_purpose
                .into_iter()
                .map(|(purpose, totals)| (purpose, totals.summary()))
                .collect(),
            unclassified: unclassified.summary(),
        })
    }
}
