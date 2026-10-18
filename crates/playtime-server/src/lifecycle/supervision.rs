//! Durable manager observations remain separate from worker-confirmed receipts.
use super::{coordinator::Inner, *};
impl Inner {
    pub(super) async fn observe_supervisor(&self, admission: Admission) -> Result<Admission> {
        let Some(prepared) = admission.prepared.as_ref() else {
            return Ok(admission);
        };
        let result = self.supervisor.observe(prepared).await;
        self.save_supervisor(admission, result, false).await
    }
    pub(super) async fn stop_supervisor(&self, admission: Admission) -> Result<Admission> {
        let Some(prepared) = admission.prepared.as_ref() else {
            return Ok(admission);
        };
        if !admission.start_attempted {
            return Ok(admission);
        }
        // Once accepted, only query. A lost acknowledgement is resolved by the
        // adapter against the original unit, without restarting its stop timer.
        let accepted = admission.stop_accepted.is_some();
        let result = if accepted {
            self.supervisor.observe(prepared).await
        } else {
            self.supervisor.stop(prepared).await
        };
        self.save_supervisor(admission, result, !accepted).await
    }
    async fn save_supervisor(
        &self,
        expected: Admission,
        result: Result<Option<SupervisorObservation>>,
        submitted: bool,
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
                        Ok(Some(mut observation)) => {
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
                                    && (submitted || observation.active_state == "deactivating")
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
                            if observation.stopped {
                                record.phase = Phase::Reconciling;
                                record.reason = Some(Reason::WorkerUnavailable);
                            }
                            record.supervisor = Some(observation);
                        }
                        Ok(None) => (), // Explicitly unenforced fixture port.
                        Err(_) => {
                            if record.cancel_requested.is_some() {
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
