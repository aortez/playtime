use egui_kittest::{Harness, kittest::Queryable};
use playtime_core::RunMode;
use playtime_protocol::api::*;
use playtime_server::{application::Application, fixture::Fixture};
use playtime_ui::{backend::prepare_preview, model::*, ui::Workbench};

#[tokio::test]
async fn named_recipe_preview_launch_and_double_click_controls() {
    let fixture = Fixture::new(ServiceId::new("ui-fixture").unwrap());
    let preview = fixture
        .preview(
            RecipeId::new("fixture-example").unwrap(),
            PreviewRequest {
                mode: RunMode::Simulation,
            },
        )
        .await
        .unwrap();
    let mut app = Workbench::default();
    app.model.connection = ConnectionState::Connected;
    app.model.modes = vec![RunMode::Simulation];
    app.model.recipes = vec![Recipe {
        recipe_id: preview.recipe_id.clone(),
        name: "Two fresh sessions".into(),
        modes: vec![RunMode::Simulation],
    }];
    let mut h = Harness::builder()
        .with_size([1440.0, 900.0])
        .build_ui_state(|ui, app: &mut Workbench| app.draw(ui), app);
    h.get_by_label("Recipe").click();
    h.run();
    h.get_by_label("Two fresh sessions").click();
    h.run();
    assert!(matches!(
        h.state().model.commands.back().unwrap().action,
        Action::Preview(_, _)
    ));
    let tag = h.state().model.tag;
    h.state_mut().model.apply(Reply {
        tag,
        run: None,
        data: Data::Preview(Box::new(prepare_preview(preview))),
    });
    h.state_mut().model.commands.clear();
    h.run();
    h.get_by_label("Launch experiment").click();
    h.run();
    h.get_by_label("Launch experiment").click();
    h.run();
    assert_eq!(
        h.state()
            .model
            .commands
            .iter()
            .filter(|c| matches!(c.action, Action::Launch(_)))
            .count(),
        1
    );
}
#[test]
fn disclosure_follow_keyboard_and_narrow_drawer_use_real_widgets() {
    let mut app = Workbench::default();
    app.model.launcher = false;
    let mut h = Harness::builder()
        .with_size([900.0, 600.0])
        .build_ui_state(|ui, app: &mut Workbench| app.draw(ui), app);
    h.get_by_label("Full").click();
    h.run();
    assert_eq!(h.state().model.preferences.disclosure, Disclosure::Full);
    h.get_by_label("Compact").click();
    h.run();
    assert_eq!(h.state().model.preferences.disclosure, Disclosure::Compact);
    h.get_by_label("Readable").click();
    h.run();
    h.get_by_label("Pause following").click();
    h.run();
    assert!(!h.state().model.following);
    h.get_by_label("Jump to live (0)").click();
    h.run();
    assert!(h.state().model.following);
    h.key_press(egui::Key::Escape);
    h.run();
    assert!(!h.state().model.show_inspector);
    h.key_press(egui::Key::Tab);
    h.run();
    assert!(h.ctx.memory(|m| m.focused().is_some()));
    assert!(
        !h.state()
            .model
            .commands
            .iter()
            .any(|c| matches!(c.action, Action::Cancel(_)))
    );
}

mod common;
#[tokio::test]
async fn recover_cancel_reconnect_and_inspect_through_named_controls() {
    let mut app = common::demo(true).await;
    let preview = &app.model.preview.as_ref().unwrap().value;
    let saved = playtime_client::state::LaunchReceipt {
        schema_version: 1,
        service_id: app.model.service.clone().unwrap(),
        endpoint: "http://127.0.0.1:1234".into(),
        key: IdempotencyKey::new("lost-ui-reply").unwrap(),
        request: LaunchRequest {
            recipe_id: preview.recipe_id.clone(),
            mode: preview.mode,
            preview_revision: preview.preview_revision.clone(),
        },
        created_at_unix_ms: 0,
        run_id: None,
    };
    app.model.saved = Some(saved.clone());
    app.model.launch = Mutation::Unresolved("Reply lost".into());
    let mut h = Harness::builder()
        .with_size([1440.0, 900.0])
        .build_ui_state(|ui, app: &mut Workbench| app.draw(ui), app);
    h.get_by_label("Look up saved launch").click();
    h.run();
    assert!(
        h.state()
            .model
            .commands
            .iter()
            .any(|c| matches!(&c.action, Action::Recover(key, false) if key == &saved.key))
    );
    h.state_mut().model.commands.clear();
    let tag = h.state().model.tag;
    h.state_mut().model.apply(Reply {
        tag,
        run: None,
        data: Data::LaunchProgress(Mutation::Unresolved("Lookup reply lost".into()), None),
    });
    h.run();
    h.get_by_label("Retry saved launch").click();
    h.run();
    assert!(
        h.state()
            .model
            .commands
            .iter()
            .any(|c| matches!(&c.action, Action::Recover(key, true) if key == &saved.key))
    );
    h.state_mut().model.launcher = false;
    h.state_mut()
        .model
        .run
        .as_mut()
        .unwrap()
        .value
        .orchestration
        .state = OrchestrationState::Running;
    h.state_mut().model.content = None;
    h.run();
    h.get_by_label("Cancel run").click();
    h.run();
    assert!(
        h.state()
            .model
            .commands
            .iter()
            .any(|c| matches!(c.action, Action::Cancel(_)))
    );
    let tag = h.state().model.tag;
    let run = h.state().model.selected.clone();
    h.state_mut().model.apply(Reply {
        tag,
        run,
        data: Data::CancellationUnresolved("Missing cancellation reply".into()),
    });
    h.run();
    assert!(matches!(
        h.state().model.cancellation,
        Mutation::Unresolved(_)
    ));
    h.get_by_label("Memory").click();
    h.run();
    h.get_by_label("Outgoing source").click();
    h.run();
    assert!(
        h.state()
            .model
            .commands
            .iter()
            .any(|c| matches!(c.action, Action::Content(..)))
    );
    h.get_by_label("Files").click();
    h.run();
    h.get_by_label("Inspect file saved-image").click();
    h.run();
    assert_eq!(h.state().model.inspector, Inspector::Files);
    h.get_by_label("Reconnect").click();
    h.run();
    assert_eq!(h.state().model.connection, ConnectionState::Connecting);
}
