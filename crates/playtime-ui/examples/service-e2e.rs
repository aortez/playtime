//! Existing desktop backend and widgets against an actual daemon/worker run.
use playtime_client::config::Options;
use playtime_protocol::api::{OrchestrationState, RecipeId};
use playtime_test_support::{Result, service::NormalService};
use playtime_ui::{
    backend::Backend,
    model::{Action, Inspector, Model, Mutation},
    ui::Workbench,
};
use std::{path::PathBuf, time::Duration};
async fn until(
    model: &mut Model,
    backend: &mut Backend,
    predicate: impl Fn(&Model) -> bool,
) -> Result<()> {
    until_within(Duration::from_secs(20), model, backend, predicate).await
}
async fn until_within(
    limit: Duration,
    model: &mut Model,
    backend: &mut Backend,
    predicate: impl Fn(&Model) -> bool,
) -> Result<()> {
    tokio::time::timeout(limit, async {
        loop {
            backend.pump(model);
            if predicate(model) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .map_err(|_| {
        format!(
            "workbench deadline: {:?}; {}",
            model.connection, model.notice
        )
    })?;
    Ok(())
}
#[tokio::main]
async fn main() -> Result<()> {
    let mut cli = None;
    let mut daemon = None;
    let mut bundle = None;
    let mut screenshot: Option<PathBuf> = None;
    let mut docker = false;
    let mut live: Option<&'static str> = None;
    let mut evidence: Option<PathBuf> = None;
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        if flag == "--docker-scripted" {
            docker = true;
            continue;
        }
        if flag == "--live" {
            let endpoint = args.next().ok_or("--live needs an endpoint")?;
            live = Some(Box::leak(endpoint.into_boxed_str()));
            docker = true;
            continue;
        }
        let path = args.next().ok_or("flag needs a path")?.into();
        match flag.as_str() {
            "--cli" => cli = Some(path),
            "--daemon" => daemon = Some(path),
            "--bundle" => bundle = Some(path),
            "--screenshot" => screenshot = Some(path),
            "--evidence" => evidence = Some(path),
            _ => return Err("unknown flag".into()),
        }
    }
    let cli: PathBuf = cli.ok_or("--cli required")?;
    let daemon: PathBuf = daemon.ok_or("--daemon required")?;
    let bundle: PathBuf = bundle.ok_or("--bundle required")?;
    if screenshot.is_some() && !cfg!(feature = "ui-snapshots") {
        return Err("--screenshot requires --features ui-snapshots".into());
    }
    let mut service = if let Some(endpoint) = live {
        NormalService::start_live(&cli, &daemon, &bundle, endpoint).await?
    } else if docker {
        NormalService::start_docker(&cli, &daemon, &bundle).await?
    } else {
        NormalService::start(&cli, &daemon, &bundle).await?
    };
    let config = service.root.join("workbench.toml");
    std::fs::write(
        &config,
        format!(
            "connection_file = {}\n",
            serde_json::to_string(&service.connection.to_string_lossy())?
        ),
    )?;
    let options = Options {
        connection_file: Some(service.connection.clone()),
        config: Some(config),
        state_dir: Some(service.root.join("workbench-state")),
        ..Default::default()
    };
    let mut backend = Backend::start(egui::Context::default());
    let mut model = Model::default();
    model.connect(options.clone());
    until(&mut model, &mut backend, |m| !m.recipes.is_empty()).await?;
    model.select_run(service.run.clone());
    if docker {
        until(&mut model, &mut backend, |m| m.run.is_some()).await?;
        if model.run.as_ref().unwrap().value.orchestration.state == OrchestrationState::Terminal {
            return Err("workbench did not attach before the live run completed".into());
        }
        if live.is_some() {
            // Another checkout's live admission is refused while this worker
            // holds the machine-wide lock.
            service
                .second_live_admission(&cli, &daemon, &bundle)
                .await?;
        }
        // Close the real UI backend while execution is active. Completion below
        // must remain AgentFinished, with every effect and both checkpoints.
        drop(backend);
        service.await_completion(&cli).await?;
        backend = Backend::start(egui::Context::default());
        model.connect(options.clone());
        until(&mut model, &mut backend, |m| !m.recipes.is_empty()).await?;
        model.select_run(service.run.clone());
    }
    until(&mut model, &mut backend, |m| {
        m.run
            .as_ref()
            .is_some_and(|v| v.value.orchestration.state == OrchestrationState::Terminal)
            && m.checkpoints.len() == 2
            && m.window.metrics.records
                == service
                    .view
                    .last_confirmed_frontier
                    .as_ref()
                    .unwrap()
                    .prefix
                    .sequence as usize
    })
    .await?;
    model.inspect_action(Action::Checkpoint(
        service.run.clone(),
        playtime_core::SessionId::new("s2")?,
    ));
    until(&mut model, &mut backend, |m| {
        m.memory.is_some()
            && m.checkpoint
                .as_ref()
                .is_some_and(|c| c.checkpoint.session_id.as_str() == "s2")
    })
    .await?;
    let memory = model.memory.as_ref().ok_or("missing workbench memory")?;
    if live.is_some() {
        if memory.incoming.trim().is_empty()
            || memory.outgoing.trim().is_empty()
            || !memory.notice.contains("Index v1 verified")
        {
            return Err(format!("workbench lost live checkpoint detail: {memory:?}").into());
        }
    } else if !memory.incoming.contains(if docker {
        "Inspect probe.txt, probe.png and the stopped counter in session two."
    } else {
        "Read persistent.txt in the next session."
    }) || !memory.outgoing.contains(if docker {
        "Two sessions complete:"
    } else {
        "The two-session fixture is complete."
    }) || !memory.notice.contains("Index v1 verified")
    {
        return Err(format!("workbench lost checkpoint detail: {memory:?}").into());
    }
    model.inspector = Inspector::Memory;
    if docker {
        for name in ["result.txt", "probe.png"] {
            let artifact = service
                .artifacts
                .iter()
                .find(|a| {
                    a.kind == "file_version"
                        && a.name == name
                        && a.session_id.as_ref().is_some_and(|s| s.as_str() == "s2")
                })
                .ok_or("missing native artifact")?;
            model.inspect_action(Action::Content(
                service.run.clone(),
                artifact.artifact_id.clone(),
                0,
            ));
            until(&mut model, &mut backend, |m| {
                m.content
                    .as_ref()
                    .is_some_and(|c| c.artifact.artifact_id == artifact.artifact_id)
            })
            .await?;
            let content = model.content.as_ref().unwrap();
            if name == "probe.png"
                && content
                    .image
                    .as_ref()
                    .is_none_or(|pixels| pixels.image.size != [32, 32])
            {
                return Err("PNG was not decoded by actual workbench backend".into());
            }
            if name == "result.txt" && content.source.is_none() {
                return Err("text was not inspected by workbench backend".into());
            }
        }
        // A client disconnect never owns cancellation. Reopen the actual backend
        // and observe the same immutable run/frontier before rendering.
        drop(backend);
        backend = Backend::start(egui::Context::default());
        model.connect(options);
        until(&mut model, &mut backend, |m| !m.recipes.is_empty()).await?;
        model.select_run(service.run.clone());
        until(&mut model, &mut backend, |m| {
            m.run
                .as_ref()
                .is_some_and(|r| r.value.orchestration.state == OrchestrationState::Terminal)
                && m.checkpoints.len() == 2
                && m.window.metrics.records
                    == service
                        .view
                        .last_confirmed_frontier
                        .as_ref()
                        .unwrap()
                        .prefix
                        .sequence as usize
        })
        .await?;
        model.inspect_action(Action::Checkpoint(
            service.run.clone(),
            playtime_core::SessionId::new("s2")?,
        ));
        until(&mut model, &mut backend, |m| {
            m.checkpoint
                .as_ref()
                .is_some_and(|c| c.checkpoint.session_id.as_str() == "s2")
        })
        .await?;
        let image = service
            .artifacts
            .iter()
            .find(|a| {
                a.kind == "file_version"
                    && a.name == "probe.png"
                    && a.session_id.as_ref().is_some_and(|s| s.as_str() == "s2")
            })
            .unwrap();
        model.inspect_action(Action::Content(
            service.run.clone(),
            image.artifact_id.clone(),
            0,
        ));
        until(&mut model, &mut backend, |m| {
            m.content.as_ref().is_some_and(|c| c.image.is_some())
        })
        .await?;
        model.inspector = Inspector::Files;
    }
    let mut app = Workbench::default();
    app.model = model;
    let builder = egui_kittest::Harness::builder().with_size([1440.0, 900.0]);
    #[cfg(feature = "ui-snapshots")]
    let builder = builder.wgpu_setup(software_renderer());
    let mut harness = builder.build_ui_state(|ui, app: &mut Workbench| app.draw(ui), app);
    harness.run();
    if harness.state().metrics.laid_out_rows == 0 {
        return Err("workbench rendered no transcript rows".into());
    }
    println!(
        "Workbench: real run {}, {} records, two checkpoints, verified incoming/outgoing memory and index; rendered {} transcript rows",
        service.run,
        harness.state().model.window.metrics.records,
        harness.state().metrics.laid_out_rows
    );
    #[cfg(feature = "ui-snapshots")]
    if let Some(path) = screenshot {
        harness.render()?.save(path)?;
    }
    let mut model = std::mem::take(&mut harness.state_mut().model);
    if live.is_some() {
        // The same live recipe launches from the workbench. Any outcome is
        // recorded and kept; the gate never retries.
        model.choose_recipe(
            RecipeId::new(service.backend.recipe())?,
            playtime_core::RunMode::LiveSmoke,
        );
        until(&mut model, &mut backend, |m| m.can_launch()).await?;
        model.launch();
        until(&mut model, &mut backend, |m| m.launch == Mutation::Accepted).await?;
        let launched = model
            .selected
            .clone()
            .ok_or("workbench launch selected no run")?;
        until_within(Duration::from_secs(300), &mut model, &mut backend, |m| {
            m.run.as_ref().is_some_and(|r| {
                r.value.run_id == launched
                    && r.value.orchestration.state == OrchestrationState::Terminal
            })
        })
        .await?;
        let view = &model.run.as_ref().unwrap().value;
        println!(
            "Workbench launch: live run {} finished {:?}",
            view.run_id,
            view.outcome.as_ref().map(|o| o.reason)
        );
        if let Some(path) = &evidence {
            service.retain_run(&launched, &path.join("workbench-launch-archive"))?;
        }
    }
    drop(backend);
    service.stop().await?;
    if let Some(path) = evidence {
        service.retain_artifact_evidence(&path)?;
        if docker && live.is_none() {
            playtime_test_support::service_loss::worker_loss(&cli, &daemon, &bundle, &path).await?;
        }
    }
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
