use std::collections::BTreeMap;

use playtime_core::{RunId, RunMode, Sha256Digest};
use playtime_engine::{
    Controller, ControllerConfig, EffectKind, FileInfo, Input, Memory, ModelResponse, Outcome,
    Record,
    simulation::{
        self, ModelEnding, ModelScript, Scenario, SequentialIds, TimedChunk, TimedCompletion,
    },
};
use serde::Serialize;
use serde_json::json;
use sha2::{Digest, Sha256};

#[derive(Serialize)]
pub struct Case {
    pub records: Vec<Record>,
    pub model_effects: BTreeMap<String, u64>,
    pub tool_effects: BTreeMap<u64, u64>,
}

fn scenario() -> Scenario {
    let mut scenario: Scenario =
        serde_json::from_str(include_str!("../fixtures/two-session.json")).unwrap();
    scenario.models[0].chunks[1].chunk["message"]["content"] = json!("Streaming draft.");
    let ModelEnding::Response(response) =
        &mut scenario.models[0].completion.as_mut().unwrap().outcome
    else {
        unreachable!()
    };
    response.message["content"] = json!("Final text replaces the streamed draft.");
    scenario
}

fn executed(scenario: Scenario) -> Case {
    let simulation = simulation::run(scenario).unwrap();
    let model_effects = simulation
        .dispatches
        .iter()
        .filter_map(|dispatch| {
            if let EffectKind::Model(request) = &dispatch.effect.kind {
                Some((request.request_id.as_str().to_owned(), dispatch.effect.id))
            } else {
                None
            }
        })
        .collect();
    let requests: Vec<_> = simulation
        .records
        .iter()
        .filter(|record| matches!(record.kind.as_str(), "tool_dispatch" | "memento_write"))
        .map(|record| record.data["request_sequence"].as_u64().unwrap())
        .collect();
    let effects: Vec<_> = simulation
        .dispatches
        .iter()
        .filter(|dispatch| matches!(dispatch.effect.kind, EffectKind::Tool(_)))
        .map(|dispatch| dispatch.effect.id)
        .collect();
    assert_eq!(requests.len(), effects.len());
    Case {
        records: simulation.records,
        model_effects,
        tool_effects: requests.into_iter().zip(effects).collect(),
    }
}

// Observe a real pending tool intent, then cancel before acknowledging dispatch.
// There is no external workshop invocation, even though both source intent
// records are retained. A failed snapshot tests the absence of a checkpoint.
fn not_dispatched() -> Case {
    let scenario = scenario();
    let reference = simulation::run(scenario.clone()).unwrap();
    let initial_bytes = &reference.artifacts["inputs/simulation-workspace.json"];
    let digest = |bytes: &[u8]| {
        Sha256Digest::new(
            Sha256::digest(bytes)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>(),
        )
        .unwrap()
    };
    let config = ControllerConfig {
        parameters: scenario.parameters,
        protocol: scenario.protocol,
        prompt: scenario.prompt,
        inventory: scenario.inventory,
        initial_workspace: playtime_protocol::archive::ContentRef {
            path: playtime_core::ArchivePath::new("inputs/simulation-workspace.json").unwrap(),
            sha256: digest(initial_bytes),
            size_bytes: initial_bytes.len() as u64,
        },
        initial_files: scenario
            .initial_files
            .iter()
            .map(|(path, content)| {
                (
                    path.clone(),
                    FileInfo {
                        size_bytes: content.len() as u64,
                        mode: 0o644,
                        entry_type: "0".into(),
                        sha256: Some(digest(content.as_bytes())),
                        link: None,
                    },
                )
            })
            .collect(),
    };
    let mut controller = Controller::new(
        config,
        RunId::new("simulation").unwrap(),
        RunMode::Simulation,
        SequentialIds,
        0,
    )
    .unwrap();
    let start = controller.pending().unwrap().id;
    controller
        .advance(0, Input::Dispatched { effect_id: start })
        .unwrap();
    controller
        .advance(
            0,
            Input::Complete {
                effect_id: start,
                outcome: Outcome::Started(Memory::loaded(String::new())),
            },
        )
        .unwrap();
    let model = controller.pending().unwrap().clone();
    let EffectKind::Model(request) = &model.kind else {
        unreachable!()
    };
    let model_effects = BTreeMap::from([(request.request_id.as_str().to_owned(), model.id)]);
    controller
        .advance(
            0,
            Input::Dispatched {
                effect_id: model.id,
            },
        )
        .unwrap();
    controller.advance(1, Input::Complete { effect_id: model.id, outcome: Outcome::Model(ModelResponse {
        message: json!({"role": "assistant", "content": "", "tool_calls": [{"function": {"name": "exec", "arguments": {"command": "fixture:never-executed"}}}]}),
        done: true, generated_tokens: Some(3), prompt_tokens: None,
    }) }).unwrap();
    let tool = controller.pending().unwrap().clone();
    assert!(matches!(tool.kind, EffectKind::Tool(_)));
    let mut records = controller.take_records();
    let request = records
        .iter()
        .find(|record| record.kind == "tool_request")
        .unwrap()
        .sequence;
    controller.advance(2, Input::Cancel).unwrap();
    let snapshot = controller.pending().unwrap().clone();
    assert!(matches!(snapshot.kind, EffectKind::Snapshot { .. }));
    controller
        .advance(
            2,
            Input::Dispatched {
                effect_id: snapshot.id,
            },
        )
        .unwrap();
    controller
        .advance(
            3,
            Input::Complete {
                effect_id: snapshot.id,
                outcome: Outcome::Failed("scripted unavailable snapshot".into()),
            },
        )
        .unwrap();
    let cleanup = controller.pending().unwrap().clone();
    assert!(matches!(cleanup.kind, EffectKind::Cleanup));
    controller
        .advance(
            3,
            Input::Dispatched {
                effect_id: cleanup.id,
            },
        )
        .unwrap();
    controller
        .advance(
            4,
            Input::Complete {
                effect_id: cleanup.id,
                outcome: Outcome::Cleaned(vec![]),
            },
        )
        .unwrap();
    assert!(controller.result().unwrap().last_checkpoint.is_none());
    assert_eq!(
        controller.result().unwrap().usage.unknown_reserved_tokens,
        0
    );
    records.extend(controller.take_records());
    Case {
        records,
        model_effects,
        tool_effects: BTreeMap::from([(request, tool.id)]),
    }
}

pub fn cases() -> BTreeMap<&'static str, Case> {
    let complete = scenario();
    let mut partial = scenario();
    partial.models[2] = ModelScript {
        chunks: vec![TimedChunk {
            after_ms: 10,
            chunk: json!({"message": {"content": "Unfinished session two", "thinking": "Recorded partial thinking"}}),
        }],
        completion: Some(TimedCompletion {
            after_ms: 10,
            outcome: ModelEnding::Error(playtime_engine::ModelFailure::Connection(
                "scripted disconnect after partial output".into(),
            )),
        }),
    };
    BTreeMap::from([
        ("completed", executed(complete)),
        ("not-dispatched", not_dispatched()),
        ("partial-failure", executed(partial)),
    ])
}
