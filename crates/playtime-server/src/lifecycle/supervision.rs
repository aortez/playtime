//! Durable manager observations remain separate from worker-confirmed receipts.
use super::{coordinator::Inner, *};

enum Evidence {
    Observation(Option<Box<SupervisorObservation>>),
    StopAccepted,
}

/// The daemon stops a worker without a cancel only in `finish_normal`, after
/// confirming normal completion, or in `finish_failed`, after the worker
/// reported a failed execution. Each records its outcome next. Any other stop
/// means the worker is gone and the daemon stops monitoring it.
pub(super) fn normal_stop(record: &Admission) -> bool {
    record.stop_accepted.is_some() && record.cancel_requested.is_none()
}

/// The worker reported that its execution ended without a normal outcome, and
/// it has no publication pending. A cancellation stops its worker on its own.
pub(super) fn reported_failure(record: &Admission) -> bool {
    record.cancel_requested.is_none()
        && record
            .last_status
            .as_ref()
            .is_some_and(|s| s.failure.is_some() && !s.publication_pending)
}

impl Inner {
    pub(super) async fn observe_supervisor(&self, admission: Admission) -> Result<Admission> {
        let Some(prepared) = admission.prepared.as_ref() else {
            return Ok(admission);
        };
        let result = self
            .supervisor
            .observe(prepared)
            .await
            .map(|observation| Evidence::Observation(observation.map(Box::new)));
        self.save_supervisor(admission, result).await
    }
    pub(super) async fn stop_supervisor(&self, admission: Admission) -> Result<Admission> {
        if admission.prepared.is_none() || !admission.start_attempted {
            return Ok(admission);
        }
        // Once accepted, only query. A lost acknowledgement is resolved by the
        // adapter against the original unit, without restarting its stop timer.
        if admission.stop_accepted.is_some() {
            return self.observe_supervisor(admission).await;
        }
        // Authenticate once if no identity was saved yet. Subsequent stops use
        // durable identity, even across restart or private-filesystem failure.
        let admission = if admission.supervisor.is_none() {
            self.observe_supervisor(admission).await?
        } else {
            admission
        };
        let Some(authenticated) = admission.supervisor.as_ref() else {
            return Ok(admission); // Unauthenticated or explicitly unenforced fixture.
        };
        if authenticated.stopped {
            return Ok(admission);
        }
        let result = self
            .supervisor
            .stop(
                admission.prepared.as_ref().ok_or(Error::Uncertain)?,
                authenticated,
            )
            .await;
        let accepted = result.is_ok();
        let recorded = self
            .save_supervisor(admission, result.map(|()| Evidence::StopAccepted))
            .await?;
        if accepted {
            // Persist acknowledgement before touching diagnostics. Failed
            // termination observation must not erase manager stop acceptance.
            self.observe_supervisor(recorded).await
        } else {
            Ok(recorded)
        }
    }
    async fn save_supervisor(
        &self,
        expected: Admission,
        result: Result<Evidence>,
    ) -> Result<Admission> {
        let now = self.clock.now();
        self.ledger
            .update(
                &expected.run_id.clone(),
                "supervisor_observed",
                move |record| {
                    if record.owner_generation != expected.owner_generation
                        || record.phase == Phase::Terminal
                        || record.prepared.as_ref().map(|p| &p.binding)
                            != expected.prepared.as_ref().map(|p| &p.binding)
                    {
                        return Ok(());
                    }
                    match result {
                        Ok(Evidence::Observation(Some(observation))) => {
                            let mut observation = *observation;
                            if let Some(prior) = &record.supervisor {
                                if prior.manager_id != observation.manager_id
                                    || prior.cgroup != observation.cgroup
                                {
                                    record.phase = Phase::Reconciling;
                                    record.reason = Some(Reason::ControlUncertain);
                                    return Ok(());
                                }
                                if prior.stopped && !observation.stopped {
                                    return Ok(());
                                }
                                if prior.active_state == "deactivating"
                                    && matches!(
                                        observation.active_state.as_str(),
                                        "active" | "activating"
                                    )
                                {
                                    observation.active_state = prior.active_state.clone();
                                }
                                // Unloaded units no longer expose their original clocks.
                                if observation.ready_monotonic_us == 0 {
                                    observation.ready_monotonic_us = prior.ready_monotonic_us;
                                }
                                if observation.stopping_monotonic_us == 0 {
                                    observation.stopping_monotonic_us = prior.stopping_monotonic_us;
                                }
                            }
                            if record.cancel_requested.is_some() {
                                if !observation.stopped
                                    && observation.active_state == "deactivating"
                                    && record.stop_accepted.is_none()
                                {
                                    record.stop_accepted = Some(now);
                                }
                                record.stop_delivery = Some(if observation.stopped {
                                    StopDelivery::Stopped
                                } else if record.stop_accepted.is_some()
                                    || observation.active_state == "deactivating"
                                {
                                    StopDelivery::Accepted
                                } else {
                                    StopDelivery::Requested
                                });
                            }
                            if observation.stopped && !normal_stop(record) {
                                record.phase = Phase::Reconciling;
                                record.reason = Some(Reason::WorkerUnavailable);
                            }
                            record.supervisor = Some(observation);
                        }
                        Ok(Evidence::StopAccepted) => {
                            if record
                                .supervisor
                                .as_ref()
                                .map(|s| (&s.manager_id, &s.cgroup))
                                != expected
                                    .supervisor
                                    .as_ref()
                                    .map(|s| (&s.manager_id, &s.cgroup))
                            {
                                record.phase = Phase::Reconciling;
                                record.reason = Some(Reason::ControlUncertain);
                                return Ok(());
                            }
                            record.stop_accepted.get_or_insert(now);
                            if record.stop_delivery != Some(StopDelivery::Stopped) {
                                record.stop_delivery = Some(StopDelivery::Accepted);
                            }
                        }
                        Ok(Evidence::Observation(None)) => (), // Unenforced fixture port.
                        Err(_) => {
                            if record.cancel_requested.is_some()
                                && record.stop_accepted.is_none()
                                && record.stop_delivery != Some(StopDelivery::Stopped)
                            {
                                record.stop_delivery = Some(StopDelivery::Uncertain);
                            }
                            record.phase = Phase::Reconciling;
                            record.reason = Some(Reason::ControlUncertain);
                        }
                    }
                    Ok(())
                },
            )
            .await
    }
}
