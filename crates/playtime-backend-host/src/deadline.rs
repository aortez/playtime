//! Independent host deadline register. No request execution or accounting.
//! Only a verified terminal can clear a request timer; reconnect cannot renew it.
use crate::{Result, wire::*};
use playtime_protocol::worker::ControlId;
use serde::{Deserialize, Serialize};
use std::os::fd::OwnedFd;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Register {
    pub backend: BackendIdentity,
    pub leases: Vec<LeaseSpec>,
    pub requests: Vec<RequestSpec>,
    pub lease: Option<LeaseSpec>,
    pub request: Option<RequestSpec>,
    pub stop: Option<StopOperation>,
    pub closed: bool,
    pub retired: Vec<StopOperation>,
    pub job_path: Option<String>,
}
impl Register {
    pub fn new(backend: BackendIdentity) -> Self {
        Self {
            backend,
            leases: vec![],
            requests: vec![],
            lease: None,
            request: None,
            stop: None,
            closed: false,
            retired: vec![],
            job_path: None,
        }
    }
    pub fn arm_lease(&mut self, lease: &LeaseSpec, now: u64) -> Result<()> {
        lease.validate().map_err(|_| Failure::Invalid)?;
        if self.backend != lease.backend || self.closed || self.stop.is_some() {
            return Err(Failure::Blocked);
        }
        if let Some(old) = self
            .leases
            .iter()
            .find(|l| l.lease_id == lease.lease_id || l.operation_id == lease.operation_id)
        {
            return if old == lease && self.lease.as_ref() == Some(lease) {
                Ok(())
            } else {
                Err(Failure::Conflict)
            };
        }
        if self.lease.is_some() {
            return Err(Failure::Busy);
        }
        if lease.execution_deadline_ms <= now
            || lease.execution_deadline_ms - now > 120_000
            || lease
                .cleanup_deadline_ms
                .checked_sub(lease.execution_deadline_ms)
                .is_none_or(|n| n != 180_000)
            || lease
                .cleanup_deadline_ms
                .checked_sub(now)
                .is_none_or(|n| n > 300_000)
        {
            return Err(Failure::Expired);
        }
        if self.leases.len() + self.requests.len() >= 9_000 {
            return Err(Failure::Limit);
        }
        self.leases.push(lease.clone());
        self.lease = Some(lease.clone());
        Ok(())
    }
    pub fn arm_request(&mut self, request: &RequestSpec, now: u64) -> Result<()> {
        request.validate().map_err(|_| Failure::Invalid)?;
        let lease = self.lease.as_ref().ok_or(Failure::Blocked)?;
        if self.closed
            || self.stop.is_some()
            || request.lease_id != lease.lease_id
            || request.generation != self.backend.generation
        {
            return Err(Failure::Blocked);
        }
        if let Some(old) = self
            .requests
            .iter()
            .find(|r| r.request_id == request.request_id || r.effect_id == request.effect_id)
        {
            return if old == request && self.request.as_ref() == Some(request) {
                Ok(())
            } else {
                Err(Failure::Conflict)
            };
        }
        if self.request.is_some() {
            return Err(Failure::Busy);
        }
        let ordinal = self
            .requests
            .iter()
            .filter(|r| r.lease_id == lease.lease_id)
            .map(|r| r.ordinal)
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or(Failure::Limit)?;
        if request.ordinal != ordinal
            || request.deadline_ms <= now
            || request.deadline_ms > lease.execution_deadline_ms
        {
            return Err(Failure::Expired);
        }
        if self.leases.len() + self.requests.len() >= 9_000 {
            return Err(Failure::Limit);
        }
        self.requests.push(request.clone());
        self.request = Some(request.clone());
        Ok(())
    }
    pub fn terminal(&mut self, request: &RequestSpec, now: u64) -> Result<()> {
        if self.stop.is_some()
            || self.closed
            || self.request.as_ref() != Some(request)
            || now >= request.deadline_ms
        {
            return Err(Failure::Blocked);
        }
        self.request = None;
        Ok(())
    }
    pub fn release(&mut self, lease: &ControlId, terminated: bool) -> Result<()> {
        if self.lease.as_ref().is_none_or(|l| &l.lease_id != lease) {
            return if self.leases.iter().any(|l| &l.lease_id == lease) {
                Ok(())
            } else {
                Err(Failure::Conflict)
            };
        }
        if (self.request.is_some() || self.closed || self.stop.is_some()) && !terminated {
            return Err(Failure::Blocked);
        }
        self.lease = None;
        Ok(())
    }
    pub fn deadline(&self) -> Option<u64> {
        self.lease.as_ref().map(|l| {
            self.request.as_ref().map_or(l.execution_deadline_ms, |r| {
                r.deadline_ms.min(l.execution_deadline_ms)
            })
        })
    }
    pub fn select_stop(
        &mut self,
        reason: StopReason,
        now: u64,
        supplied: Option<&StopOperation>,
    ) -> Result<Option<StopOperation>> {
        if let Some(original) = &self.stop {
            if supplied
                .is_some_and(|s| s.backend != original.backend || s.lease_id != original.lease_id)
            {
                return Err(Failure::Conflict);
            }
            return Ok(Some(original.clone()));
        }
        self.closed = true;
        let Some(lease) = &self.lease else {
            return Ok(None);
        };
        let operation = if let Some(op) = supplied {
            if op.backend != self.backend
                || op.lease_id != lease.lease_id
                || op.decided_ms > now
                || op.supervisor_deadline_ms
                    != op
                        .decided_ms
                        .checked_add(SUPERVISOR_STOP_MS)
                        .ok_or(Failure::Invalid)?
                        .min(lease.cleanup_deadline_ms)
                || op.observation_deadline_ms
                    != op
                        .decided_ms
                        .checked_add(STOP_OBSERVATION_MS)
                        .ok_or(Failure::Invalid)?
                        .min(lease.cleanup_deadline_ms)
            {
                return Err(Failure::Invalid);
            }
            op.clone()
        } else {
            let decided = now.min(self.deadline().unwrap_or(now));
            StopOperation {
                operation_id: ControlId::new(
                    playtime_backend_guard::digest(
                        format!("host-stop:{}", lease.lease_id.as_str()).as_bytes(),
                    )
                    .as_str()[..32]
                        .into(),
                )
                .map_err(|_| Failure::Invalid)?,
                backend: self.backend.clone(),
                lease_id: lease.lease_id.clone(),
                reason,
                decided_ms: decided,
                supervisor_deadline_ms: decided
                    .checked_add(SUPERVISOR_STOP_MS)
                    .ok_or(Failure::Invalid)?
                    .min(lease.cleanup_deadline_ms),
                observation_deadline_ms: decided
                    .checked_add(STOP_OBSERVATION_MS)
                    .ok_or(Failure::Invalid)?
                    .min(lease.cleanup_deadline_ms),
            }
        };
        self.stop = Some(operation.clone());
        Ok(Some(operation))
    }
}

/// Kernel absolute CLOCK_BOOTTIME timer: suspend consumes the original budget.
/// The owner process is separately supervised; its watchdog is fed only by the
/// same event loop which checks this timer and the held manager.
pub struct AbsoluteTimer(OwnedFd);
impl AbsoluteTimer {
    pub fn new() -> Result<Self> {
        use rustix::time::*;
        let fd = timerfd_create(
            TimerfdClockId::Boottime,
            TimerfdFlags::CLOEXEC | TimerfdFlags::NONBLOCK,
        )
        .map_err(|_| Failure::Blocked)?;
        Ok(Self(fd))
    }
    pub fn arm(&self, deadline_ms: u64) -> Result<()> {
        use rustix::time::*;
        if deadline_ms == 0 {
            return Err(Failure::Expired);
        }
        let time = Timespec {
            tv_sec: (deadline_ms / 1000)
                .try_into()
                .map_err(|_| Failure::Invalid)?,
            tv_nsec: ((deadline_ms % 1000) * 1_000_000) as _,
        };
        timerfd_settime(
            &self.0,
            TimerfdTimerFlags::ABSTIME,
            &Itimerspec {
                it_interval: Timespec {
                    tv_sec: 0,
                    tv_nsec: 0,
                },
                it_value: time,
            },
        )
        .map_err(|_| Failure::Blocked)?;
        Ok(())
    }
    pub async fn expired(&self) -> Result<()> {
        loop {
            match rustix::io::read(&self.0, &mut [0_u8; 8]) {
                Ok(8) => return Ok(()),
                Err(rustix::io::Errno::AGAIN) => {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await
                }
                _ => return Err(Failure::Blocked),
            }
        }
    }
}
