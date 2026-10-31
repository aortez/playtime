use playtime_core::RunMode;
use playtime_protocol::api::*;
use playtime_server::{application::Application, fixture::Fixture};
use playtime_ui::{
    backend::{prepare_preview, prepare_run},
    model::*,
    transcript::{Transcript, View},
    ui::Workbench,
};

pub async fn demo(launcher: bool) -> Workbench {
    let fixture = Fixture::new(ServiceId::new("snapshot-fixture").unwrap());
    let preview = fixture
        .preview(
            RecipeId::new("fixture-example").unwrap(),
            PreviewRequest {
                target: None,
                mode: RunMode::Simulation,
            },
        )
        .await
        .unwrap();
    let mut app = Workbench::default();
    app.model.connection = ConnectionState::Connected;
    app.model.backend = "Fixture replay · scripted HTTP contract; no worker execution".into();
    app.model.service = Some(ServiceId::new("snapshot-fixture").unwrap());
    app.model.endpoint = "http://127.0.0.1:PORT".into();
    app.model.modes = vec![RunMode::Simulation];
    app.model.recipe = Some(preview.recipe_id.clone());
    app.model.recipes = vec![Recipe {
        saved: None,
        recipe_id: preview.recipe_id.clone(),
        name: "Two-session workshop".into(),
        modes: vec![RunMode::Simulation],
    }];
    app.model.preview = Some(prepare_preview(preview));
    let contract: serde_json::Value = serde_json::from_str(include_str!(
        "../../../playtime-server/tests/fixtures/http-contract.json"
    ))
    .unwrap();
    let mut run: RunView = serde_json::from_value(contract["completed_run"].clone()).unwrap();
    run.cleanup = Cleanup::Complete;
    app.model.selected = Some(run.run_id.clone());
    app.model.runs = vec![run.clone()];
    app.model.run = Some(prepare_run(run));
    app.model.launcher = launcher;
    app.model.stale = false;
    app.model.following = false;
    let mut transcript = Transcript::default();
    for line in include_str!(
        "../../../playtime-protocol/tests/fixtures/controller-observations/completed.jsonl"
    )
    .lines()
    {
        let event: playtime_protocol::events::EventEnvelope = serde_json::from_str(line).unwrap();
        // A focused rendering fixture, independently of the full-history tests.
        if event
            .data
            .get("identity")
            .and_then(|id| id.get("request_id"))
            .is_some_and(|id| !id.is_null())
        {
            transcript.apply(event, None).unwrap();
        }
    }
    app.model.window = transcript.window(View::Readable, 0, 64);
    app.model.history = "Reviewed #42 model/tool excerpt · synthetic rendering fixture".into();
    app.model.notice =
        "Saved versions are evidence of publication, not verification of model claims.".into();
    let checkpoints: Page<Checkpoint> =
        serde_json::from_value(contract["checkpoints"].clone()).unwrap();
    app.model.checkpoints = checkpoints.items;
    app.model.checkpoint = app.model.checkpoints.first().cloned();
    app.model.memory = Some(MemoryView {
        incoming: "Goal: inspect the workshop and leave a concise handoff.\n".into(),
        outgoing: "Saved: inspected the image; next session should compare the changed files.\n"
            .into(),
        evidence: "Recorded source references remain separate from model-authored memory.".into(),
        diff: "- Goal: inspect the workshop\n+ Saved: inspected the image".into(),
        notice: "Saved publication · exact source comparison".into(),
    });
    if let Some(checkpoint) = &mut app.model.checkpoint {
        let image = image::RgbaImage::from_fn(128, 96, |x, y| {
            if (x / 16 + y / 16) % 2 == 0 {
                image::Rgba([50, 190, 155, 255])
            } else {
                image::Rgba([35, 55, 75, 255])
            }
        });
        let mut encoded = std::io::Cursor::new(vec![]);
        image
            .write_to(&mut encoded, image::ImageFormat::Png)
            .unwrap();
        let bytes = encoded.into_inner();
        let pixels = playtime_ui::media::decode(&bytes).unwrap();
        if let ContentAvailability::Available {
            artifact_id,
            content,
        } = &mut checkpoint.files[0].content
        {
            // Prepared fake completion: metadata and bytes describe this exact
            // generated checkerboard. Real verification is covered by HTTP tests.
            content.sha256 = playtime_server::fixture::digest(&bytes);
            content.size_bytes = bytes.len() as u64;
            app.model.content = Some(ContentView {
                artifact: Artifact {
                    artifact_id: artifact_id.clone(),
                    name: "synthetic-checkerboard.png".into(),
                    kind: "image".into(),
                    content: content.clone(),
                    session_id: Some(checkpoint.checkpoint.session_id.clone()),
                    published_sequence: checkpoint.checkpoint.published_sequence,
                    frozen_input: false,
                },
                image: Some(std::sync::Arc::new(pixels)),
                source: None,
                animated: false,
                notice: "Synthetic checkerboard · prepared rendering fixture".into(),
            });
        }
    }
    app
}

#[allow(dead_code)]
pub async fn recipe_editor() -> Workbench {
    let root = playtime_test_support::TestRoot::new().unwrap();
    let ledger = playtime_server::lifecycle::Ledger::open(&root.0)
        .await
        .unwrap();
    let recipes = playtime_server::daemon::catalog::load(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../recipes"),
    )
    .unwrap();
    let catalog = playtime_server::catalog::Catalog::new(ledger, recipes).unwrap();
    let definition = catalog
        .definition(&RecipeId::new("curiosity").unwrap())
        .await
        .unwrap();
    let mut app = demo(true).await;
    app.model.mode = RunMode::Experiment;
    app.model.recipe = Some(definition.execution_recipe_id.clone());
    app.model.recipes = vec![Recipe {
        recipe_id: definition.execution_recipe_id.clone(),
        name: definition.name.clone(),
        modes: vec![RunMode::Experiment],
        saved: None,
    }];
    app.model.preview = None;
    app.model.authoring.enabled = true;
    app.model.authoring.definition = Some(playtime_ui::authoring::DefinitionView {
        harness: None,
        model_name: Some("gpt-oss:20b".into()),
        settings: None,
        inherited: serde_json::to_string_pretty(&definition.inherited).unwrap(),
        value: definition,
    });
    app.model.notify(
        "Execution unavailable: the saved configuration remains editable.",
        None,
    );
    app.model.edit_recipe(true);
    let draft = app.model.authoring.draft.as_mut().unwrap();
    draft.name = "Curiosity: a different starting question".into();
    draft.prompt = "Explore a question raised by the seed files. Record what you test, what you learn, and what remains uncertain. Leave a clear handoff for the next session.".into();
    app
}

#[allow(dead_code)]
pub async fn model_settings_editor() -> Workbench {
    let mut app = recipe_editor().await;
    app.model.authoring.draft = None;
    let root = playtime_test_support::TestRoot::new().unwrap();
    let ledger = playtime_server::lifecycle::Ledger::open(&root.0)
        .await
        .unwrap();
    let recipes = playtime_server::daemon::catalog::load(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../recipes"),
    )
    .unwrap();
    let catalog = playtime_server::catalog::Catalog::new(ledger, recipes).unwrap();
    let settings = catalog
        .model_settings(&RecipeId::new("curiosity").unwrap())
        .await
        .unwrap();
    app.model.authoring.definition.as_mut().unwrap().settings = Some(settings);
    app.model.edit_recipe(true);
    app
}

#[allow(dead_code)]
pub async fn harness_editor() -> Workbench {
    let root = playtime_test_support::TestRoot::new().unwrap();
    let ledger = playtime_server::lifecycle::Ledger::open(&root.0)
        .await
        .unwrap();
    let service = ServiceId::new(ledger.service_id().as_str()).unwrap();
    let catalog = playtime_server::catalog::Catalog::new(ledger, vec![]).unwrap();
    let catalog = Fixture::with_catalog(service, catalog)
        .harnesses()
        .await
        .unwrap();
    let mut app = recipe_editor().await;
    let view = app.model.authoring.definition.as_mut().unwrap();
    let parameters =
        serde_json::from_value(view.value.inherited["recipe"]["parameters"].clone()).unwrap();
    view.harness = Some(playtime_ui::harness::RecipeHarness::new(
        RecipeHarnessView {
            execution_recipe_id: view.value.execution_recipe_id.clone(),
            definition_digest: view.value.definition_digest.clone(),
            harness: HarnessView::project(MEMENTO_TURNS_V1, &parameters),
        },
        catalog,
    ));
    app
}

#[allow(dead_code)]
pub async fn long_recipe_runs() -> Workbench {
    let mut app = demo(true).await;
    app.model.recipes[0].name = "Curiosity: compare how persistent workspace notes affect exploration across repeated sessions".into();
    let template = app.model.runs[0].clone();
    app.model.runs = [
        (
            "needs-release",
            OrchestrationState::Reconciling,
            Some("worker_unavailable"),
        ),
        ("running", OrchestrationState::Running, None),
        ("ended", OrchestrationState::Terminal, None),
    ]
    .into_iter()
    .map(|(id, state, reason)| {
        let mut run = template.clone();
        run.run_id = playtime_core::RunId::new(id).unwrap();
        run.orchestration = Orchestration {
            state,
            reason: reason.map(str::to_owned),
            failure: None,
        };
        run
    })
    .collect();
    app
}
