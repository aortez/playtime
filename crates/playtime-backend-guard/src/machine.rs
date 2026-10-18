use crate::{Result, wire::*};
use std::collections::BTreeMap;

#[derive(Clone, Default)]
pub(crate) struct Machine {
    pub backend: Option<BackendIdentity>,
    pub lease: Option<String>,
    pub leases: BTreeMap<String, LeaseRecord>,
    pub requests: BTreeMap<String, RequestRecord>,
    pub active_request: Option<String>,
    pub stop: Option<StopOperation>,
    pub observation: Option<ManagerObservation>,
    pub terminated: bool,
    pub operations: BTreeMap<String, Event>,
}
impl Machine {
    pub fn lease(&self) -> Option<&LeaseRecord> {
        self.lease.as_ref().and_then(|id| self.leases.get(id))
    }
    pub fn request(&self) -> Option<&RequestRecord> {
        self.active_request
            .as_ref()
            .and_then(|id| self.requests.get(id))
    }
    pub fn pending_request(&self) -> bool {
        self.request()
            .is_some_and(|r| r.state == (RequestState::MayHaveDispatched {}))
    }
    pub fn apply(&mut self, event: &Event) -> Result<()> {
        let invalid = Failure::Conflict;
        match event {
            Event::Prepared { backend } => {
                backend.validate().map_err(|_| Failure::Invalid)?;
                if let Some(old) = &self.backend {
                    if !self.terminated
                        || self.lease().is_some_and(|l| !l.released)
                        || old.backend_id != backend.backend_id
                        || old.guard_id != backend.guard_id
                        || old.boot_id != backend.boot_id
                        || old.unit != backend.unit
                        || old.invocation_id == backend.invocation_id
                        || old.generation.checked_add(1) != Some(backend.generation)
                    {
                        return Err(invalid);
                    }
                } else if backend.generation != 1 {
                    return Err(invalid);
                }
                self.backend = Some(backend.clone());
                self.lease = None;
                self.active_request = None;
                self.stop = None;
                self.observation = None;
                self.terminated = false;
            }
            Event::Acquired { lease } => {
                lease.spec.validate().map_err(|_| Failure::Invalid)?;
                if self.backend.as_ref() != Some(&lease.spec.backend)
                    || self.stop.is_some()
                    || self.lease().is_some_and(|l| !l.released)
                    || lease.last_ordinal != 0
                    || lease.released
                    || self.leases.contains_key(lease.spec.lease_id.as_str())
                    || self
                        .operations
                        .contains_key(lease.spec.operation_id.as_str())
                {
                    return Err(invalid);
                }
                self.lease = Some(lease.spec.lease_id.as_str().into());
                self.active_request = None;
                self.operations
                    .insert(lease.spec.operation_id.as_str().into(), event.clone());
                self.leases
                    .insert(lease.spec.lease_id.as_str().into(), lease.clone());
            }
            Event::DispatchMarked { request } => {
                request.spec.validate().map_err(|_| Failure::Invalid)?;
                let lease = self.lease().ok_or(invalid)?;
                if lease.released
                    || self.stop.is_some()
                    || self.pending_request()
                    || request.spec.lease_id != lease.spec.lease_id
                    || request.spec.generation != lease.spec.backend.generation
                    || lease.last_ordinal.checked_add(1) != Some(request.spec.ordinal)
                    || request.marked_ms >= request.spec.deadline_ms
                    || request.spec.deadline_ms > lease.spec.execution_deadline_ms
                    || request.state != (RequestState::MayHaveDispatched {})
                    || self.requests.contains_key(request.spec.request_id.as_str())
                    || self
                        .requests
                        .values()
                        .any(|r| r.spec.effect_id == request.spec.effect_id)
                {
                    return Err(invalid);
                }
                self.leases
                    .get_mut(request.spec.lease_id.as_str())
                    .unwrap()
                    .last_ordinal = request.spec.ordinal;
                self.active_request = Some(request.spec.request_id.as_str().into());
                self.requests
                    .insert(request.spec.request_id.as_str().into(), request.clone());
            }
            Event::Terminal {
                request_id,
                completed_ms,
            } => {
                let r = self.request().ok_or(invalid)?;
                if &r.spec.request_id != request_id
                    || !self.pending_request()
                    || self.stop.is_some()
                    || *completed_ms < r.marked_ms
                    || *completed_ms >= r.spec.deadline_ms
                {
                    return Err(invalid);
                }
                self.requests.get_mut(request_id.as_str()).unwrap().state =
                    RequestState::Terminal {
                        completed_ms: *completed_ms,
                    };
            }
            Event::StopSelected { operation } => {
                let lease = self.lease().ok_or(invalid)?;
                if self.stop.is_some()
                    || lease.released
                    || operation.backend != lease.spec.backend
                    || operation.lease_id != lease.spec.lease_id
                    || self
                        .operations
                        .contains_key(operation.operation_id.as_str())
                    || operation.supervisor_deadline_ms
                        != operation
                            .decided_ms
                            .checked_add(SUPERVISOR_STOP_MS)
                            .ok_or(invalid)?
                            .min(lease.spec.cleanup_deadline_ms)
                    || operation.observation_deadline_ms
                        != operation
                            .decided_ms
                            .checked_add(STOP_OBSERVATION_MS)
                            .ok_or(invalid)?
                            .min(lease.spec.cleanup_deadline_ms)
                {
                    return Err(invalid);
                }
                self.operations
                    .insert(operation.operation_id.as_str().into(), event.clone());
                self.stop = Some(operation.clone());
            }
            Event::StopObserved {
                operation_id,
                observation,
            } => {
                if self.stop.as_ref().map(|s| &s.operation_id) != Some(operation_id)
                    || self.terminated
                    || *observation == (ManagerObservation::Terminated {})
                {
                    return Err(invalid);
                }
                self.observation = Some(observation.clone());
            }
            Event::TerminationConfirmed { operation_id } => {
                if self.stop.as_ref().map(|s| &s.operation_id) != Some(operation_id)
                    || self.terminated
                {
                    return Err(invalid);
                }
                self.terminated = true;
                self.observation = Some(ManagerObservation::Terminated {});
            }
            Event::Released {
                lease_id,
                operation_id,
            } => {
                let lease = self.lease().ok_or(invalid)?;
                if &lease.spec.lease_id != lease_id
                    || lease.released
                    || self.operations.contains_key(operation_id.as_str())
                    || ((self.pending_request() || self.stop.is_some()) && !self.terminated)
                {
                    return Err(invalid);
                }
                self.leases.get_mut(lease_id.as_str()).unwrap().released = true;
                self.operations
                    .insert(operation_id.as_str().into(), event.clone());
            }
        }
        Ok(())
    }
}
