use egui_kittest::{Harness, kittest::Queryable};
use playtime_core::{RunId, RunMode};
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
        saved: None,
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

/// Applies a reply to the model's current requests, then redraws.
fn reply(h: &mut Harness<'_, Workbench>, run: Option<RunId>, data: Data) {
    let tag = h.state().model.tag;
    h.state_mut().model.apply(Reply { tag, run, data });
    h.run();
}

/// The status bar keeps a run's notices only while that run is open (#104).
#[tokio::test]
async fn a_run_notice_clears_when_another_run_or_the_launcher_opens() {
    let mut app = common::demo(false).await;
    let first = app.model.selected.clone().unwrap();
    let other = RunId::new("other-run").unwrap();
    let mut view = app.model.runs[0].clone();
    view.run_id = other.clone();
    app.model.runs.push(view);
    let mut h = Harness::builder()
        .with_size([1440.0, 900.0])
        .build_ui_state(|ui, app: &mut Workbench| app.draw(ui), app);
    let reconciled =
        "Stream reconciled: committed completion; lifecycle and cleanup are shown separately";
    reply(&mut h, Some(first.clone()), Data::Notice(reconciled.into()));
    h.get_by_label(reconciled);
    h.get_by_label("Run other-run · Ended").click();
    h.run();
    assert!(h.query_by_label(reconciled).is_none());
    // A failed load, such as a checkpoint list that timed out.
    let failed = "HTTP deadline exceeded";
    reply(&mut h, Some(other.clone()), Data::Error(failed.into()));
    h.get_by_label(failed);
    h.get_by_label(&format!("Run {first} · Ended")).click();
    h.run();
    assert!(h.query_by_label(failed).is_none());
    // A notice that isn't about a run stays.
    let saved = "Verified download saved to /tmp/art.png";
    reply(&mut h, None, Data::Notice(saved.into()));
    h.get_by_label("Run other-run · Ended").click();
    h.run();
    h.get_by_label(saved);
    let unresolved = "Cancellation unresolved: reply lost. Inspect status before retrying.";
    reply(
        &mut h,
        Some(other),
        Data::CancellationUnresolved(unresolved.into()),
    );
    h.get_by_label(unresolved);
    h.get_by_label("New experiment").click();
    h.run();
    assert!(h.query_by_label(unresolved).is_none());
}

/// An accepted launch asks for the run list at once rather than waiting for
/// the list's next poll, even when another run was opened meanwhile (#104).
#[tokio::test]
async fn an_accepted_launch_refreshes_the_run_list() {
    for navigate in [false, true] {
        let mut h = Harness::builder()
            .with_size([1440.0, 900.0])
            .build_ui_state(
                |ui, app: &mut Workbench| app.draw(ui),
                common::demo(true).await,
            );
        h.get_by_label("Launch experiment").click();
        h.run();
        // The service's reply carries the launch command's tag.
        let Some(Command {
            tag,
            action: Action::Launch(request),
        }) = h.state().model.commands.back().cloned()
        else {
            panic!("Launch experiment queued no launch");
        };
        let old = h.state().model.runs[0].clone();
        if navigate {
            h.get_by_label(&format!("Run {} · Ended", old.run_id))
                .click();
            h.run();
        }
        let mut run = old.clone();
        run.run_id = RunId::new("launched-run").unwrap();
        run.orchestration.state = OrchestrationState::Preparing;
        run.outcome = None;
        let saved = playtime_client::state::LaunchReceipt {
            schema_version: 1,
            service_id: h.state().model.service.clone().unwrap(),
            endpoint: "http://127.0.0.1:1234".into(),
            key: IdempotencyKey::new("accepted-launch").unwrap(),
            request,
            created_at_unix_ms: 0,
            run_id: Some(run.run_id.clone()),
        };
        h.state_mut().model.commands.clear();
        h.state_mut().model.apply(Reply {
            tag,
            run: None,
            data: Data::Accepted(
                Box::new(LaunchResponse {
                    run_id: run.run_id.clone(),
                    accepted_at: run.accepted_at.clone(),
                    replayed: false,
                    run: run.clone(),
                }),
                saved,
            ),
        });
        h.run();
        assert!(
            h.state()
                .model
                .commands
                .iter()
                .any(|command| matches!(command.action, Action::Runs(None)))
        );
        assert_eq!(
            h.state().model.selected.as_ref() == Some(&run.run_id),
            !navigate
        );
        reply(
            &mut h,
            None,
            Data::Runs(Page {
                items: vec![run, old],
                next_cursor: None,
                frontier: None,
            }),
        );
        h.get_by_label("Run launched-run · Preparing");
    }
}

/// Presses Reconnect and answers it, then says whether the model watches `run`.
fn reconnect(h: &mut Harness<'_, Workbench>, run: &RunId) -> bool {
    let capabilities = Fixture::new(ServiceId::new("snapshot-fixture").unwrap()).capabilities();
    h.get_by_label("Reconnect").click();
    h.run();
    h.state_mut().model.commands.clear();
    reply(
        h,
        None,
        Data::Connected(capabilities, "http://127.0.0.1:PORT".into()),
    );
    h.state()
        .model
        .commands
        .iter()
        .any(|command| matches!(&command.action, Action::Observe(watched) if watched == run))
}

/// Reconnecting from the launcher doesn't watch the run it left again, so
/// that run's notices stay closed with it (#104).
#[tokio::test]
async fn reconnecting_from_the_launcher_leaves_the_last_run_closed() {
    let app = common::demo(false).await;
    let run = app.model.selected.clone().unwrap();
    let mut h = Harness::builder()
        .with_size([1440.0, 900.0])
        .build_ui_state(|ui, app: &mut Workbench| app.draw(ui), app);
    h.get_by_label("New experiment").click();
    h.run();
    assert!(!reconnect(&mut h, &run));
    assert!(h.state().model.launcher);
    // Reconnecting with the run open still watches it again.
    h.get_by_label(&format!("Run {run} · Ended")).click();
    h.run();
    assert!(reconnect(&mut h, &run));
}

#[tokio::test]
async fn recipe_editor_keyboard_and_narrow_save_keep_unsaved_text() {
    let app = common::recipe_editor().await;
    let mut h = Harness::builder()
        .with_size([900.0, 600.0])
        .build_ui_state(|ui, app: &mut Workbench| app.draw(ui), app);
    h.run();
    h.get_by_label("Name").click();
    h.run();
    h.get_by_label("Name").type_text(" edited");
    h.run();
    h.get_by_label("Prompt").click();
    h.run();
    h.get_by_label("Prompt").type_text(" Keep a short journal.");
    h.run();
    assert!(
        h.state()
            .model
            .authoring
            .draft
            .as_ref()
            .unwrap()
            .name
            .contains("edited")
    );
    assert!(
        h.state()
            .model
            .authoring
            .draft
            .as_ref()
            .unwrap()
            .prompt
            .contains("short journal")
    );
    h.get_by_label("Launch experiment").click();
    h.run();
    assert!(
        !h.state()
            .model
            .commands
            .iter()
            .any(|c| matches!(c.action, Action::Launch(_)))
    );
    h.get_by_label("Save revision").click();
    h.run();
    assert_eq!(
        h.state()
            .model
            .commands
            .iter()
            .filter(|c| matches!(c.action, Action::SaveRecipe(_)))
            .count(),
        1
    );
    h.get_by_label("Save revision").click();
    h.run();
    assert_eq!(
        h.state()
            .model
            .commands
            .iter()
            .filter(|c| matches!(c.action, Action::SaveRecipe(_)))
            .count(),
        1
    );
}

#[tokio::test]
async fn model_settings_keyboard_zero_invalid_value_and_resets() {
    let app = common::model_settings_editor().await;
    let mut h = Harness::builder()
        .with_size([900.0, 600.0])
        .build_ui_state(|ui, app: &mut Workbench| app.draw(ui), app);
    h.run();
    h.get_by_label("Temperature").focus();
    h.run();
    h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::A);
    h.run();
    h.get_by_label("Temperature").type_text("0");
    h.run();
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
            .unwrap()
            .temperature,
        0.0
    );
    h.get_by_label("Reset Temperature").click_accesskit();
    h.run();
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
            .unwrap()
            .temperature,
        0.8
    );
    h.get_by_label("Context capacity (tokens)").focus();
    h.run();
    h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::A);
    h.run();
    h.get_by_label("Context capacity (tokens)")
        .type_text("not a number");
    h.run();
    assert!(!h.state().model.authoring.draft.as_ref().unwrap().valid());
    h.state_mut().model.save_recipe();
    assert!(
        !h.state()
            .model
            .commands
            .iter()
            .any(|c| matches!(c.action, Action::SaveRecipe(_)))
    );
    h.get_by_label("Reset model settings").click_accesskit();
    h.run();
    assert!(h.state().model.authoring.draft.as_ref().unwrap().valid());
    h.get_by_label("Save revision").click_accesskit();
    h.run();
    assert!(h.state().model.commands.iter().any(|c| matches!(&c.action, Action::SaveRecipe(RecipeMutation::Clone { request }) if request.model_settings.as_ref().is_some_and(|s| s.context_tokens == 65536 && s.temperature == 0.8))));
}

#[tokio::test]
async fn recovered_save_preserves_an_unfinished_numeric_draft() {
    let mut app = common::model_settings_editor().await;
    let request = app
        .model
        .authoring
        .draft
        .as_ref()
        .unwrap()
        .request()
        .unwrap();
    let root = playtime_test_support::TestRoot::new().unwrap();
    let ledger = playtime_server::lifecycle::Ledger::open(&root.0)
        .await
        .unwrap();
    let catalog = playtime_server::catalog::Catalog::new(
        ledger,
        playtime_server::daemon::catalog::load(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../recipes"),
        )
        .unwrap(),
    )
    .unwrap();
    let saved = catalog
        .save(
            IdempotencyKey::new("another-workbench").unwrap(),
            request.clone(),
        )
        .await
        .unwrap();
    app.model.service = Some(saved.service_id.clone());
    let mut h = Harness::builder()
        .with_size([900.0, 600.0])
        .build_ui_state(|ui, app: &mut Workbench| app.draw(ui), app);
    h.run();
    let selected = h.state().model.recipe.clone();
    for text in ["-", "", "not a number", "3"] {
        h.get_by_label("Temperature").focus();
        h.run();
        h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::A);
        h.run();
        h.key_press(egui::Key::Backspace);
        h.run();
        h.get_by_label("Temperature").type_text(text);
        h.run();
        assert!(!h.state().model.authoring.draft.as_ref().unwrap().valid());
        let tag = h.state().model.tag;
        let receipt = playtime_client::recipes::RecipeSaveReceipt {
            pending: playtime_client::recipes::PendingRecipeSave {
                schema_version: 1,
                service_id: saved.service_id.clone(),
                key: saved.key.clone(),
                request: request.clone(),
            },
            endpoint: h.state().model.endpoint.clone(),
            saved_execution: None,
        };
        // Reconnect discovers another client's receipt after this draft was opened.
        h.state_mut().model.apply(Reply {
            tag,
            run: None,
            data: Data::RecipeSaveReceipts(vec![receipt]),
        });
        h.state_mut().model.commands.clear();
        h.state_mut().model.apply(Reply {
            tag,
            run: None,
            data: Data::RecipeSaved(Box::new(saved.clone())),
        });
        h.run();
        assert_eq!(h.get_by_label("Temperature").value().as_deref(), Some(text));
        assert_eq!(h.state().model.recipe, selected);
        assert!(h.state().model.authoring.draft.is_some());
        assert!(h.state().model.authoring.receipts.is_empty());
        assert_eq!(
            h.state().model.authoring.last_saved.as_ref(),
            Some(&saved.definition.execution_recipe_id)
        );
        assert!(!h.state().model.commands.iter().any(|c| matches!(
            c.action,
            Action::Preview(..) | Action::Definition(_) | Action::SaveRecipe(_)
        )));
    }
}
