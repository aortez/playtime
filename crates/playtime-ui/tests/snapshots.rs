#![cfg(feature = "ui-snapshots")]
mod common;
#[path = "support/continuous_scene.rs"]
#[allow(dead_code)]
mod continuous_scene;
#[path = "support/model_scenes.rs"]
mod model_scenes;
#[path = "support/preview_scene.rs"]
mod preview_scene;
#[path = "support/registration_scenes.rs"]
mod registration_scenes;
#[path = "support/session_scenes.rs"]
mod session_scenes;
#[path = "support/trace_scenes.rs"]
mod trace_scenes;
use egui_kittest::Harness;
use playtime_core::RunPhase;
use playtime_protocol::{api::*, events::EventEnvelope, v1::UsageSummary};
use playtime_ui::{
    backend::prepare_run,
    model::{ConnectionState, Inspector},
    transcript::{Transcript, View},
    ui::Workbench,
};
use std::sync::Arc;

fn setup() -> eframe::egui_wgpu::WgpuSetup {
    use eframe::egui_wgpu::{WgpuSetup, WgpuSetupCreateNew, wgpu};
    let mut setup = WgpuSetupCreateNew::without_display_handle();
    setup.instance_descriptor.backends = wgpu::Backends::VULKAN;
    setup.native_adapter_selector = Some(Arc::new(|adapters, _| {
        adapters.iter().find(|adapter| {
            let info = adapter.get_info();
            info.device_type == wgpu::DeviceType::Cpu && info.backend == wgpu::Backend::Vulkan
                && info.name.contains("llvmpipe") && info.driver_info == "Mesa 26.0.8-1ubuntu0.3 (LLVM 21.1.8)"
        }).cloned().ok_or_else(|| "Pinned Mesa lavapipe 26.0.8-1ubuntu0.3 / LLVM 21.1.8 CPU Vulkan adapter required; no renderer fallback".into())
    }));
    WgpuSetup::CreateNew(setup)
}
/// The controller-observation fixture's run, live while its second session
/// waits for the model, or ended, so the header shows progress.
fn progress_screen(app: &mut Workbench, ended: bool) {
    let last = if ended { 59 } else { 34 };
    let mut transcript = Transcript::default();
    for line in include_str!(
        "../../playtime-protocol/tests/fixtures/controller-observations/completed.jsonl"
    )
    .lines()
    {
        let event: EventEnvelope = serde_json::from_str(line).unwrap();
        if event.sequence.get() <= last {
            transcript.apply(event, None).unwrap();
        }
    }
    let mut view = app.model.run.as_ref().unwrap().value.clone();
    let known = if ended { 26 } else { 13 };
    view.usage = Some(UsageSummary {
        known_generated_tokens: known,
        unknown_reserved_tokens: 0,
        unknown_requests: 0,
        budget_charged_tokens: known,
        usage_complete: true,
    });
    view.accounting_coverage = Some(AccountingCoverage {
        schema_version: 1,
        availability: EvidenceAvailability::Available,
        unsettled_requests: if ended { 0 } else { 1 },
        unsettled_allowance: if ended { 0 } else { 100 },
        frontier: view.last_confirmed_frontier.clone().unwrap(),
    });
    if !ended {
        view.orchestration.state = OrchestrationState::Running;
        view.outcome = None;
        view.cleanup = Cleanup::Unknown;
        view.controller_phase = Some(RunPhase::Working);
    }
    app.model.runs = vec![view.clone()];
    app.model.run = Some(prepare_run(view));
    app.model.window = transcript.window(View::Readable, 0, 64);
    app.model.window.progress = transcript.progress(true);
    app.model.window.sessions = transcript.sessions(0, true);
}
#[tokio::test]
async fn reviewed_workbench_screens() {
    let mut results = egui_kittest::SnapshotResults::new();
    for (name, launcher, narrow, inspector, disconnected) in [
        ("launcher", true, false, Inspector::Details, false),
        ("compaction-editor", true, false, Inspector::Details, false),
        (
            "compaction-editor-narrow",
            true,
            true,
            Inspector::Details,
            false,
        ),
        ("compaction-trace", false, false, Inspector::Details, false),
        ("harness-editor", true, false, Inspector::Details, false),
        (
            "harness-editor-narrow",
            true,
            true,
            Inspector::Details,
            false,
        ),
        ("harness-preview", true, false, Inspector::Details, false),
        ("harness-archive", false, false, Inspector::Details, false),
        (
            "recipe-model-invalid",
            true,
            false,
            Inspector::Details,
            false,
        ),
        (
            "recipe-model-invalid-narrow",
            true,
            true,
            Inspector::Details,
            false,
        ),
        (
            "recipe-model-menu-narrow",
            true,
            true,
            Inspector::Details,
            false,
        ),
        (
            "recipe-model-corrected",
            true,
            false,
            Inspector::Details,
            false,
        ),
        ("model-register", true, false, Inspector::Details, false),
        (
            "model-register-narrow",
            true,
            true,
            Inspector::Details,
            false,
        ),
        ("model-registered", true, false, Inspector::Details, false),
        (
            "model-registered-source-narrow",
            true,
            true,
            Inspector::Details,
            false,
        ),
        (
            "model-registered-narrow",
            true,
            true,
            Inspector::Details,
            false,
        ),
        (
            "model-registration-pending",
            true,
            true,
            Inspector::Details,
            false,
        ),
        ("model-browser", true, false, Inspector::Details, false),
        (
            "model-browser-narrow",
            true,
            true,
            Inspector::Details,
            false,
        ),
        (
            "model-browser-missing",
            true,
            true,
            Inspector::Details,
            false,
        ),
        (
            "model-browser-removed",
            true,
            true,
            Inspector::Details,
            false,
        ),
        (
            "model-browser-offline",
            true,
            false,
            Inspector::Details,
            false,
        ),
        ("long-recipe-runs", true, false, Inspector::Details, false),
        (
            "long-recipe-runs-narrow",
            true,
            true,
            Inspector::Details,
            false,
        ),
        ("live-preview", true, false, Inspector::Details, false),
        ("live-preview-narrow", true, true, Inspector::Details, false),
        (
            "live-preview-details",
            true,
            false,
            Inspector::Details,
            false,
        ),
        (
            "live-preview-details-narrow",
            true,
            true,
            Inspector::Details,
            false,
        ),
        ("files-narrow", false, true, Inspector::Files, false),
        ("files-details-narrow", false, true, Inspector::Files, false),
        ("archive-waiting", false, false, Inspector::Details, false),
        ("model-settings", true, false, Inspector::Details, false),
        (
            "model-settings-narrow",
            true,
            true,
            Inspector::Details,
            false,
        ),
        ("recipe-editor", true, false, Inspector::Details, false),
        (
            "recipe-editor-narrow",
            true,
            true,
            Inspector::Details,
            false,
        ),
        ("session-memory", false, false, Inspector::Details, false),
        (
            "session-memory-narrow",
            false,
            true,
            Inspector::Details,
            false,
        ),
        (
            "session-interrupted",
            false,
            false,
            Inspector::Details,
            false,
        ),
        (
            "session-interrupted-narrow",
            false,
            true,
            Inspector::Details,
            false,
        ),
        ("readable", false, false, Inspector::Details, false),
        ("trace-handoff", false, false, Inspector::Details, false),
        (
            "trace-handoff-narrow",
            false,
            true,
            Inspector::Details,
            false,
        ),
        ("trace-streaming", false, false, Inspector::Details, false),
        (
            "trace-streaming-narrow",
            false,
            true,
            Inspector::Details,
            false,
        ),
        ("trace-long", false, false, Inspector::Details, false),
        ("trace-long-narrow", false, true, Inspector::Details, false),
        ("image", false, false, Inspector::Files, false),
        ("memory", false, false, Inspector::Memory, false),
        ("disconnected", false, false, Inspector::Details, true),
        ("narrow", false, true, Inspector::Details, false),
        ("running", false, false, Inspector::Details, false),
        ("ended", false, false, Inspector::Details, false),
    ] {
        let mut app = if name.starts_with("harness-editor") || name.starts_with("compaction-editor")
        {
            common::harness_editor().await
        } else if name.starts_with("long-recipe-runs") {
            common::long_recipe_runs().await
        } else if name.starts_with("model-settings") || name.starts_with("recipe-model") {
            common::model_settings_editor().await
        } else if name.starts_with("recipe-editor") {
            common::recipe_editor().await
        } else {
            common::demo(launcher).await
        };
        if name.starts_with("compaction-editor") {
            app.model.authoring.draft = None;
            app.model.edit_recipe(true);
            let draft = app.model.authoring.draft.as_mut().unwrap();
            draft.harness = Some(HarnessId::new(CONTINUOUS_COMPACTION_V1).unwrap());
            draft.name = "Curiosity: continuous conversation".into();
            draft.prompt = "Explore a question raised by the seed files. Keep verified results and uncertainty clear; finish when the work is complete.".into();
        }
        if name == "compaction-trace" {
            // The timeline starts at the compaction; the header sums the
            // whole scene.
            let mut transcript = Transcript::default();
            let mut whole = Transcript::default();
            for event in continuous_scene::events() {
                if event.sequence.get() >= 7 {
                    transcript.apply(event.clone(), None).unwrap();
                }
                whole.apply(event, None).unwrap();
            }
            app.model.window = transcript.window(View::Readable, 0, 64);
            app.model.window.progress = whole.progress(true);
            app.model.recipes[0].name = "Continuous compaction example".into();
            app.model.history =
                "Synthetic continuous execution evidence · no model or worker".into();
            app.model.checkpoints.clear();
            app.model.checkpoint = None;
            let mut run = app.model.run.as_ref().unwrap().value.clone();
            run.run_id = playtime_core::RunId::new("continuous-ui").unwrap();
            run.harness = Some(ArchivedHarness {
                configuration: run.input_manifest.clone().unwrap(),
                harness: HarnessView::continuous(Default::default()).unwrap(),
            });
            app.model.selected = Some(run.run_id.clone());
            app.model.runs = vec![run.clone()];
            app.model.run = Some(prepare_run(run));
        }
        if name.starts_with("model-register") || name == "model-registration-pending" {
            registration_scenes::prepare(&mut app, name);
        }
        if name.starts_with("model-browser") {
            model_scenes::prepare(&mut app, name.contains("missing"), name.contains("offline"));
            if name == "model-browser-removed" {
                app.model
                    .inspect_model(app.model.models.selection.clone().unwrap());
                app.model.apply(playtime_ui::model::Reply {
                    tag: app.model.tag,
                    run: None,
                    data: playtime_ui::model::Data::ModelInspection(
                        playtime_ui::models::inspection(model_scenes::missing_inspection(
                            "gpt-oss:20b",
                        )),
                    ),
                });
            }
        }
        if name.starts_with("recipe-model") {
            let model = playtime_test_support::recipe_models::boolean_model().summary;
            app.model.authoring.model_selection_enabled = true;
            app.model.registrations.items = vec![model.clone()];
            app.model
                .authoring
                .draft
                .as_mut()
                .unwrap()
                .select_model(model);
            app.model.backend =
                "Synthetic authoring fixture · no native model capability evidence".into();
        }
        if matches!(name, "running" | "ended") {
            progress_screen(&mut app, name == "ended");
        }
        if name.starts_with("trace-") {
            app.model.window = trace_scenes::scene(name);
            app.model.history = "Synthetic reading fixture · no commands executed".into();
            let mut run = app.model.run.as_ref().unwrap().value.clone();
            run.orchestration.state = OrchestrationState::Running;
            run.outcome = None;
            run.cleanup = Cleanup::Unknown;
            run.controller_phase = Some(RunPhase::Working);
            app.model.runs = vec![run.clone()];
            app.model.run = Some(prepare_run(run));
        }
        if name.starts_with("session-") {
            session_scenes::prepare(&mut app, name.contains("interrupted"));
        }
        if name.starts_with("live-preview") {
            let preview = preview_scene::live(app.model.preview.take().unwrap().value);
            app.model.preview = Some(playtime_ui::backend::prepare_preview(preview));
            app.model.backend =
                "Synthetic preview · live-profile metadata only; no inference or tool execution"
                    .into();
        }
        if name == "harness-preview" {
            let mut preview = preview_scene::live(app.model.preview.take().unwrap().value);
            preview.harness = Some(HarnessView::project(MEMENTO_TURNS_V1, &preview.parameters));
            app.model.preview = Some(playtime_ui::backend::prepare_preview(preview));
            app.model.backend = "Synthetic harness preview · no inference or tool execution".into();
        }
        if name == "harness-archive" {
            let parameters = app.model.preview.as_ref().unwrap().value.parameters.clone();
            let mut value = app.model.run.as_ref().unwrap().value.clone();
            value.harness = Some(ArchivedHarness {
                configuration: playtime_protocol::archive::ContentRef {
                    path: playtime_core::ArchivePath::new("inputs/scenario.json").unwrap(),
                    sha256: playtime_core::Sha256Digest::new("a".repeat(64)).unwrap(),
                    size_bytes: 1024,
                },
                harness: HarnessView::project(MEMENTO_TURNS_V1, &parameters),
            });
            app.model.run = Some(prepare_run(value));
            app.model.source = None;
        }
        if name == "archive-waiting" {
            let mut view = app.model.run.take().unwrap().value;
            view.orchestration.state = OrchestrationState::Preparing;
            view.outcome = None;
            view.cleanup = Cleanup::Unknown;
            view.controller_phase = None;
            view.usage = None;
            view.checkpoint = None;
            view.input_manifest = None;
            view.accounting_coverage = None;
            view.event_anchors = None;
            view.last_confirmed_frontier = None;
            view.evidence = EvidenceAvailability::Unavailable {
                reason: "Archive identity is not prepared".into(),
            };
            app.model.runs = vec![view.clone()];
            app.model.run = Some(prepare_run(view));
            app.model.archive_waiting = true;
            app.model.window = Default::default();
            app.model.history = "Attaching live stream and loading history".into();
        }
        if name == "long-recipe-runs-narrow" {
            app.model.preferences.left_width = 180.0;
        }
        app.model.inspector = inspector;
        if name != "image" {
            app.model.content = None;
        }
        if disconnected {
            app.model.connection = ConnectionState::Error(
                "Connection lost; the experiment may still be running".into(),
            );
            app.model.stale = true;
        }
        let mut h = Harness::builder()
            .with_size(if narrow {
                [900.0, 600.0]
            } else {
                [1440.0, 900.0]
            })
            .with_pixels_per_point(1.0)
            .with_step_dt(1.0 / 60.0)
            .wgpu_setup(setup())
            .build_ui_state(|ui, app: &mut Workbench| app.draw(ui), app);
        h.run();
        if name.starts_with("harness-editor") {
            use egui_kittest::kittest::Queryable;
            h.get_by_label("Inherited harness settings (read only)")
                .click();
            h.run();
        }
        if name.starts_with("recipe-model") {
            use egui_kittest::kittest::Queryable;
            if name == "recipe-model-corrected" {
                h.get_by_label("Reasoning").scroll_to_me();
                h.run();
                h.get_by_label("Reasoning").click();
                h.run();
                h.get_by_label("On").click();
                h.run();
                let label = "Context capacity (tokens)";
                h.get_by_label(label).focus();
                h.run();
                h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::A);
                h.run();
                h.get_by_label(label).type_text("49152");
                h.run();
                assert!(h.state().model.authoring.draft.as_ref().unwrap().valid());
                h.ctx.memory_mut(|memory| {
                    memory.surrender_focus(egui::Id::new(("recipe-model-setting", label)));
                });
            }
            h.input_mut()
                .events
                .push(egui::Event::PointerMoved(egui::pos2(600.0, 500.0)));
            h.input_mut().events.push(egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Point,
                phase: egui::TouchPhase::Move,
                delta: egui::vec2(0.0, -440.0),
                modifiers: Default::default(),
            });
            h.run_steps(60);
            if name == "recipe-model-menu-narrow" {
                h.get_by_label("Registered model").scroll_to_me();
                h.run();
                h.get_by_label("Registered model").click();
                h.run();
            }
        }
        if name.starts_with("files-") {
            use egui_kittest::kittest::Queryable;
            h.get_by_label("Inspector").click();
            h.run();
            if name == "files-details-narrow" {
                h.get_all_by_label("Details").last().unwrap().click();
                h.run();
            }
        }
        if name.starts_with("model-settings") {
            h.input_mut()
                .events
                .push(egui::Event::PointerMoved(egui::pos2(600.0, 500.0)));
            h.input_mut().events.push(egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Point,
                phase: egui::TouchPhase::Move,
                delta: egui::vec2(0.0, -420.0),
                modifiers: egui::Modifiers::default(),
            });
            h.run_steps(60);
        }
        if name.starts_with("live-preview-details") {
            h.input_mut()
                .events
                .push(egui::Event::PointerMoved(egui::pos2(600.0, 500.0)));
            h.input_mut().events.push(egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Point,
                phase: egui::TouchPhase::Move,
                delta: egui::vec2(0.0, -560.0),
                modifiers: Default::default(),
            });
            h.run_steps(60);
            use egui_kittest::kittest::Queryable;
            h.get_by_label("Protocol").click();
            h.run();
            h.get_by_label("Workshop inventory").click();
            h.run();
            h.input_mut()
                .events
                .push(egui::Event::PointerMoved(egui::pos2(600.0, 500.0)));
            h.input_mut().events.push(egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Point,
                phase: egui::TouchPhase::Move,
                delta: egui::vec2(0.0, -260.0),
                modifiers: Default::default(),
            });
            h.run_steps(60);
        }
        if name.starts_with("model-register") && !name.starts_with("model-registered") {
            h.input_mut()
                .events
                .push(egui::Event::PointerMoved(egui::pos2(500.0, 430.0)));
            h.input_mut().events.push(egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Point,
                phase: egui::TouchPhase::Move,
                delta: egui::vec2(0.0, -700.0),
                modifiers: Default::default(),
            });
            h.run_steps(60);
        }
        if name == "model-registration-pending" {
            use egui_kittest::kittest::Queryable;
            h.get_by_label("Unresolved model registrations").click();
            h.run();
        }
        if name == "model-registered-source-narrow" {
            use egui_kittest::kittest::Queryable;
            h.get_by_label("Frozen identity and canonical definition")
                .click();
            h.run();
        }
        results.add(h.try_snapshot(name));
    }
}
