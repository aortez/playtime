use playtime_core::{GenerationUsage, RunId, RunMode, StopReason};
use playtime_engine::{simulation::*, *};
use serde_json::json;

fn setup() -> (Controller<SequentialIds>, ScriptedAdapters) {
    let scenario: Scenario =
        serde_json::from_str(include_str!("fixtures/two-session.json")).unwrap();
    let adapters = ScriptedAdapters::new(&scenario).unwrap();
    let controller = Controller::new(
        adapters.config().clone(),
        RunId::new("simulation").unwrap(),
        RunMode::Simulation,
        SequentialIds,
        0,
    )
    .unwrap();
    (controller, adapters)
}

fn complete_next(
    controller: &mut Controller<SequentialIds>,
    adapters: &mut ScriptedAdapters,
    now: &mut u64,
) {
    controller.take_control_updates();
    controller.take_records();
    let effect = controller.pending().unwrap().clone();
    controller
        .advance(
            *now,
            Input::Dispatched {
                effect_id: effect.id,
            },
        )
        .unwrap();
    adapters.cancel();
    adapters.dispatch(*now, &effect).unwrap();
    while let Some((time, input)) = adapters.pop_input() {
        *now = time;
        controller.advance(time, input).unwrap();
    }
}

fn pending_publication() -> (Controller<SequentialIds>, ScriptedAdapters, u64, Effect) {
    let (mut controller, mut adapters) = setup();
    let mut now = 0;
    while !matches!(
        controller.pending().unwrap().kind,
        EffectKind::PublishCheckpoint(_)
    ) {
        complete_next(&mut controller, &mut adapters, &mut now);
    }
    let effect = controller.pending().unwrap().clone();
    controller.take_control_updates();
    controller.take_records();
    (controller, adapters, now, effect)
}

#[test]
fn checkpoint_receipts_require_dispatch_and_exact_once_identity() {
    let (mut controller, _, now, effect) = pending_publication();
    assert!(
        controller
            .advance(
                now,
                Input::PublicationCommitted {
                    effect_id: effect.id
                }
            )
            .is_err()
    );
    controller
        .advance(
            now,
            Input::Dispatched {
                effect_id: effect.id,
            },
        )
        .unwrap();
    assert!(
        controller
            .advance(
                now,
                Input::PublicationCommitted {
                    effect_id: effect.id + 1
                }
            )
            .is_err()
    );
    controller
        .advance(
            now,
            Input::PublicationCommitted {
                effect_id: effect.id,
            },
        )
        .unwrap();
    assert!(matches!(
        controller.pending().unwrap().kind,
        EffectKind::StartSession { ordinal: 2, .. }
    ));
    assert!(
        controller
            .advance(
                now,
                Input::PublicationCommitted {
                    effect_id: effect.id
                }
            )
            .is_err()
    );
    assert_eq!(
        controller
            .take_control_updates()
            .iter()
            .filter(|update| matches!(update.fact, ControlFact::CheckpointCommitted { .. }))
            .count(),
        1
    );
}

#[test]
fn late_checkpoint_receipt_after_timeout_or_cancel_keeps_first_stop_and_never_starts_another_session()
 {
    for cancel in [false, true] {
        for after_cleanup in [false, true] {
            let (mut controller, mut adapters, mut now, effect) = pending_publication();
            controller
                .advance(
                    now,
                    Input::Dispatched {
                        effect_id: effect.id,
                    },
                )
                .unwrap();
            if cancel {
                controller.advance(now + 1, Input::Cancel).unwrap();
            }
            now = effect.deadline_ms;
            controller.advance(now, Input::Tick).unwrap();
            assert!(matches!(
                controller.pending().unwrap().kind,
                EffectKind::Cleanup
            ));
            let reason = if cancel {
                StopReason::Interrupted
            } else {
                StopReason::InfrastructureFailure
            };
            if after_cleanup {
                complete_next(&mut controller, &mut adapters, &mut now);
            }
            controller
                .advance(
                    now + 7,
                    Input::PublicationCommitted {
                        effect_id: effect.id,
                    },
                )
                .unwrap();
            assert!(controller.advance(now + 6, Input::Tick).is_err());
            if !after_cleanup {
                now += 7;
                complete_next(&mut controller, &mut adapters, &mut now);
            }
            let result = controller.result().unwrap();
            assert_eq!(result.reason, reason);
            assert_eq!(result.sessions_started, 1);
            assert_eq!(result.last_checkpoint.as_ref().unwrap().ordinal, 1);
            assert!(controller.pending().is_none());
            assert!(
                controller
                    .advance(
                        now + 8,
                        Input::PublicationCommitted {
                            effect_id: effect.id
                        }
                    )
                    .is_err()
            );
        }
    }
}

#[test]
fn cancelled_tool_preserves_original_request_binding() {
    let (mut controller, mut adapters) = setup();
    let mut now = 0;
    while !matches!(controller.pending().unwrap().kind, EffectKind::Tool(_)) {
        complete_next(&mut controller, &mut adapters, &mut now);
    }
    let expected = controller.pending_info().unwrap();
    assert!(expected.identity.tool_request_sequence.is_some());
    controller.take_control_updates();
    controller.advance(now, Input::Cancel).unwrap();
    let updates = controller.take_control_updates();
    assert!(updates.iter().any(
        |update| matches!(&update.fact, ControlFact::EffectNotDispatched(info) if *info == expected)
    ));
    assert!(
        !updates
            .iter()
            .any(|update| matches!(update.fact, ControlFact::RequestSettled { .. }))
    );
}

#[test]
fn archive_capacity_stops_with_log_limit_and_distinguishes_unstarted_from_unknown() {
    for dispatched in [false, true] {
        let (mut controller, mut adapters) = setup();
        let mut now = 0;
        complete_next(&mut controller, &mut adapters, &mut now);
        let model = controller.pending().unwrap().clone();
        assert!(matches!(model.kind, EffectKind::Model(_)));
        controller.take_control_updates();
        if dispatched {
            controller
                .advance(
                    now,
                    Input::Dispatched {
                        effect_id: model.id,
                    },
                )
                .unwrap();
        }
        controller
            .advance(now, Input::ExternalLogBudgetExhausted)
            .unwrap();
        let updates = controller.take_control_updates();
        assert!(
            updates.iter().any(|update| matches!(
                update.fact,
                ControlFact::StopDecided(StopReason::LogLimit)
            ))
        );
        assert_eq!(
            updates
                .iter()
                .filter(|update| matches!(update.fact, ControlFact::RequestNotDispatched { .. }))
                .count(),
            usize::from(!dispatched)
        );
        assert_eq!(
            updates
                .iter()
                .filter(|update| matches!(
                    update.fact,
                    ControlFact::RequestSettled {
                        usage: GenerationUsage::Unknown { .. },
                        ..
                    }
                ))
                .count(),
            usize::from(dispatched)
        );
    }
}

#[test]
fn measured_zero_survives_raw_settlement_suppression() {
    let (_, adapters) = setup();
    let mut config = adapters.config().clone();
    config.parameters.limits.log_bytes = 100_000.try_into().unwrap();
    let mut controller = Controller::new(
        config,
        RunId::new("simulation").unwrap(),
        RunMode::Simulation,
        SequentialIds,
        0,
    )
    .unwrap();
    let mut adapters = adapters;
    // Retain startup records to account their exact ASCII bytes.
    let mut records = controller.take_records();
    let start = controller.pending().unwrap().clone();
    controller
        .advance(
            0,
            Input::Dispatched {
                effect_id: start.id,
            },
        )
        .unwrap();
    adapters.dispatch(0, &start).unwrap();
    let (_, input) = adapters.pop_input().unwrap();
    controller.advance(0, input).unwrap();
    records.extend(controller.take_records());
    let used: usize = records
        .iter()
        .map(|record| serde_json::to_vec(record).unwrap().len() + 1)
        .sum();
    let model = controller.pending().unwrap().clone();
    controller
        .advance(
            0,
            Input::Dispatched {
                effect_id: model.id,
            },
        )
        .unwrap();
    let probe = Record {
        sequence: records.last().unwrap().sequence + 1,
        elapsed_ms: 0,
        session_id: records.last().unwrap().session_id.clone(),
        kind: "model_chunk".into(),
        data: json!({"call": 1, "chunk": {"message": {"content": ""}}}),
    };
    let overhead = serde_json::to_vec(&probe).unwrap().len() + 1;
    let text = "x".repeat(100_000 - 65_536 - used - overhead - 1);
    controller
        .advance(
            0,
            Input::Chunk {
                effect_id: model.id,
                chunk: json!({"message": {"content": text}}),
            },
        )
        .unwrap();
    controller.take_records();
    controller.take_control_updates();
    controller
        .advance(
            0,
            Input::Complete {
                effect_id: model.id,
                outcome: Outcome::Model(ModelResponse {
                    message: json!({"role": "assistant", "content": ""}),
                    done: true,
                    generated_tokens: Some(0),
                    prompt_tokens: None,
                }),
            },
        )
        .unwrap();
    assert!(
        !controller
            .take_records()
            .iter()
            .any(|record| record.kind == "request_settled")
    );
    let updates = controller.take_control_updates();
    assert!(updates.iter().any(|update| matches!(&update.fact, ControlFact::ObservationOmitted { kind, .. } if kind == "request_settled")));
    assert!(updates.iter().any(|update| matches!(
        update.fact,
        ControlFact::RequestSettled {
            usage: GenerationUsage::Known {
                generated_tokens: 0
            },
            ..
        }
    )));
    assert!(
        updates
            .iter()
            .any(|update| matches!(update.fact, ControlFact::StopDecided(StopReason::LogLimit)))
    );
}

#[test]
fn bounded_control_queue_requires_draining_without_dropping_facts() {
    let (_, adapters) = setup();
    let mut config = adapters.config().clone();
    config.parameters.limits.work_turns_per_session = 100.try_into().unwrap();
    config.parameters.limits.context_tokens = 1_000_000.try_into().unwrap();
    let mut controller = Controller::new(
        config,
        RunId::new("simulation").unwrap(),
        RunMode::Simulation,
        SequentialIds,
        0,
    )
    .unwrap();
    let mut settled = 0;
    loop {
        let effect = controller.pending().unwrap().clone();
        if controller
            .advance(
                0,
                Input::Dispatched {
                    effect_id: effect.id,
                },
            )
            .is_err()
        {
            assert_eq!(controller.pending(), Some(&effect));
            break;
        }
        let outcome = match effect.kind {
            EffectKind::StartSession { .. } => Outcome::Started(Memory::loaded(String::new())),
            EffectKind::Model(_) => Outcome::Model(ModelResponse {
                message: json!({"role":"assistant","content":""}),
                done: true,
                generated_tokens: Some(0),
                prompt_tokens: None,
            }),
            _ => panic!("unexpected stop before filling the control queue"),
        };
        let input = Input::Complete {
            effect_id: effect.id,
            outcome,
        };
        if controller.advance(0, input.clone()).is_err() {
            let updates = controller.take_control_updates();
            assert!(updates.len() <= CONTROL_QUEUE_CAPACITY);
            assert_eq!(
                updates
                    .iter()
                    .filter(|update| matches!(update.fact, ControlFact::RequestSettled { .. }))
                    .count(),
                settled
            );
            controller.advance(0, input).unwrap();
            return;
        }
        if matches!(effect.kind, EffectKind::Model(_)) {
            settled += 1;
        }
        controller.take_records();
    }
    let updates = controller.take_control_updates();
    assert!(updates.len() <= CONTROL_QUEUE_CAPACITY);
    assert!(settled > 20);
    assert_eq!(
        updates
            .iter()
            .filter(|update| matches!(update.fact, ControlFact::RequestSettled { .. }))
            .count(),
        settled
    );
    let effect = controller.pending().unwrap().clone();
    controller
        .advance(
            0,
            Input::Dispatched {
                effect_id: effect.id,
            },
        )
        .unwrap();
}
