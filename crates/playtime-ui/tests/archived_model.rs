//! Read-only native readback; never launches a worker or inference request.
use playtime_client::config::Options;
use playtime_core::RunId;
use playtime_ui::{
    backend::Backend,
    model::{ConnectionState, Model},
};

#[tokio::test]
#[ignore = "requires PLAYTIME_MODEL_CONNECTION, PLAYTIME_MODEL_RUN and PLAYTIME_MODEL_NAME"]
async fn native_archived_model_readback() {
    let state = playtime_test_support::TestRoot::new().unwrap();
    let options = Options {
        connection_file: Some(std::env::var("PLAYTIME_MODEL_CONNECTION").unwrap().into()),
        state_dir: Some(state.join("client")),
        ..Default::default()
    };
    let run = RunId::new(std::env::var("PLAYTIME_MODEL_RUN").unwrap()).unwrap();
    let expected = std::env::var("PLAYTIME_MODEL_NAME").unwrap();
    let mut backend = Backend::start(egui::Context::default());
    let mut model = Model::default();
    model.connect(options);
    tokio::time::timeout(std::time::Duration::from_secs(45), async {
        let mut selected = false;
        loop {
            backend.pump(&mut model);
            if model.connection == ConnectionState::Connected && !selected {
                model.select_run(run.clone());
                selected = true;
            }
            if let Some(result) = &model.run_model {
                let archived = result.as_ref().unwrap().as_ref().unwrap();
                assert_eq!(archived.name, expected);
                assert!(!archived.details.is_empty());
                println!("Verified archived model: {archived:#?}");
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("archived-model readback deadline");
}
