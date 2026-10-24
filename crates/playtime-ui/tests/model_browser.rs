mod common;
#[path = "support/model_scenes.rs"]
mod model_scenes;
use playtime_protocol::api::*;
use playtime_ui::{model::*, models};
#[tokio::test]
async fn browsing_preserves_draft_and_execution_and_rejects_stale_replies() {
    let mut app = common::recipe_editor().await;
    let m = &mut app.model;
    let request = m.authoring.draft.as_ref().unwrap().request().unwrap();
    let recipe = m.recipe.clone();
    let preview = m.preview.as_ref().map(|p| p.value.clone());
    let run = m.selected.clone();
    m.commands.clear();
    m.models.enabled = true;
    m.browse_models();
    let old = m.tag;
    m.refresh_models();
    m.apply(Reply {
        run: None,
        tag: old,
        data: Data::ModelInventory(models::inventory(model_scenes::inventory())),
    });
    assert!(m.models.inventory.is_none());
    assert!(m.models.pending);
    m.apply(Reply {
        run: None,
        tag: m.tag,
        data: Data::ModelInventory(models::inventory(model_scenes::inventory())),
    });
    assert!(!m.models.stale);
    m.inspect_model(ModelInspectRequest {
        name: "gpt-oss:20b".into(),
        expected_digest: Some("a".repeat(64)),
    });
    let inspection_tag = m.tag;
    m.refresh_models();
    m.apply(Reply {
        run: None,
        tag: inspection_tag,
        data: Data::ModelInspection(models::inspection(model_scenes::inspection("gpt-oss:20b"))),
    });
    assert!(m.models.inspection.is_none());
    m.apply(Reply {
        run: None,
        tag: m.tag,
        data: Data::ModelError("offline".into()),
    });
    assert!(m.models.stale);
    assert!(m.models.inventory.is_some());
    assert!(!m.models.pending);
    assert_eq!(m.models.selection.as_ref().unwrap().name, "gpt-oss:20b");
    let old = m.tag;
    m.close_models();
    m.apply(Reply {
        run: None,
        tag: old,
        data: Data::ModelInventory(models::inventory(model_scenes::inventory())),
    });
    assert!(m.models.stale);
    assert_eq!(
        m.authoring.draft.as_ref().unwrap().request().unwrap(),
        request
    );
    assert_eq!(m.recipe, recipe);
    assert_eq!(m.preview.as_ref().map(|p| p.value.clone()), preview);
    assert_eq!(m.selected, run);
    assert!(m.commands.iter().all(|c| matches!(
        c.action,
        Action::ModelInventory | Action::InspectModel(_) | Action::StopModelDiscovery
    )));
    m.connect(Default::default());
    m.apply(Reply {
        run: None,
        tag: old,
        data: Data::ModelError("late".into()),
    });
    assert!(m.models.inventory.is_none());
    assert!(m.models.selection.is_none());
    assert!(!m.models.open);
}
#[tokio::test]
async fn missing_selection_and_equal_digest_aliases_remain_explicit() {
    let mut app = common::demo(true).await;
    model_scenes::prepare(&mut app, true, false);
    let m = &mut app.model;
    m.refresh_models();
    m.apply(Reply {
        run: None,
        tag: m.tag,
        data: Data::ModelInventory(models::inventory(model_scenes::inventory())),
    });
    assert_eq!(
        m.models.selection.as_ref().unwrap().name,
        "saved-model:no-longer-installed"
    );
    let rows = &m.models.inventory.as_ref().unwrap().rows;
    assert!(rows[0].1.contains("experiment/alias:latest"));
    assert!(!rows[2].1.contains("same digest"));
}

#[tokio::test]
async fn model_buttons_only_enqueue_metadata_reads() {
    use egui_kittest::{Harness, kittest::Queryable};
    let mut app = common::demo(true).await;
    app.model.models.enabled = true;
    app.model.commands.clear();
    let mut h = Harness::builder()
        .with_size([900.0, 600.0])
        .build_ui_state(|ui, app: &mut playtime_ui::ui::Workbench| app.draw(ui), app);
    h.get_by_label("Models").click();
    h.run();
    assert!(h.state().model.models.open);
    assert!(matches!(
        h.state().model.commands.back().unwrap().action,
        Action::ModelInventory
    ));
    let tag = h.state().model.tag;
    h.state_mut().model.apply(Reply {
        tag,
        run: None,
        data: Data::ModelInventory(models::inventory(model_scenes::inventory())),
    });
    h.run();
    h.get_by_label("Inspect model experiment/alias:latest")
        .click();
    h.run();
    assert!(
        matches!(&h.state().model.commands.back().unwrap().action, Action::InspectModel(request) if request.name == "experiment/alias:latest")
    );
    h.get_by_label("Refresh installed models").click();
    h.run();
    assert!(matches!(
        h.state().model.commands.back().unwrap().action,
        Action::ModelInventory
    ));
}
