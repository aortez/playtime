use playtime_client::{Client, config::Options, recipes::RecipeSaveStore};
use playtime_core::RunMode;
use playtime_protocol::api::*;
use playtime_server::{
    catalog::Catalog, daemon::catalog::load, fixture::Fixture, http::Server, lifecycle::Ledger,
};
use playtime_test_support::TestRoot;
use playtime_ui::{backend::Backend, model::*};
use std::{path::Path, sync::Arc, time::Duration};

async fn until(model: &mut Model, backend: &mut Backend, done: impl Fn(&Model) -> bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            backend.pump(model);
            if done(model) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("UI deadline: {} {:?}", model.notice, model.authoring.save));
}
#[tokio::test]
async fn edit_save_reopen_and_recover_offline_through_actual_catalog_and_http() {
    let root = TestRoot::new().unwrap();
    let ledger = Ledger::open(&root.0).await.unwrap();
    let service = ServiceId::new(ledger.service_id().as_str()).unwrap();
    let recipes = load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../recipes")).unwrap();
    let catalog = Catalog::new(ledger, recipes).unwrap();
    let fixture = Arc::new(Fixture::with_catalog(service.clone(), catalog));
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
    let mut backend = Backend::start(egui::Context::default());
    let mut model = Model::default();
    model.connect(options.clone());
    until(&mut model, &mut backend, |m| !m.recipes.is_empty()).await;
    model.choose_recipe(RecipeId::new("curiosity").unwrap(), RunMode::Experiment);
    until(&mut model, &mut backend, |m| {
        m.authoring.definition.is_some() && !m.previewing
    })
    .await;
    assert!(model.preview.is_none());
    assert!(model.notice.contains("execution_unavailable"));
    model.edit_recipe(true);
    let draft = model.authoring.draft.as_mut().unwrap();
    draft.name = "A new recipe".into();
    draft.prompt = "Saved exact prompt A".into();
    model.choose_recipe(RecipeId::new("other").unwrap(), RunMode::Simulation);
    assert_eq!(model.recipe.as_ref().unwrap().as_str(), "curiosity");
    model.save_recipe();
    model.save_recipe();
    assert_eq!(
        model
            .commands
            .iter()
            .filter(|c| matches!(c.action, Action::SaveRecipe(_)))
            .count(),
        1
    );
    until(&mut model, &mut backend, |m| {
        m.authoring
            .definition
            .as_ref()
            .is_some_and(|d| d.value.saved.is_some())
            && !m.previewing
    })
    .await;
    let a = model.authoring.definition.as_ref().unwrap().value.clone();
    assert_eq!(a.prompt, "Saved exact prompt A");
    assert!(!model.can_launch());
    assert!(model.authoring.draft.is_none());
    drop(backend);
    // A separately submitted save has no acknowledged receipt, as after closing
    // the workbench between remote commit and local acknowledgment.
    let client = Client::from_connection(&credentials.connection_path)
        .await
        .unwrap();
    let pending = client
        .prepare_recipe_save(RecipeMutation::Revise {
            recipe_id: a.saved.as_ref().unwrap().recipe_id.clone(),
            request: ReviseRecipeRequest {
                model_settings: None,
                expected_head: a.execution_recipe_id.clone(),
                name: "B".into(),
                prompt: "Prompt B".into(),
            },
        })
        .unwrap();
    RecipeSaveStore::new(&root.join("state"))
        .save(&pending, client.endpoint())
        .unwrap();
    let b = client.save_recipe(&pending).await.unwrap();
    model.apply(Reply {
        tag: model.tag,
        run: None,
        data: Data::Recipes(client.recipes(&PageQuery::default()).await.unwrap()),
    });
    assert_eq!(model.recipe.as_ref(), Some(&a.execution_recipe_id));
    assert!(
        model
            .recipes
            .iter()
            .any(|r| r.recipe_id == b.definition.execution_recipe_id)
    );
    let mut backend = Backend::start(egui::Context::default());
    let mut model = Model::default();
    model.connect(options);
    until(&mut model, &mut backend, |m| {
        !m.authoring.receipts.is_empty()
    })
    .await;
    model.command(Action::RecoverRecipe(pending.key, false));
    until(&mut model, &mut backend, |m| {
        m.authoring
            .definition
            .as_ref()
            .is_some_and(|d| d.value.execution_recipe_id == b.definition.execution_recipe_id)
    })
    .await;
    assert!(model.authoring.receipts.is_empty());
    assert_eq!(model.recipe, Some(b.definition.execution_recipe_id));
    model.edit_recipe(false);
    let draft = model.authoring.draft.as_mut().unwrap();
    draft.prompt = "Unsaved newer draft".into();
    let mut stale = model.tag;
    stale.authoring -= 1;
    model.apply(Reply {
        tag: stale,
        run: None,
        data: Data::Definition(Box::new(playtime_ui::authoring::DefinitionView {
            settings: None,
            value: a,
            inherited: String::new(),
        })),
    });
    assert_eq!(
        model.authoring.draft.as_ref().unwrap().prompt,
        "Unsaved newer draft"
    );
    assert!(!model.can_launch());
    drop(backend);
    server.stop().await.unwrap();
}
