//! Opt-in native registered-model smoke against an already running daemon.
//! Does not start Ollama, change its models, or substitute for the GPT-OSS gate.
use playtime_client::{Client, config::Options};
use playtime_core::{RunId, RunMode};
use playtime_protocol::api::*;
use playtime_test_support::Result;
use playtime_ui::{
    backend::Backend,
    model::{Action, Inspector, Model, Mutation},
    ui::Workbench,
};
use std::{
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
    time::Duration,
};

async fn until(
    model: &mut Model,
    backend: &mut Backend,
    limit: u64,
    done: impl Fn(&Model) -> bool,
) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(limit), async {
        loop {
            backend.pump(model);
            if done(model) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .map_err(|_| {
        format!(
            "workbench deadline: {:?}; {}; {:?}; {}",
            model.connection, model.notice, model.launch, model.authoring.error
        )
    })?;
    Ok(())
}
fn retain(path: &Path, value: &impl serde::Serialize) -> Result<()> {
    std::fs::write(path, serde_json::to_vec_pretty(value)?)?;
    Ok(())
}
async fn completed(client: &Client, run: &RunId, path: &Path) -> Result<RunView> {
    let result = tokio::time::timeout(Duration::from_secs(300), async {
        loop {
            let view = client.run(run).await?;
            retain(path, &view)?;
            if view.orchestration.state == OrchestrationState::Terminal {
                return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(view);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await??;
    println!(
        "Native run {run}: {:?}",
        result.outcome.as_ref().map(|o| o.reason)
    );
    if result
        .usage
        .as_ref()
        .is_none_or(|u| u.known_generated_tokens == 0)
    {
        return Err("native run produced no confirmed model usage; evidence retained".into());
    }
    Ok(result)
}

async fn frozen_configuration(
    client: &Client,
    run: &RunId,
    preview: &Preview,
    path: &Path,
) -> Result<()> {
    let mut query = PageQuery::default();
    loop {
        let page = client.artifacts(run, &query).await?;
        if let Some(artifact) = page
            .items
            .iter()
            .find(|a| a.content.path.as_str() == "inputs/scenario.json")
        {
            client
                .download_to_path(
                    run,
                    artifact,
                    path,
                    &tokio_util::sync::CancellationToken::new(),
                )
                .await?;
            let frozen: serde_json::Value = serde_json::from_slice(&std::fs::read(path)?)?;
            if frozen != preview.effective_configuration {
                return Err("archive configuration differs from exact selected preview".into());
            }
            let archived = client
                .run(run)
                .await?
                .harness
                .ok_or("run lacks archived harness")?;
            if archived.configuration != artifact.content
                || Some(&archived.harness) != preview.harness.as_ref()
                || HarnessView::from_configuration(&frozen)?.as_ref() != Some(&archived.harness)
            {
                return Err(
                    "run inspector harness differs from verified archived configuration".into(),
                );
            }
            return Ok(());
        }
        query.cursor = page.next_cursor;
        if query.cursor.is_none() {
            return Err("archive has no frozen configuration".into());
        }
    }
}

async fn verify_continuous(client: &Client, run: &RunId, path: &Path) -> Result<()> {
    use playtime_core::{RequestPurpose, StopReason};
    let view = client.run(run).await?;
    if view
        .outcome
        .as_ref()
        .is_none_or(|o| o.reason != StopReason::AgentFinished)
        || view.cleanup != Cleanup::Complete
        || view.checkpoint.is_some()
    {
        return Err("continuous run did not finish with final-only capture and cleanup".into());
    }
    let mut query = PageQuery::default();
    let mut events = Vec::new();
    loop {
        let page = client.history(run, &query).await?;
        events.extend(page.events);
        query.cursor = page.next_cursor;
        if query.cursor.is_none() {
            break;
        }
    }
    retain(path, &events)?;
    let applied = events
        .iter()
        .filter(|e| {
            e.data
                .pointer("/fact/data/observation_kind")
                .and_then(serde_json::Value::as_str)
                == Some("compaction_applied")
        })
        .count();
    let accounting = view
        .request_accounting
        .ok_or("purpose accounting unavailable")?;
    if applied < 2
        || accounting
            .by_purpose
            .get(&RequestPurpose::Compaction)
            .is_none_or(|a| a.settled_calls != applied as u64 || a.usage.unknown_requests != 0)
    {
        return Err("compaction count and purpose accounting disagree".into());
    }
    let mut query = PageQuery::default();
    let mut captures = 0;
    let mut result = false;
    loop {
        let page = client.artifacts(run, &query).await?;
        for artifact in page.items {
            captures += usize::from(artifact.kind == "final_workspace");
            if artifact.kind == "file_version" && artifact.name == "result.txt" {
                let mut bytes = Vec::new();
                client
                    .download(
                        run,
                        &artifact,
                        &mut bytes,
                        &tokio_util::sync::CancellationToken::new(),
                    )
                    .await?;
                result = bytes == b"continuity-ok\n" && artifact.session_id.is_none();
            }
        }
        query.cursor = page.next_cursor;
        if query.cursor.is_none() {
            break;
        }
    }
    if captures != 1 || !result {
        return Err("final continuity result or capture missing".into());
    }
    println!(
        "Verified saved continuous recipe: {applied} compactions, final continuity file, complete cleanup"
    );
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let connection = PathBuf::from(args.next().ok_or("connection file required")?);
    let evidence = PathBuf::from(args.next().ok_or("evidence directory required")?);
    let continuous = match args.next().as_deref() {
        None => false,
        Some("--continuous") => true,
        _ => return Err("usage: registered-model-e2e CONNECTION EVIDENCE [--continuous]".into()),
    };
    if args.next().is_some() {
        return Err("unexpected arguments".into());
    }
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&evidence)?;
    let client = Client::from_connection(&connection).await?;
    if !client
        .capabilities()
        .features
        .iter()
        .any(|f| f == RECIPE_MODEL_EXECUTION)
    {
        return Err("daemon lacks registered-model execution".into());
    }
    let review = client
        .review_model(&ModelInspectRequest {
            name: "qwen3:1.7b".into(),
            expected_digest: None,
        })
        .await?;
    retain(&evidence.join("model-review.json"), &review)?;
    let pending =
        client.prepare_model_registration(review.request.ok_or("model is not registrable")?)?;
    retain(&evidence.join("pending-registration.json"), &pending)?;
    let registered = client.register_model(&pending).await?;
    retain(&evidence.join("registration.json"), &registered)?;
    let source = client
        .recipe_definition(&RecipeId::new("dev-smoke")?)
        .await?;
    let mut settings = client.recipe_model_settings(&source).await?.settings;
    settings.reasoning = RecipeReasoning::Off;
    let pending = client.prepare_recipe_save(RecipeMutation::Clone {
        request: CloneRecipeRequest {
            compaction: continuous.then(|| CompactionSettings { summary_tokens: 512, instructions: "Summarize the checked progress in at most 300 characters. Preserve the task, next command and any uncertainty. Return only the summary; no tools.".into(), ..Default::default() }),
            harness: Some(HarnessId::new(if continuous { CONTINUOUS_COMPACTION_V1 } else { MEMENTO_TURNS_V1 })?),
            source: RecipeSource {
                execution_recipe_id: source.execution_recipe_id,
                definition_digest: source.definition_digest,
            },
            name: if continuous { "Local Qwen: continuous compaction" } else { "Local Qwen: two-session smoke" }.into(),
            prompt: source.prompt,
            model_settings: Some(settings),
            model_registration: Some(registered.registration.registration_id.clone()),
        },
    })?;
    retain(&evidence.join("pending-save.json"), &pending)?;
    let saved = client.save_recipe(&pending).await?;
    retain(&evidence.join("saved-recipe.json"), &saved)?;
    let selected_harness = client.recipe_harness(&saved.definition).await?.harness;
    let id = saved.definition.execution_recipe_id;
    let mut preview = client.preview(&id, RunMode::Experiment).await?;
    retain(&evidence.join("preview.json"), &preview)?;
    if preview.harness.as_ref() != Some(&selected_harness) {
        return Err("preview differs from saved harness selection/settings".into());
    }
    if preview.model_identity["registration"] != serde_json::to_value(&registered.registration)?
        || preview.model_identity["name"] != "qwen3:1.7b"
    {
        return Err("preview did not bind the selected identity".into());
    }
    let mut backend = Backend::start(egui::Context::default());
    let mut model = Model::default();
    model.connect(Options {
        connection_file: Some(connection),
        state_dir: Some(evidence.join("workbench-state")),
        ..Default::default()
    });
    until(&mut model, &mut backend, 30, |m| !m.recipes.is_empty()).await?;
    model.choose_recipe(id.clone(), RunMode::Experiment);
    until(&mut model, &mut backend, 30, |m| m.can_launch()).await?;
    capture(
        &mut model,
        &mut backend,
        &evidence.join("selected-model-preview.png"),
    )
    .await?;

    let pending = client.prepare_launch(
        LaunchRequest {
            recipe_id: id.clone(),
            mode: RunMode::Experiment,
            preview_revision: preview.preview_revision.clone(),
        },
        None,
    )?;
    retain(&evidence.join("pending-launch.json"), &pending)?;
    let accepted = client.launch(&pending).await?;
    retain(&evidence.join("api-acceptance.json"), &accepted)?;
    let replay = client.launch(&pending).await?;
    if !replay.replayed || replay.run_id != accepted.run_id {
        return Err("launch replay changed run identity".into());
    }
    completed(
        &client,
        &accepted.run_id,
        &evidence.join("api-outcome.json"),
    )
    .await?;
    frozen_configuration(
        &client,
        &accepted.run_id,
        &preview,
        &evidence.join("api-configuration.json"),
    )
    .await?;

    if continuous {
        verify_continuous(
            &client,
            &accepted.run_id,
            &evidence.join("api-history.json"),
        )
        .await?;
    }
    // A fresh preview and launch through the actual workbench backend/receipts.
    model.choose_recipe(id, RunMode::Experiment);
    until(&mut model, &mut backend, 30, |m| m.can_launch()).await?;
    if continuous {
        model.edit_recipe(false);
        let draft = model.authoring.draft.as_mut().ok_or("missing editor")?;
        draft.compaction.trigger = "70".into();
        capture(
            &mut model,
            &mut backend,
            &evidence.join("compaction-editor.png"),
        )
        .await?;
        model.save_recipe();
        until(&mut model, &mut backend, 30, |m| {
            m.authoring.draft.is_none() && m.can_launch()
        })
        .await?;
        preview = model
            .preview
            .as_ref()
            .ok_or("missing revised preview")?
            .value
            .clone();
        if preview
            .harness
            .as_ref()
            .and_then(|h| h.compaction.as_ref())
            .is_none_or(|s| s.trigger_percent != 70)
        {
            return Err("workbench revision did not freeze the edited trigger".into());
        }
        retain(&evidence.join("workbench-preview.json"), &preview)?;
    }
    model.launch();
    until(&mut model, &mut backend, 30, |m| {
        m.launch == Mutation::Accepted
    })
    .await?;
    let run = model.selected.clone().ok_or("UI launch selected no run")?;
    let ended = completed(&client, &run, &evidence.join("workbench-outcome.json")).await?;
    frozen_configuration(
        &client,
        &run,
        &preview,
        &evidence.join("workbench-configuration.json"),
    )
    .await?;
    let sequence = ended
        .last_confirmed_frontier
        .as_ref()
        .ok_or("no terminal frontier")?
        .prefix
        .sequence;
    model.command(Action::Runs(None));
    until(&mut model, &mut backend, 60, |m| {
        m.run.as_ref().is_some_and(|r| {
            r.value.run_id == run && r.value.orchestration.state == OrchestrationState::Terminal
        }) && m.window.metrics.records as u64 == sequence
            && m.runs
                .iter()
                .any(|r| r.run_id == run && r.orchestration.state == OrchestrationState::Terminal)
    })
    .await?;
    if continuous {
        verify_continuous(&client, &run, &evidence.join("workbench-history.json")).await?;
        model.inspector = Inspector::Files;
        model.inspect_action(Action::FinalFiles(run.clone()));
        until(&mut model, &mut backend, 30, |m| m.final_files.is_some()).await?;
        if model.final_files.as_ref().is_none_or(|(files, complete)| {
            !complete || !files.iter().any(|a| a.name == "result.txt")
        }) {
            return Err("workbench did not expose final file".into());
        }
    }
    if let Some(checkpoint) = &ended.checkpoint {
        model.inspect_checkpoint(checkpoint.session_id.clone(), Inspector::Memory);
        until(&mut model, &mut backend, 20, |m| m.memory.is_some()).await?;
    }
    capture(
        &mut model,
        &mut backend,
        &evidence.join("selected-model-run.png"),
    )
    .await?;
    model.inspector = Inspector::Details;
    model.source = None;
    model.selected_row = None;
    capture(
        &mut model,
        &mut backend,
        &evidence.join("archived-harness.png"),
    )
    .await?;
    // Outcomes are retained even when the tiny model does not complete its task.
    // This gate checks execution plumbing; the GPT-OSS acceptance gate is separate.
    Ok(())
}

async fn capture(model: &mut Model, backend: &mut Backend, path: &Path) -> Result<()> {
    let mut app = Workbench::default();
    app.model = std::mem::take(model);
    let builder = egui_kittest::Harness::builder().with_size([1440.0, 1000.0]);
    #[cfg(feature = "ui-snapshots")]
    let builder = builder.wgpu_setup(software_renderer());
    let mut h = builder.build_ui_state(|ui, app: &mut Workbench| app.draw(ui), app);
    // Drawing requests visible boundary text. Let the real backend settle it
    // before capture, just as successive desktop frames do.
    for _ in 0..20 {
        h.run();
        backend.pump(&mut h.state_mut().model);
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    h.run();
    #[cfg(feature = "ui-snapshots")]
    h.render()?.save(path)?;
    #[cfg(not(feature = "ui-snapshots"))]
    let _ = path;
    *model = std::mem::take(&mut h.state_mut().model);
    Ok(())
}
#[cfg(feature = "ui-snapshots")]
fn software_renderer() -> eframe::egui_wgpu::WgpuSetup {
    use eframe::egui_wgpu::{WgpuSetup, WgpuSetupCreateNew, wgpu};
    let mut setup = WgpuSetupCreateNew::without_display_handle();
    setup.instance_descriptor.backends = wgpu::Backends::VULKAN;
    setup.native_adapter_selector = Some(std::sync::Arc::new(|adapters, _| {
        adapters
            .iter()
            .find(|adapter| {
                let info = adapter.get_info();
                info.device_type == wgpu::DeviceType::Cpu
                    && info.backend == wgpu::Backend::Vulkan
                    && info.name.contains("llvmpipe")
                    && info.driver_info == "Mesa 26.0.8-1ubuntu0.3 (LLVM 21.1.8)"
            })
            .cloned()
            .ok_or_else(|| {
                "Pinned Mesa lavapipe 26.0.8-1ubuntu0.3 / LLVM 21.1.8 CPU Vulkan adapter required"
                    .into()
            })
    }));
    WgpuSetup::CreateNew(setup)
}
