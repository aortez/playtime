//! Separate-process public API/CLI smoke gate. Build once in the shell wrapper.
use playtime_client::{Client, config::Options};
use playtime_core::{RunId, RunMode};
use playtime_protocol::api::*;
use playtime_test_support::{Result, Service, TestRoot, failure_bundle, run_command};
use playtime_ui::{backend::Backend, model::*};
use std::{path::Path, time::Duration};

async fn until(
    model: &mut Model,
    backend: &mut Backend,
    predicate: impl Fn(&Model) -> bool,
) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            backend.pump(model);
            if predicate(model) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .map_err(|_| format!("UI deadline: {:?}; {}", model.connection, model.notice))?;
    Ok(())
}
async fn case(cli: &Path, fixture: &Path, scenario: &str) -> Result<()> {
    let root = TestRoot::new()?;
    let mut records = vec![];
    let service =
        Service::start(fixture, &root, "service", scenario, "ui-e2e-fixture", None).await?;
    let result = async {
        let config = root.join("ui.toml");
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
            state_dir: Some(root.join("ui-state")),
            ..Default::default()
        };
        let client = Client::from_connection(&service.connection).await?;
        let mut backend = Backend::start(egui::Context::default());
        let mut model = Model::default();
        model.connect(options.clone());
        until(&mut model, &mut backend, |m| !m.recipes.is_empty()).await?;
        let run = if scenario == "normal" {
            let output = run_command(
                cli,
                &root,
                Some(&service.connection),
                [
                    "run",
                    "fixture-example",
                    "--mode",
                    "simulation",
                    "--wait",
                    "--format",
                    "json",
                ],
                &mut records,
            )
            .await?;
            output.expect(0)?;
            let runs = client.runs(&PageQuery::default()).await?;
            let run = runs
                .items
                .first()
                .ok_or("CLI launch missing")?
                .run_id
                .clone();
            model.select_run(run.clone());
            run
        } else {
            model.choose_recipe(RecipeId::new("fixture-example")?, RunMode::Simulation);
            until(&mut model, &mut backend, Model::can_launch).await?;
            model.launch();
            model.launch();
            until(&mut model, &mut backend, |m| {
                matches!(m.launch, Mutation::Unresolved(_))
            })
            .await?;
            let key = model
                .saved
                .as_ref()
                .ok_or("lost launch receipt missing")?
                .key
                .clone();
            drop(backend);
            backend = Backend::start(egui::Context::default());
            model = Model::default();
            model.connect(options);
            until(&mut model, &mut backend, |m| {
                m.connection == ConnectionState::Connected && !m.receipts.is_empty()
            })
            .await?;
            if client.runs(&PageQuery::default()).await?.items.len() != 1 {
                return Err("reopen changed admission count".into());
            }
            model.command(Action::Recover(key, true));
            until(&mut model, &mut backend, |m| m.selected.is_some()).await?;
            model.selected.clone().unwrap()
        };
        until(&mut model, &mut backend, |m| {
            m.window.metrics.records == 4 && !m.checkpoints.is_empty()
        })
        .await?;
        let session = model.checkpoints[0].checkpoint.session_id.clone();
        model.inspect_action(Action::Checkpoint(run.clone(), session));
        until(&mut model, &mut backend, |m| m.memory.is_some()).await?;
        if !model
            .memory
            .as_ref()
            .unwrap()
            .outgoing
            .contains("fixture saved memory")
        {
            return Err("saved memory missing".into());
        }
        model.inspect_action(Action::Content(
            run.clone(),
            ArtifactId::new("saved-image")?,
            0,
        ));
        until(&mut model, &mut backend, |m| m.content.is_some()).await?;
        if model.content.as_ref().unwrap().image.is_none() {
            return Err("verified image missing".into());
        }
        model.select_run(RunId::new("nonexistent")?);
        drop(backend);
        let view = client.run(&run).await?;
        if view.cancellation.is_some() {
            return Err("client detach cancelled the experiment".into());
        }
        if client.runs(&PageQuery::default()).await?.items.len() != 1 {
            return Err("duplicate experiment admitted".into());
        }
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    }
    .await;
    if let Err(error) = &result {
        let bundle = failure_bundle(&root, &records, &error.to_string())?;
        eprintln!("Redacted failure evidence: {}", bundle.display());
    }
    service.stop().await?;
    result
}
#[tokio::main]
async fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() != 6 || args[0] != "--backend" || args[2] != "--cli" || args[4] != "--fixture" {
        eprintln!("usage: ui-e2e --backend fixture|simulation --cli PATH --fixture PATH");
        std::process::exit(2);
    }
    if args[1] == "simulation" {
        eprintln!(
            "Simulation gate unavailable: #42 archive driver is merged, but #20 production service and #43 recovery are required. No fixture fallback; #22 remains open."
        );
        std::process::exit(1);
    }
    if args[1] != "fixture" {
        std::process::exit(2);
    }
    let suite = async {
        for scenario in ["normal", "lost-launch"] {
            tokio::time::timeout(
                Duration::from_secs(30),
                case(Path::new(&args[3]), Path::new(&args[5]), scenario),
            )
            .await
            .map_err(|_| "UI case exceeded 30 seconds")??;
            println!(
                "PASS {scenario}: separate CLI/service processes and desktop backend over HTTP"
            );
        }
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    };
    match tokio::time::timeout(Duration::from_secs(180), suite).await {
        Ok(Ok(())) => println!("2 UI process fixture cases passed; not production simulation"),
        result => {
            eprintln!("UI fixture gate failed: {result:?}");
            std::process::exit(1);
        }
    }
}
