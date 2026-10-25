mod common;
use egui_kittest::{Harness, kittest::Queryable};
use playtime_protocol::api::*;
use playtime_ui::ui::Workbench;

#[tokio::test]
async fn selector_is_explicit_reset_inherits_and_legacy_editor_has_no_selector() {
    let app = common::harness_editor().await;
    let mut h = Harness::builder()
        .with_size([1440.0, 900.0])
        .build_ui_state(|ui, app: &mut Workbench| app.draw(ui), app);
    h.run();
    h.get_by_label("Harness").click();
    h.run();
    h.get_by_label("Memento (turn-based) · v1").click();
    h.run();
    let request = h
        .state()
        .model
        .authoring
        .draft
        .as_ref()
        .unwrap()
        .request()
        .unwrap();
    assert!(
        matches!(request, RecipeMutation::Clone { request } if request.harness.as_ref().unwrap().as_str() == MEMENTO_TURNS_V1)
    );
    h.get_by_label("Inherit source harness").click();
    h.run();
    assert!(
        h.state()
            .model
            .authoring
            .draft
            .as_ref()
            .unwrap()
            .harness
            .is_none()
    );
    h.get_by_label("Inherited harness settings (read only)")
        .click();
    h.run();
    assert!(
        h.query_by_label("10 sessions · 10 work turns per session")
            .is_some()
    );

    let old = common::recipe_editor().await;
    let mut old = Harness::builder()
        .with_size([1440.0, 900.0])
        .build_ui_state(|ui, app: &mut Workbench| app.draw(ui), old);
    old.run();
    assert!(old.query_by_label("Harness").is_none());
    assert!(
        old.state()
            .model
            .authoring
            .draft
            .as_ref()
            .unwrap()
            .harness
            .is_none()
    );
}
