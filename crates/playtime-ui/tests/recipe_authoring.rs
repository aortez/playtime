use playtime_client::{Client, config::Options, recipes::RecipeSaveStore};
use playtime_core::RunMode;
use playtime_protocol::api::*;
use playtime_server::{
    catalog::Catalog,
    daemon::catalog::load,
    fixture::Fixture,
    http::{RunningServer, Server},
    lifecycle::Ledger,
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
/// A catalog service over HTTP, without execution, and the workbench's options.
async fn service(root: &TestRoot) -> (RunningServer, Options) {
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
    (server, options)
}
#[tokio::test]
async fn edit_save_reopen_and_recover_offline_through_actual_catalog_and_http() {
    let root = TestRoot::new().unwrap();
    let (server, options) = service(&root).await;
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
    let harness = model
        .authoring
        .definition
        .as_ref()
        .unwrap()
        .harness
        .as_ref()
        .unwrap();
    assert_eq!(harness.view.harness.id, MEMENTO_TURNS_V1);
    assert_eq!(harness.catalog.harnesses.len(), 2);
    model.edit_recipe(true);
    let draft = model.authoring.draft.as_mut().unwrap();
    assert!(draft.harness.is_none());
    draft.harness = Some(HarnessId::new(MEMENTO_TURNS_V1).unwrap());
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
    let client = Client::from_connection(options.connection_file.as_ref().unwrap())
        .await
        .unwrap();
    let pending = client
        .prepare_recipe_save(RecipeMutation::Revise {
            recipe_id: a.saved.as_ref().unwrap().recipe_id.clone(),
            request: ReviseRecipeRequest {
                seeds: None,
                compaction: None,
                harness: None,
                model_registration: None,
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
            harness: None,
            model_name: Some("gpt-oss:20b".into()),
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

#[tokio::test]
async fn seed_files_are_read_listed_saved_and_kept_by_later_edits() {
    use playtime_ui::seed_files::{Choice, Request};
    let root = TestRoot::new().unwrap();
    let (_server, options) = service(&root).await;
    let mut backend = Backend::start(egui::Context::default());
    let mut model = Model::default();
    model.connect(options);
    until(&mut model, &mut backend, |m| !m.recipes.is_empty()).await;
    model.choose_recipe(RecipeId::new("curiosity").unwrap(), RunMode::Experiment);
    until(&mut model, &mut backend, |m| {
        m.authoring.definition.is_some() && !m.previewing
    })
    .await;
    let catalog_seeds = model
        .authoring
        .definition
        .as_ref()
        .unwrap()
        .value
        .seeds
        .clone();
    assert!(!catalog_seeds.is_empty());
    let local = root.join("local");
    std::fs::create_dir_all(local.join("other")).unwrap();
    std::fs::write(local.join("palette.txt"), "palette: blue\n").unwrap();
    std::fs::write(local.join("plan.md"), "# Plan\n").unwrap();
    std::fs::write(local.join("sketch.png"), [0x89, 0x50, 0x4e, 0x47, 0xff]).unwrap();
    std::fs::write(local.join("other/palette.txt"), "palette: red\n").unwrap();
    let read = |model: &mut Model, paths: &[&str]| {
        model.read_seed_files(Request::Read(
            paths.iter().map(|path| local.join(path)).collect(),
        ));
    };
    // Nothing is read without a draft.
    read(&mut model, &["plan.md"]);
    assert!(
        model
            .commands
            .iter()
            .all(|c| !matches!(c.action, Action::ReadSeedFiles(_)))
    );
    model.edit_recipe(true);
    model.authoring.draft.as_mut().unwrap().name = "Seeded".into();
    read(&mut model, &["palette.txt", "plan.md", "sketch.png"]);
    // A second request waits for the first.
    read(&mut model, &["plan.md"]);
    let reads = |model: &Model| {
        model
            .commands
            .iter()
            .filter(|c| matches!(c.action, Action::ReadSeedFiles(_)))
            .count()
    };
    assert_eq!(reads(&model), 1);
    until(&mut model, &mut backend, |m| {
        !m.authoring.draft.as_ref().unwrap().seeds.reading
    })
    .await;
    let seeds = &model.authoring.draft.as_ref().unwrap().seeds;
    assert_eq!(seeds.choice, Choice::Uploaded);
    let names: Vec<_> = seeds.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(names, ["palette.txt", "plan.md"]);
    assert_eq!(seeds.errors.len(), 1);
    assert!(seeds.errors[0].ends_with("sketch.png isn't UTF-8 text"));
    // A file with a listed name waits for Replace or Skip before saving.
    read(&mut model, &["other/palette.txt"]);
    until(&mut model, &mut backend, |m| {
        !m.authoring.draft.as_ref().unwrap().seeds.reading
    })
    .await;
    let draft = model.authoring.draft.as_mut().unwrap();
    assert_eq!(draft.seeds.conflicts.len(), 1);
    assert!(!draft.valid());
    draft.seeds.replace(0);
    assert!(draft.valid());
    assert_eq!(draft.seeds.files[0].content, "palette: red\n");
    model.save_recipe();
    until(&mut model, &mut backend, |m| {
        m.authoring
            .definition
            .as_ref()
            .is_some_and(|d| d.value.saved.is_some())
            && !m.previewing
    })
    .await;
    let a = model.authoring.definition.as_ref().unwrap().value.clone();
    assert_eq!(a.seed_origin, Some(SeedOrigin::Uploaded));
    let saved: Vec<_> = a
        .seeds
        .iter()
        .map(|s| (s.path.as_str(), s.size_bytes))
        .collect();
    assert_eq!(saved, [("palette.txt", 13), ("plan.md", 7)]);
    // The confirmed receipt keeps digests, not contents.
    let receipts = root.join("state").join("recipe-saves");
    for entry in std::fs::read_dir(&receipts).unwrap() {
        let text = std::fs::read_to_string(entry.unwrap().path()).unwrap();
        assert!(!text.contains("palette: red"));
    }
    // A later edit that leaves the choice alone keeps the uploaded files.
    model.edit_recipe(false);
    let draft = model.authoring.draft.as_mut().unwrap();
    assert_eq!(draft.seeds.choice, Choice::Source);
    draft.prompt = "A revised prompt".into();
    model.save_recipe();
    until(&mut model, &mut backend, |m| {
        m.authoring
            .definition
            .as_ref()
            .is_some_and(|d| d.value.prompt == "A revised prompt")
            && !m.previewing
    })
    .await;
    let b = model.authoring.definition.as_ref().unwrap().value.clone();
    assert_eq!((&b.seeds, b.seed_origin), (&a.seeds, a.seed_origin));
    // Starting empty clears them.
    model.edit_recipe(false);
    model.authoring.draft.as_mut().unwrap().seeds.choice = Choice::Empty;
    model.save_recipe();
    until(&mut model, &mut backend, |m| {
        m.authoring
            .definition
            .as_ref()
            .is_some_and(|d| d.value.seed_origin == Some(SeedOrigin::Empty))
            && !m.previewing
    })
    .await;
    assert!(
        model
            .authoring
            .definition
            .as_ref()
            .unwrap()
            .value
            .seeds
            .is_empty()
    );
    // Files read for a discarded draft don't reach the next one.
    model.edit_recipe(false);
    let discarded = model.tag;
    model.discard_recipe_edits();
    until(&mut model, &mut backend, |m| {
        m.authoring.definition.is_some() && !m.previewing
    })
    .await;
    model.edit_recipe(false);
    let file = playtime_client::seeds::read_seed_file(&local.join("plan.md")).unwrap();
    for (tag, listed) in [(discarded, 0), (model.tag, 1)] {
        model.apply(Reply {
            tag,
            run: None,
            data: Data::SeedFiles(vec![Ok(file.clone())]),
        });
        assert_eq!(
            model.authoring.draft.as_ref().unwrap().seeds.files.len(),
            listed
        );
    }
}
