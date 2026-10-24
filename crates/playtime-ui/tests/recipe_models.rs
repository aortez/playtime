use egui_kittest::{Harness, kittest::Queryable};
use playtime_client::config::Options;
use playtime_core::RunMode;
use playtime_protocol::api::*;
use playtime_server::{
    catalog::Catalog, daemon::catalog::load, fixture::Fixture, http::Server, lifecycle::Ledger,
    registrations::Registry,
};
use playtime_test_support::{
    TestRoot,
    recipe_models::{boolean_model, retain},
};
use playtime_ui::{backend::Backend, model::*, ui::Workbench};
use std::{path::Path, sync::Arc, time::Duration};

async fn until(m: &mut Model, b: &mut Backend, done: impl Fn(&Model) -> bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            b.pump(m);
            if done(m) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{} {} {:?}", m.notice, m.authoring.error, m.authoring.save));
}
fn field(h: &mut Harness<'_, Workbench>, label: &str, text: &str) {
    h.get_by_label(label).focus();
    h.run();
    h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::A);
    h.run();
    h.get_by_label(label).type_text(text);
    h.run();
}
fn choose(h: &mut Harness<'_, Workbench>, id: &ModelRegistrationId) {
    h.get_by_label("Registered model").scroll_to_me();
    h.run();
    h.get_by_label("Registered model").click();
    h.run();
    h.get_by_label(&format!("Choose registration {id}")).click();
    h.run();
}
#[tokio::test]
async fn widgets_preserve_invalid_settings_reset_distinctly_and_save_the_selected_identity() {
    let root = TestRoot::new().unwrap();
    let ledger = Ledger::open(&root.0).await.unwrap();
    let service = ServiceId::new(ledger.service_id().as_str()).unwrap();
    let catalog = Catalog::new(
        ledger.clone(),
        load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../recipes")).unwrap(),
    )
    .unwrap();
    let registry = Registry::new(ledger, None);
    let boolean = boolean_model();
    retain(&root.0, &boolean);
    let fixture = Arc::new(
        Fixture::with_catalog(service.clone(), catalog.clone())
            .with_model_registry(registry.clone()),
    );
    let credentials = playtime_local::Credentials::initialize(&root.join("credentials")).unwrap();
    let server = Server::new(fixture, credentials.token.clone())
        .unwrap()
        .listen("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    credentials
        .publish(&Connection {
            schema_version: 1,
            service_id: service,
            endpoint: server.endpoint(),
            token_file: credentials.token_path.to_string_lossy().into(),
        })
        .unwrap();
    let options = Options {
        connection_file: Some(credentials.connection_path.clone()),
        state_dir: Some(root.join("state")),
        ..Default::default()
    };
    let mut b = Backend::start(egui::Context::default());
    let mut app = Workbench::default();
    app.model.connect(options);
    until(&mut app.model, &mut b, |m| !m.recipes.is_empty()).await;
    app.model
        .choose_recipe(RecipeId::new("curiosity").unwrap(), RunMode::Experiment);
    until(&mut app.model, &mut b, |m| {
        m.authoring
            .definition
            .as_ref()
            .is_some_and(|d| d.settings.is_some())
            && !m.previewing
    })
    .await;
    app.model.edit_recipe(true);
    until(&mut app.model, &mut b, |m| m.registrations.items.len() == 1).await;
    let original = app
        .model
        .authoring
        .draft
        .as_ref()
        .unwrap()
        .settings
        .as_ref()
        .unwrap()
        .value()
        .unwrap();
    let prompt = app.model.authoring.draft.as_ref().unwrap().prompt.clone();
    let mut h = Harness::builder()
        .with_size([1440.0, 1600.0])
        .build_ui_state(|ui, app: &mut Workbench| app.draw(ui), app);
    h.run();
    field(&mut h, "Temperature", "0.25");
    choose(&mut h, &boolean.summary.registration_id);
    assert!(!h.state().model.authoring.draft.as_ref().unwrap().valid());
    assert!(
        h.state()
            .model
            .authoring
            .draft
            .as_ref()
            .unwrap()
            .request()
            .unwrap_err()
            .contains("reasoning")
    );
    h.get_by_label("Reasoning").click();
    h.run();
    h.get_by_label("On").click();
    h.run();
    assert!(!h.state().model.authoring.draft.as_ref().unwrap().valid());
    field(&mut h, "Context capacity (tokens)", "49152");
    let corrected = h
        .state()
        .model
        .authoring
        .draft
        .as_ref()
        .unwrap()
        .settings
        .as_ref()
        .unwrap()
        .value()
        .unwrap();
    let mut expected = original.clone();
    expected.reasoning = RecipeReasoning::On;
    expected.context_tokens = 49152;
    expected.temperature = 0.25;
    assert_eq!(corrected, expected);
    let before_refresh = h
        .state()
        .model
        .authoring
        .draft
        .as_ref()
        .unwrap()
        .request()
        .unwrap();
    h.state_mut().model.refresh_registrations();
    until(&mut h.state_mut().model, &mut b, |m| {
        !m.registrations.loading
    })
    .await;
    h.run();
    assert_eq!(
        h.state()
            .model
            .authoring
            .draft
            .as_ref()
            .unwrap()
            .request()
            .unwrap(),
        before_refresh
    );
    h.get_by_label("Reset model").click();
    h.run();
    assert!(
        h.state()
            .model
            .authoring
            .draft
            .as_ref()
            .unwrap()
            .model_registration
            .is_none()
    );
    assert!(!h.state().model.authoring.draft.as_ref().unwrap().valid()); // On remains On.
    choose(&mut h, &boolean.summary.registration_id);
    assert_eq!(
        h.state()
            .model
            .authoring
            .draft
            .as_ref()
            .unwrap()
            .settings
            .as_ref()
            .unwrap()
            .value()
            .unwrap(),
        corrected
    );
    field(&mut h, "Temperature", "-");
    h.get_by_label("Reset model").click();
    h.run();
    choose(&mut h, &boolean.summary.registration_id);
    assert!(
        h.state()
            .model
            .authoring
            .draft
            .as_ref()
            .unwrap()
            .request()
            .is_err()
    );
    h.get_by_label("Reset model settings").click();
    h.run();
    let draft = h.state().model.authoring.draft.as_ref().unwrap();
    assert!(draft.model_registration.is_none());
    assert_eq!(draft.settings.as_ref().unwrap().value().unwrap(), original);
    assert_eq!(draft.prompt, prompt);
    choose(&mut h, &boolean.summary.registration_id);
    h.get_by_label("Reasoning").click();
    h.run();
    h.get_by_label("Off").click();
    h.run();
    field(&mut h, "Context capacity (tokens)", "49152");
    h.get_by_label("Save revision").click();
    h.run();
    until(&mut h.state_mut().model, &mut b, |m| {
        m.authoring.draft.is_none()
            && m.authoring
                .definition
                .as_ref()
                .is_some_and(|d| d.value.model_registration.is_some())
            && !m.previewing
    })
    .await;
    assert_eq!(
        h.state()
            .model
            .authoring
            .definition
            .as_ref()
            .unwrap()
            .model_name
            .as_deref(),
        Some(boolean.summary.identity.name.as_str())
    );
    assert!(!h.state().model.can_launch());
    let saved = h
        .state()
        .model
        .authoring
        .definition
        .as_ref()
        .unwrap()
        .value
        .clone();
    assert_eq!(saved.model_registration, Some(boolean.summary.clone()));
    h.state_mut().model.edit_recipe(false);
    h.run();
    field(&mut h, "Temperature", "0.1");
    h.get_by_label("Reset model settings").click();
    h.run();
    assert_eq!(
        h.state()
            .model
            .authoring
            .draft
            .as_ref()
            .unwrap()
            .model_registration,
        saved.model_registration
    );
    h.get_by_label("Discard edits").click();
    h.run();
    until(&mut h.state_mut().model, &mut b, |m| {
        m.authoring.definition.is_some() && !m.previewing
    })
    .await;
    assert_eq!(
        catalog
            .definition(&saved.execution_recipe_id)
            .await
            .unwrap(),
        saved
    );
    assert_eq!(registry.list().await.unwrap().items.len(), 1);
    drop(b);
    server.stop().await.unwrap();
}
