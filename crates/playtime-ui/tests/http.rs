use playtime_client::{Client, config::Options};
use playtime_protocol::api::*;
use playtime_server::http::Server;
use playtime_test_support::{TestRoot, fixture::Script};
use playtime_ui::{backend::Backend, model::*};
use std::{sync::Arc, time::Duration};

async fn until(model: &mut Model, backend: &mut Backend, predicate: impl Fn(&Model) -> bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            backend.pump(model);
            if predicate(model) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "workbench deadline: {:?}, notice {}, launch {:?}",
            model.connection, model.notice, model.launch
        )
    });
}
#[tokio::test]
async fn desktop_client_launch_observe_inspect_and_reopen_uses_only_public_http() {
    let root = TestRoot::new().unwrap();
    let credentials = playtime_local::Credentials::initialize(&root.join("service")).unwrap();
    let script = Arc::new(Script::new(
        ServiceId::new("ui-http").unwrap(),
        "normal".into(),
    ));
    let server = Server::new(script, credentials.token.clone())
        .unwrap()
        .listen("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    credentials
        .publish(&Connection {
            schema_version: 1,
            endpoint: server.endpoint(),
            service_id: ServiceId::new("ui-http").unwrap(),
            token_file: credentials.token_path.to_string_lossy().into(),
        })
        .unwrap();
    let config = root.join("client.toml");
    std::fs::write(
        &config,
        format!(
            "connection_file = {}\n",
            serde_json::to_string(&credentials.connection_path.to_string_lossy()).unwrap()
        ),
    )
    .unwrap();
    let options = Options {
        connection_file: Some(credentials.connection_path.clone()),
        config: Some(config),
        state_dir: Some(root.join("state")),
        ..Default::default()
    };
    let mut backend = Backend::start(egui::Context::default());
    let mut model = Model::default();
    model.connect(options.clone());
    until(&mut model, &mut backend, |m| !m.recipes.is_empty()).await;
    model.choose_recipe(
        RecipeId::new("fixture-example").unwrap(),
        playtime_core::RunMode::Simulation,
    );
    until(&mut model, &mut backend, |m| m.can_launch()).await;
    model.launch();
    model.launch();
    until(&mut model, &mut backend, |m| {
        m.window.metrics.records == 4 && !m.checkpoints.is_empty()
    })
    .await;
    assert!((1..=4 << 20).contains(&model.window.metrics.inbox_bytes_high_water));
    assert!((1..=64).contains(&model.window.metrics.inbox_records_high_water));
    assert!(backend.prepared_this_frame <= 100);
    let run = model.selected.clone().unwrap();
    let client = Client::from_connection(&credentials.connection_path)
        .await
        .unwrap();
    assert_eq!(
        client
            .runs(&PageQuery::default())
            .await
            .unwrap()
            .items
            .len(),
        1
    );
    let session = model.checkpoints[0].checkpoint.session_id.clone();
    model.inspect_action(Action::Checkpoint(run.clone(), session));
    until(&mut model, &mut backend, |m| m.memory.is_some()).await;
    assert!(
        model
            .memory
            .as_ref()
            .unwrap()
            .outgoing
            .contains("fixture saved memory")
    );
    model.inspect_action(Action::Content(
        run.clone(),
        ArtifactId::new("saved-image").unwrap(),
        0,
    ));
    until(&mut model, &mut backend, |m| m.content.is_some()).await;
    assert!(model.content.as_ref().unwrap().image.is_some());
    // The stream's end is the run's notice. A download's notice is about the
    // saved file, so it stays when the launcher opens (#104).
    until(&mut model, &mut backend, |m| {
        m.notice.starts_with("Stream reconciled")
    })
    .await;
    let destination = root.join("saved-image.png");
    model.command(Action::Download(
        run.clone(),
        ArtifactId::new("saved-image").unwrap(),
        destination.clone(),
    ));
    let saved = format!("Verified download saved to {}", destination.display());
    until(&mut model, &mut backend, |m| m.notice == saved).await;
    model.new_experiment();
    assert_eq!(model.notice, saved);
    // Downloading over that file fails. The failure names the destination and
    // stays too.
    model.select_run(run.clone());
    until(&mut model, &mut backend, |m| {
        m.notice.starts_with("Stream reconciled")
    })
    .await;
    model.command(Action::Download(
        run.clone(),
        ArtifactId::new("saved-image").unwrap(),
        destination.clone(),
    ));
    let failed = format!("Download not saved to {}: ", destination.display());
    until(&mut model, &mut backend, |m| m.notice.starts_with(&failed)).await;
    model.new_experiment();
    assert!(model.notice.starts_with(&failed), "{}", model.notice);
    let key = model.saved.as_ref().unwrap().key.clone();
    drop(backend);
    let mut backend = Backend::start(egui::Context::default());
    let mut model = Model::default();
    model.connect(options);
    until(&mut model, &mut backend, |m| {
        m.connection == ConnectionState::Connected
    })
    .await;
    model.command(Action::Recover(key, true));
    until(&mut model, &mut backend, |m| m.saved.is_some()).await;
    assert_eq!(model.saved.as_ref().unwrap().run_id.as_ref(), Some(&run));
    assert_eq!(
        client
            .runs(&PageQuery::default())
            .await
            .unwrap()
            .items
            .len(),
        1
    );
    assert!(client.run(&run).await.unwrap().cancellation.is_none());
    drop(backend);
    server.stop().await.unwrap();
}
