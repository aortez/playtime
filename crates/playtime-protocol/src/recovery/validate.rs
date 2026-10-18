use super::*;
use crate::{ReadError, read::require_version};
use serde::{Deserializer, de::DeserializeOwned};
use serde_json::Value;
use std::collections::BTreeSet;

pub trait RecoveryRecord: Serialize + DeserializeOwned {
    fn validate(&self) -> Result<(), ReadError>;
}
fn require(ok: bool, message: &str) -> Result<(), ReadError> {
    if ok {
        Ok(())
    } else {
        Err(ReadError::malformed(message))
    }
}
fn version(value: u32) -> Result<(), ReadError> {
    require(value == VERSION, "unsupported recovery control version")
}
/// Strict control decoding includes reused nested DTOs, while ordinary archive
/// decoding remains forward-compatible. Input byte bounds precede allocation.
pub fn decode<T: RecoveryRecord>(bytes: &[u8], limit: u64) -> Result<T, ReadError> {
    require(
        bytes.len() as u64 <= limit,
        "recovery control byte bound exceeded",
    )?;
    let text = std::str::from_utf8(bytes).map_err(ReadError::malformed)?;
    require_version(text, "recovery control", VERSION)?;
    let typed: T = serde_json::from_str(text).map_err(ReadError::malformed)?;
    let raw = unique_json(bytes)?;
    let known = serde_json::to_value(&typed).map_err(ReadError::malformed)?;
    reject_unknown(&raw, &known)?;
    typed.validate()?;
    Ok(typed)
}
// serde_json::Value normally accepts duplicate map keys. Recovery control
// cannot discard a conflicting field, including inside reused/defaulted DTOs.
pub(crate) fn unique_json(bytes: &[u8]) -> Result<Value, ReadError> {
    struct Unique(Value);
    impl<'de> Deserialize<'de> for Unique {
        fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            struct Visitor;
            impl<'de> serde::de::Visitor<'de> for Visitor {
                type Value = Unique;
                fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    f.write_str("JSON with unique object keys")
                }
                fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<Unique, E> {
                    Ok(Unique(v.into()))
                }
                fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Unique, E> {
                    Ok(Unique(v.into()))
                }
                fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Unique, E> {
                    Ok(Unique(v.into()))
                }
                fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<Unique, E> {
                    Ok(Unique(v.into()))
                }
                fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Unique, E> {
                    Ok(Unique(v.into()))
                }
                fn visit_unit<E: serde::de::Error>(self) -> Result<Unique, E> {
                    Ok(Unique(Value::Null))
                }
                fn visit_seq<A: serde::de::SeqAccess<'de>>(
                    self,
                    mut seq: A,
                ) -> Result<Unique, A::Error> {
                    let mut values = Vec::new();
                    while let Some(Unique(value)) = seq.next_element()? {
                        values.push(value);
                    }
                    Ok(Unique(Value::Array(values)))
                }
                fn visit_map<A: serde::de::MapAccess<'de>>(
                    self,
                    mut map: A,
                ) -> Result<Unique, A::Error> {
                    let mut values = serde_json::Map::new();
                    while let Some(key) = map.next_key::<String>()? {
                        if values.contains_key(&key) {
                            return Err(serde::de::Error::custom(
                                "duplicate recovery control field",
                            ));
                        }
                        let Unique(value) = map.next_value()?;
                        values.insert(key, value);
                    }
                    Ok(Unique(Value::Object(values)))
                }
            }
            d.deserialize_any(Visitor)
        }
    }
    serde_json::from_slice::<Unique>(bytes)
        .map(|v| v.0)
        .map_err(ReadError::malformed)
}
pub(crate) fn reject_unknown(raw: &Value, known: &Value) -> Result<(), ReadError> {
    match (raw, known) {
        (Value::Object(raw), Value::Object(known)) => {
            for (key, value) in raw {
                let target = known
                    .get(key)
                    .ok_or_else(|| ReadError::malformed("unknown recovery control field"))?;
                reject_unknown(value, target)?;
            }
        }
        (Value::Array(raw), Value::Array(known)) => {
            for (raw, known) in raw.iter().zip(known) {
                reject_unknown(raw, known)?;
            }
        }
        _ => (),
    }
    Ok(())
}
pub(super) fn full_limits<'de, D: Deserializer<'de>>(d: D) -> Result<StorageLimits, D::Error> {
    // Storage's ordinary reader defaults are intentionally not execution rules.
    let value = Value::deserialize(d)?;
    let required = [
        "event_log_bytes",
        "record_bytes",
        "batch_bytes",
        "content_bytes",
        "manifest_bytes",
        "verified_content_bytes",
        "references",
        "events",
        "page_bytes",
    ];
    if !value
        .as_object()
        .is_some_and(|v| v.len() == required.len() && required.iter().all(|k| v.contains_key(*k)))
    {
        return Err(serde::de::Error::custom(
            "all frozen storage limits are mandatory",
        ));
    }
    serde_json::from_value(value).map_err(serde::de::Error::custom)
}
impl RecoveryRecord for ExecutionContractV1 {
    fn validate(&self) -> Result<(), ReadError> {
        version(self.schema_version)?;
        require(
            self.rule_set == RULE_SET
                && self.event_schema_version == 1
                && self.bridge_schema_version == 1
                && self.observation_schema_version == 1
                && self.required_event_kinds == REQUIRED_EVENTS
                && self.effect_kinds == EFFECT_KINDS
                && self.observation_accounting == OBSERVATION_ACCOUNTING,
            "unsupported execution profile",
        )?;
        let l = &self.storage_limits;
        require(
            self.finalization_reserve_bytes == crate::storage::FINALIZATION_RESERVE_BYTES
                && l.event_log_bytes == self.log_bytes.get()
                && [
                    l.event_log_bytes,
                    l.record_bytes,
                    l.batch_bytes,
                    l.content_bytes,
                    l.manifest_bytes,
                    l.verified_content_bytes,
                    l.references,
                    l.events,
                    l.page_bytes,
                ]
                .into_iter()
                .all(|v| v > 0 && v <= isize::MAX as u64)
                && l.record_bytes <= l.batch_bytes
                && l.record_bytes <= l.page_bytes
                && l.manifest_bytes <= l.content_bytes,
            "inconsistent frozen storage limits",
        )?;
        require(
            self.worker_executable.path.as_str() == "inputs/worker/executable"
                && self.configuration.path.as_str() == "inputs/scenario.json",
            "unexpected execution identity paths",
        )
    }
}
pub(super) fn archive(a: &ArchiveIdentity) -> Result<(), ReadError> {
    require(
        a.descriptor.path.as_str() == "archive.json"
            && a.input_manifest.path.as_str() == "inputs/manifest.json",
        "invalid archive identity paths",
    )
}
pub(super) fn frontier(f: &EventFrontier, a: &ArchiveIdentity) -> Result<(), ReadError> {
    require(
        &f.archive == a
            && f.marker.path.as_str() == "events.commit.json"
            && (f.committed_sequence == 0) == (f.committed_bytes == 0),
        "invalid frontier binding",
    )
}
impl RecoveryRecord for RecoveryBindingV1 {
    fn validate(&self) -> Result<(), ReadError> {
        version(self.schema_version)?;
        archive(&self.archive)?;
        require(
            self.worker.run_id == self.archive.run_id
                && self.worker.manifest_sha256 == self.archive.input_manifest.sha256
                && self.superseding_generation > self.original_generation
                && !self.worker.unit.is_empty()
                && self.worker.unit.len() <= 255
                && self
                    .worker
                    .unit
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.')),
            "recovery binding/generation mismatch",
        )
    }
}
impl MaintenanceLimitsV1 {
    pub fn validate(&self) -> Result<(), ReadError> {
        let max = Self::default();
        let pairs = [
            (self.total_bytes, max.total_bytes),
            (self.entries, max.entries),
            (self.plan_bytes, max.plan_bytes),
            (self.inventory_bytes, max.inventory_bytes),
            (self.selection_bytes, max.selection_bytes),
            (self.resolution_bytes, max.resolution_bytes),
            (self.inventory_entries, max.inventory_entries),
        ];
        require(
            pairs.into_iter().all(|(v, m)| v > 0 && v <= m)
                && self.resolution_reserve_bytes == max.resolution_reserve_bytes
                && self.resolution_reserve_entries == max.resolution_reserve_entries
                && self.total_bytes > self.resolution_reserve_bytes
                && self.entries > self.resolution_reserve_entries
                && self.plan_bytes <= self.total_bytes - self.resolution_reserve_bytes
                && self.inventory_bytes <= self.total_bytes - self.resolution_reserve_bytes,
            "invalid recovery maintenance limits",
        )
    }
}
impl RecoveryRecord for RecoveryAttemptV1 {
    fn validate(&self) -> Result<(), ReadError> {
        version(self.schema_version)?;
        self.binding.validate()?;
        self.maintenance_limits.validate()
    }
}
impl RecoveryRecord for ConfirmedBoundsV1 {
    fn validate(&self) -> Result<(), ReadError> {
        version(self.schema_version)?;
        if let Some(worker) = &self.worker {
            frontier(worker, &worker.archive)?;
        }
        if let Some(public) = &self.public {
            frontier(&public.frontier, &public.frontier.archive)?;
            if let Some(worker) = &self.worker {
                require(
                    worker.archive == public.frontier.archive
                        && worker.committed_sequence <= public.frontier.committed_sequence,
                    "public boundary cannot precede worker confirmation",
                )?;
            }
        }
        Ok(())
    }
}
impl RecoveryRecord for RecoveryOriginV1 {
    fn validate(&self) -> Result<(), ReadError> {
        version(self.schema_version)?;
        require(
            self.plan.path.as_str() == plan_path(&self.attempt_id) && self.plan.size_bytes > 0,
            "invalid recovery origin plan",
        )
    }
}
impl RecoveryRecord for RecoveryPlanV1 {
    fn validate(&self) -> Result<(), ReadError> {
        version(self.schema_version)?;
        self.attempt.validate()?;
        self.prior_bounds.validate()?;
        frontier(&self.base, &self.archive)?;
        require(
            self.archive == self.attempt.binding.archive
                && self.execution_contract.path.as_str() == EXECUTION_CONTRACT
                && self.initial_inventory.path.as_str()
                    == private_path(&self.attempt.attempt_id, "diagnostics.json")
                && self.unresolved.len() <= 100_000
                && self.base_accounted_log_bytes >= self.base.committed_bytes
                && self
                    .base_accounted_log_bytes
                    .checked_add(self.available_log_bytes)
                    .is_some(),
            "invalid plan identity/bounds",
        )?;
        for f in self
            .prior_bounds
            .worker
            .iter()
            .chain(self.prior_bounds.public.iter().map(|p| &p.frontier))
        {
            frontier(f, &self.archive)?;
            require(
                f.committed_sequence <= self.base.committed_sequence
                    && f.committed_bytes <= self.base.committed_bytes,
                "saved boundary exceeds plan base",
            )?;
        }
        self.existing_usage
            .validate()
            .map_err(ReadError::malformed)?;
        self.resulting_usage
            .validate()
            .map_err(ReadError::malformed)?;
        let mut requests = BTreeSet::new();
        let mut effects = BTreeSet::new();
        let mut prior = 0;
        let mut reserved = self.existing_usage.unknown_reserved_tokens;
        for intent in &self.unresolved {
            require(
                intent.source.archive_sequence.get() > prior
                    && intent.source.archive_sequence.get() <= self.base.committed_sequence
                    && requests.insert(&intent.request_id)
                    && effects.insert(intent.effect_id),
                "unordered/duplicate unresolved intents",
            )?;
            prior = intent.source.archive_sequence.get();
            reserved = reserved
                .checked_add(intent.maximum_tokens.get())
                .ok_or_else(|| ReadError::malformed("recovery usage overflow"))?;
        }
        require(
            self.resulting_usage.known_generated_tokens
                == self.existing_usage.known_generated_tokens
                && self.resulting_usage.unknown_reserved_tokens == reserved
                && self
                    .existing_usage
                    .unknown_requests
                    .checked_add(self.unresolved.len() as u64)
                    == Some(self.resulting_usage.unknown_requests),
            "plan must preserve existing usage and original unresolved caps",
        )?;
        for source in self
            .first_stop
            .iter()
            .map(|s| &s.source)
            .chain(self.last_checkpoint.iter().map(|c| &c.source))
        {
            require(
                source.archive_sequence.get() <= self.base.committed_sequence,
                "plan source outside base",
            )?;
        }
        Ok(())
    }
}
impl RecoveryRecord for RecoverySelectionV1 {
    fn validate(&self) -> Result<(), ReadError> {
        version(self.schema_version)?;
        archive(&self.archive)?;
        frontier(&self.base, &self.archive)?;
        frontier(&self.final_frontier, &self.archive)?;
        let c = &self.capacity;
        require(
            self.plan.path.as_str() == plan_path(&self.attempt_id)
                && self.inventory.path.as_str()
                    == private_path(&self.attempt_id, "diagnostics.json")
                && self.patch.path.as_str() == private_path(&self.attempt_id, "patch.jsonl")
                && self.target_marker.path.as_str()
                    == private_path(&self.attempt_id, "marker.json")
                && self.final_frontier.marker.sha256 == self.target_marker.sha256
                && self.final_frontier.marker.size_bytes == self.target_marker.size_bytes
                && self.patch.size_bytes == c.patch_bytes
                && self.base.committed_bytes.checked_add(c.patch_bytes)
                    == Some(self.final_frontier.committed_bytes)
                && self.final_frontier.committed_sequence > self.base.committed_sequence
                && c.event_count == self.final_frontier.committed_sequence
                && c.base_accounted_log_bytes >= self.base.committed_bytes
                && c.base_accounted_log_bytes.checked_add(c.patch_bytes)
                    == Some(c.final_accounted_log_bytes)
                && c.final_accounted_log_bytes
                    .checked_add(c.remaining_log_bytes)
                    == Some(c.frozen_log_bytes),
            "invalid recovery selection/capacity",
        )
    }
}
impl RecoveryRecord for RecoveryResolutionV1 {
    fn validate(&self) -> Result<(), ReadError> {
        version(self.schema_version)?;
        archive(&self.archive)?;
        match &self.resolution {
            ResolutionV1::TerminalPreserved { frontier: f } => frontier(f, &self.archive),
            ResolutionV1::NeverStarted { .. } => Ok(()),
            ResolutionV1::AbandonedIncomplete {
                reason, accounting, ..
            } => {
                require(
                    !reason.is_empty() && reason.len() <= 1024,
                    "invalid abandonment reason",
                )?;
                if let AccountingAvailabilityV1::Available { usage } = accounting {
                    usage.validate().map_err(ReadError::malformed)?;
                }
                Ok(())
            }
        }
    }
}
