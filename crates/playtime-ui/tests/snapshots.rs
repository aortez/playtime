#![cfg(feature = "ui-snapshots")]
mod common;
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
        let mut app = if name.starts_with("model-settings") {
            common::model_settings_editor().await
        } else if name.starts_with("recipe-editor") {
            common::recipe_editor().await
        } else {
            common::demo(launcher).await
        };
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
        results.add(h.try_snapshot(name));
    }
}
