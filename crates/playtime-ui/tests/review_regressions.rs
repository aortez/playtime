use playtime_core::{RunId, RunMode, SessionId, UtcTime};
use playtime_protocol::api::*;
use playtime_server::{application::*, fixture::Fixture, http::Server};
use playtime_ui::{
    backend::{Backend, prepare_run},
    model::*,
};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::watch;

mod common;

#[tokio::test]
async fn refresh_preview_must_not_enable_second_launch_while_first_is_pending() {
    use egui_kittest::{Harness, kittest::Queryable};
    let app = common::demo(true).await;
    let preview = app.model.preview.as_ref().unwrap().value.clone();
    let mut h = Harness::builder()
        .with_size([1440.0, 900.0])
        .build_ui_state(|ui, app: &mut playtime_ui::ui::Workbench| app.draw(ui), app);
    h.get_by_label("Launch experiment").click();
    h.run();
    assert!(matches!(h.state().model.launch, Mutation::Recording));
    h.get_by_label("Refresh preview").click();
    h.run();
    let tag = h.state().model.tag;
    h.state_mut().model.apply(Reply {
        tag,
        run: None,
        data: Data::Preview(Box::new(playtime_ui::backend::prepare_preview(preview))),
    });
    h.run();
    h.get_by_label("Launch experiment").click();
    h.run();
    let launches = h
        .state()
        .model
        .commands
        .iter()
        .filter(|c| matches!(c.action, Action::Launch(_)))
        .count();
    assert_eq!(
        launches, 1,
        "refreshing a preview unlocked a second launch before the first request resolved"
    );
}

fn cancelled(mut view: RunView) -> RunView {
    view.orchestration.state = OrchestrationState::Cancelling;
    view.outcome = None;
    view.cleanup = Cleanup::Unknown;
    view.updated_at = UtcTime::new("2026-10-17T12:00:01Z").unwrap();
    view.cancellation = Some(Cancellation {
        requested_at: view.updated_at.clone(),
        enforcement: Enforcement::Requested,
        stop_accepted_at: None,
        grace_deadline: None,
        uncertainty: None,
    });
    view
}

#[tokio::test]
async fn launch_resolution_survives_preview_changes_without_stealing_selection() {
    for navigate in [false, true] {
        let mut app = common::demo(true).await;
        let preview = app.model.preview.as_ref().unwrap().value.clone();
        let view = app.model.run.as_ref().unwrap().value.clone();
        app.model.launch();
        let launch_tag = app.model.tag;
        let mut saved = playtime_client::state::LaunchReceipt {
            schema_version: 1,
            service_id: app.model.service.clone().unwrap(),
            endpoint: app.model.endpoint.clone(),
            key: IdempotencyKey::new("pending-refresh").unwrap(),
            request: LaunchRequest {
                recipe_id: preview.recipe_id.clone(),
                mode: preview.mode,
                preview_revision: preview.preview_revision.clone(),
            },
            created_at_unix_ms: 0,
            run_id: None,
        };
        app.model
            .choose_recipe(preview.recipe_id.clone(), preview.mode);
        if navigate {
            app.model
                .select_run(RunId::new("another-selection").unwrap());
        }
        app.model.apply(Reply {
            tag: launch_tag,
            run: None,
            data: Data::LaunchProgress(
                Mutation::Unresolved("Reply lost".into()),
                Some(saved.clone()),
            ),
        });
        assert!(matches!(app.model.launch, Mutation::Unresolved(_)));
        assert_eq!(app.model.saved.as_ref().unwrap().key, saved.key);
        app.model
            .choose_recipe(preview.recipe_id.clone(), preview.mode);
        app.model.apply(Reply {
            tag: app.model.tag,
            run: None,
            data: Data::Preview(Box::new(playtime_ui::backend::prepare_preview(preview))),
        });
        assert!(
            !app.model.can_launch(),
            "refresh must preserve an ambiguous mutation"
        );

        app.model.command(Action::Recover(saved.key.clone(), false));
        let recovery_tag = app.model.tag;
        app.model.apply(Reply {
            tag: launch_tag,
            run: None,
            data: Data::LaunchProgress(Mutation::Rejected("Late original reply".into()), None),
        });
        assert_eq!(app.model.launch, Mutation::Submitting);
        // Navigate after recovery starts, or refresh in place. Both must still
        // consume its resolution; only the latter may select the admitted run.
        if navigate {
            app.model
                .select_run(RunId::new("keep-this-selection").unwrap());
        } else {
            app.model
                .choose_recipe(saved.request.recipe_id.clone(), saved.request.mode);
        }
        saved.run_id = Some(view.run_id.clone());
        app.model.apply(Reply {
            tag: recovery_tag,
            run: None,
            data: Data::Accepted(
                Box::new(LaunchResponse {
                    run_id: view.run_id.clone(),
                    accepted_at: view.accepted_at.clone(),
                    replayed: true,
                    run: view.clone(),
                }),
                saved.clone(),
            ),
        });
        assert_eq!(app.model.launch, Mutation::Accepted);
        assert_eq!(app.model.saved.unwrap().run_id, saved.run_id);
        assert_eq!(
            app.model.selected,
            Some(if navigate {
                RunId::new("keep-this-selection").unwrap()
            } else {
                view.run_id
            })
        );
    }
}

#[tokio::test]
async fn reconnect_and_a_full_recovery_queue_keep_launch_ambiguity() {
    let mut app = common::demo(true).await;
    app.model.launch();
    app.model.connect(Default::default());
    assert!(matches!(app.model.launch, Mutation::Unresolved(_)));
    app.model.commands.clear();
    for _ in 0..64 {
        app.model.command(Action::Runs(None));
    }
    app.model.command(Action::Recover(
        IdempotencyKey::new("original-key").unwrap(),
        true,
    ));
    assert!(matches!(app.model.launch, Mutation::Unresolved(_)));
}

#[tokio::test]
async fn equal_timestamps_preserve_acknowledged_facts_and_prepared_display() {
    let mut app = common::demo(false).await;
    let cancel = cancelled(app.model.run.as_ref().unwrap().value.clone());
    let mut running = cancel.clone();
    running.orchestration.state = OrchestrationState::Running;
    running.cancellation = None;
    app.model.run = Some(prepare_run(running.clone()));
    let prepared = prepare_run(cancel.clone());
    app.model.apply(Reply {
        tag: app.model.tag,
        run: app.model.selected.clone(),
        data: Data::Cancellation(Box::new(prepared.clone())),
    });
    app.model.apply(Reply {
        tag: app.model.tag,
        run: app.model.selected.clone(),
        data: Data::Run(Box::new(prepare_run(running))),
    });
    assert_eq!(app.model.run.as_ref().unwrap().value, cancel);
    assert_eq!(app.model.run.as_ref().unwrap().summary, prepared.summary);
    assert_eq!(
        app.model.run.as_ref().unwrap().source.text,
        prepared.source.text
    );
    let mut terminal = cancel.clone();
    terminal.orchestration.state = OrchestrationState::Terminal;
    app.model.apply(Reply {
        tag: app.model.tag,
        run: app.model.selected.clone(),
        data: Data::Run(Box::new(prepare_run(terminal.clone()))),
    });
    app.model.apply(Reply {
        tag: app.model.tag,
        run: app.model.selected.clone(),
        data: Data::Cancellation(Box::new(prepare_run(cancel))),
    });
    assert_eq!(app.model.run.as_ref().unwrap().value, terminal);
}

#[tokio::test]
async fn snapshot_order_handles_fractional_seconds_and_stale_terminal_cleanup() {
    let mut app = common::demo(false).await;
    let mut older = app.model.run.as_ref().unwrap().value.clone();
    older.updated_at = UtcTime::new("2026-10-17T12:00:02Z").unwrap();
    older.cleanup = Cleanup::Unknown;
    app.model.run = Some(prepare_run(older.clone()));
    let mut newer = older.clone();
    newer.updated_at = UtcTime::new("2026-10-17T12:00:02.1Z").unwrap();
    newer.cleanup = Cleanup::Complete;
    app.model.apply(Reply {
        tag: app.model.tag,
        run: app.model.selected.clone(),
        data: Data::Run(Box::new(prepare_run(newer.clone()))),
    });
    app.model.apply(Reply {
        tag: app.model.tag,
        run: app.model.selected.clone(),
        data: Data::Cancellation(Box::new(prepare_run(older))),
    });
    assert_eq!(app.model.run.as_ref().unwrap().value, newer);
}

#[tokio::test]
async fn late_status_poll_must_not_erase_acknowledged_cancellation() {
    let mut app = common::demo(false).await;
    let mut old = app.model.run.as_ref().unwrap().value.clone();
    old.orchestration.state = OrchestrationState::Running;
    old.outcome = None;
    old.cleanup = Cleanup::Unknown;
    old.cancellation = None;
    old.updated_at = UtcTime::new("2026-10-17T12:00:00Z").unwrap();
    app.model.run = Some(prepare_run(old.clone()));
    let acknowledged = cancelled(old.clone());
    app.model.apply(Reply {
        tag: app.model.tag,
        run: app.model.selected.clone(),
        data: Data::Cancellation(Box::new(prepare_run(acknowledged))),
    });
    app.model.apply(Reply {
        tag: app.model.tag,
        run: app.model.selected.clone(),
        data: Data::Run(Box::new(prepare_run(old))),
    });
    assert!(
        app.model.run.as_ref().unwrap().value.cancellation.is_some(),
        "a status GET started before Cancel erased its acknowledged intent"
    );
}

#[tokio::test]
async fn late_cancel_reply_must_not_replace_newer_terminal_status() {
    let mut app = common::demo(false).await;
    let old = cancelled(app.model.run.as_ref().unwrap().value.clone());
    let mut terminal = app.model.run.as_ref().unwrap().value.clone();
    terminal.updated_at = UtcTime::new("2026-10-17T12:00:02Z").unwrap();
    app.model.apply(Reply {
        tag: app.model.tag,
        run: app.model.selected.clone(),
        data: Data::Run(Box::new(prepare_run(terminal))),
    });
    app.model.apply(Reply {
        tag: app.model.tag,
        run: app.model.selected.clone(),
        data: Data::Cancellation(Box::new(prepare_run(old))),
    });
    assert_eq!(
        app.model.run.as_ref().unwrap().value.orchestration.state,
        OrchestrationState::Terminal,
        "late Cancel response rolled terminal state backwards"
    );
}

struct TransientFailure {
    inner: Fixture,
    run_once: AtomicBool,
    error_code: &'static str,
    anchors_once: AtomicBool,
    history_failure_at: usize,
    history_attempts: AtomicUsize,
    history_cursors: Mutex<Vec<Option<EventCursor>>>,
}
#[async_trait::async_trait]
impl Application for TransientFailure {
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }
    async fn recipes(&self, q: PageQuery) -> Result<Page<Recipe>> {
        self.inner.recipes(q).await
    }
    async fn preview(&self, id: RecipeId, q: PreviewRequest) -> Result<Preview> {
        self.inner.preview(id, q).await
    }
    async fn admit(&self, key: IdempotencyKey, request: LaunchRequest) -> Result<LaunchResponse> {
        self.inner.admit(key, request).await
    }
    async fn launch(&self, key: IdempotencyKey) -> Result<LaunchResponse> {
        self.inner.launch(key).await
    }
    async fn runs(&self, q: PageQuery) -> Result<Page<RunView>> {
        self.inner.runs(q).await
    }
    async fn run(&self, id: RunId) -> Result<RunView> {
        if self.run_once.swap(false, Ordering::SeqCst) {
            return Err(ApiError::new(self.error_code));
        }
        let mut view = self.inner.run(id).await?;
        if self.anchors_once.swap(false, Ordering::SeqCst) {
            view.event_anchors = None;
            view.last_confirmed_frontier = None;
            view.accounting_coverage = None;
            view.evidence = EvidenceAvailability::Unavailable {
                reason: "Archive identity is not prepared".into(),
            };
        }
        Ok(view)
    }
    async fn cancel(&self, id: RunId) -> Result<CancelResponse> {
        self.inner.cancel(id).await
    }
    async fn history(
        &self,
        id: RunId,
        cursor: Option<EventCursor>,
        follow: bool,
        limit: u32,
        bytes: usize,
    ) -> Result<History> {
        if !follow {
            self.history_cursors.lock().unwrap().push(cursor.clone());
            let attempt = self.history_attempts.fetch_add(1, Ordering::SeqCst) + 1;
            if attempt == self.history_failure_at {
                return Err(ApiError::new(self.error_code));
            }
        }
        // Force multiple fixed pages when testing a failure after progress.
        let limit = if !follow && self.history_failure_at == 2 {
            1
        } else {
            limit
        };
        self.inner.history(id, cursor, follow, limit, bytes).await
    }
    async fn subscribe(&self, id: RunId) -> Result<watch::Receiver<LiveState>> {
        self.inner.subscribe(id).await
    }
    async fn checkpoints(&self, id: RunId, q: PageQuery) -> Result<Page<Checkpoint>> {
        self.inner.checkpoints(id, q).await
    }
    async fn checkpoint(&self, id: RunId, session: SessionId) -> Result<Checkpoint> {
        self.inner.checkpoint(id, session).await
    }
    async fn artifacts(&self, id: RunId, q: PageQuery) -> Result<Page<Artifact>> {
        self.inner.artifacts(id, q).await
    }
    async fn artifact(&self, id: RunId, a: ArtifactId) -> Result<Artifact> {
        self.inner.artifact(id, a).await
    }
    async fn download(&self, id: RunId, a: ArtifactId) -> Result<Download> {
        self.inner.download(id, a).await
    }
}

async fn transient_attachment_failure(
    history_failure_at: usize,
    error_code: &'static str,
    no_anchors: bool,
) {
    let root = playtime_test_support::TestRoot::new().unwrap();
    let credentials = playtime_local::Credentials::initialize(&root.join("service")).unwrap();
    let service = ServiceId::new("review-transient").unwrap();
    let app = Arc::new(TransientFailure {
        inner: Fixture::new(service.clone()),
        run_once: AtomicBool::new(history_failure_at == 0),
        history_failure_at,
        error_code,
        anchors_once: AtomicBool::new(no_anchors),
        history_attempts: AtomicUsize::new(0),
        history_cursors: Mutex::new(Vec::new()),
    });
    let preview = app
        .inner
        .preview(
            RecipeId::new("fixture-example").unwrap(),
            PreviewRequest {
                mode: RunMode::Simulation,
            },
        )
        .await
        .unwrap();
    let admitted = app
        .inner
        .admit(
            IdempotencyKey::new("review-launch").unwrap(),
            LaunchRequest {
                recipe_id: preview.recipe_id,
                mode: preview.mode,
                preview_revision: preview.preview_revision,
            },
        )
        .await
        .unwrap();
    app.inner.reveal(
        &admitted.run_id,
        4,
        Some(StreamEndStatus::CommittedCompletion),
    );
    let server = Server::new(app.clone(), credentials.token.clone())
        .unwrap()
        .listen("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    credentials
        .publish(&Connection {
            schema_version: 1,
            endpoint: server.endpoint(),
            service_id: service,
            token_file: credentials.token_path.to_string_lossy().into(),
        })
        .unwrap();
    let config = root.join("client.toml");
    std::fs::write(
        &config,
        format!(
            "connection_file = {}\n",
            serde_json::to_string(&credentials.connection_path.to_string_lossy()).unwrap()
        ),
    )
    .unwrap();
    let mut backend = Backend::start(egui::Context::default());
    let mut model = Model::default();
    model.connect(playtime_client::config::Options {
        connection_file: Some(credentials.connection_path),
        config: Some(config),
        state_dir: Some(root.join("state")),
        ..Default::default()
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while model.connection != ConnectionState::Connected {
            backend.pump(&mut model);
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    model.select_run(admitted.run_id);
    let mut reconnecting = false;
    let mut waiting = false;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            backend.pump(&mut model);
            reconnecting |= model.connection == ConnectionState::Reconnecting;
            waiting |= model.archive_waiting;
            if model.window.metrics.records == 4
                || matches!(model.connection, ConnectionState::Error(_))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let records = model.window.metrics.records;
    let state = model.connection.clone();
    drop(backend);
    server.stop().await.unwrap();
    assert_eq!(reconnecting, error_code == "backend_unavailable");
    assert_eq!(waiting, no_anchors || error_code == "evidence_not_ready");
    assert!(!model.archive_waiting, "archive waiting did not clear");
    assert_eq!(state, ConnectionState::Connected);
    assert_eq!(
        records,
        4,
        "one retryable HTTP failure terminated attachment: {state:?}; history attempts={}",
        app.history_attempts.load(Ordering::SeqCst)
    );
    if history_failure_at > 0 {
        let cursors = app.history_cursors.lock().unwrap();
        assert_eq!(
            cursors[history_failure_at - 1],
            cursors[history_failure_at],
            "retry must preserve the failed page's cursor"
        );
        if history_failure_at == 2 {
            assert_ne!(
                cursors[0], cursors[1],
                "failure must occur after page advancement"
            );
        }
    }
}

#[tokio::test]
async fn one_transient_history_failure_must_reconnect() {
    transient_attachment_failure(1, "backend_unavailable", false).await;
}

#[tokio::test]
async fn one_transient_initial_run_failure_must_reconnect() {
    transient_attachment_failure(0, "backend_unavailable", false).await;
}

#[tokio::test]
async fn later_history_failure_retries_the_same_page_after_progress() {
    transient_attachment_failure(2, "backend_unavailable", false).await;
}

fn unresolved_receipt(
    app: &playtime_ui::ui::Workbench,
    key: &str,
) -> playtime_client::state::LaunchReceipt {
    let preview = &app.model.preview.as_ref().unwrap().value;
    playtime_client::state::LaunchReceipt {
        schema_version: 1,
        service_id: app.model.service.clone().unwrap(),
        endpoint: app.model.endpoint.clone(),
        key: IdempotencyKey::new(key).unwrap(),
        request: LaunchRequest {
            recipe_id: preview.recipe_id.clone(),
            mode: preview.mode,
            preview_revision: preview.preview_revision.clone(),
        },
        created_at_unix_ms: 0,
        run_id: None,
    }
}

#[tokio::test]
async fn an_unresolved_receipt_never_widens_the_run_panel() {
    use egui_kittest::{Harness, kittest::Queryable};
    let mut app = common::demo(true).await;
    // Workbench keys have 48 characters, wider than the panel at any width.
    let key = "6dfc3079e9a7ee01e4920ada02397e59818d43e06f10b6fb";
    app.model.receipts = vec![unresolved_receipt(&app, key)];
    let mut h = Harness::builder()
        .with_size([1440.0, 900.0])
        .build_ui_state(|ui, app: &mut playtime_ui::ui::Workbench| app.draw(ui), app);
    h.run();
    let panel = h.state().model.preferences.left_width;
    let retry = h.get_by_label_contains("Retry 6dfc3079").rect();
    let launch = h.get_by_label("Launch experiment").rect();
    assert!(
        panel <= 360.0 && retry.right() <= panel && launch.left() < panel + 24.0,
        "panel {panel}, retry {retry:?}, launcher {launch:?}"
    );
    h.get_by_label_contains("Retry 6dfc3079").click();
    h.run();
    assert!(
        h.state()
            .model
            .commands
            .iter()
            .any(|c| matches!(&c.action, Action::Recover(saved, true) if saved.as_str() == key))
    );
}

#[test]
fn expanding_an_activity_keeps_it_in_place() {
    use egui_kittest::{Harness, kittest::Queryable};
    use playtime_ui::transcript::{Row, Section, Window};
    let mut app = playtime_ui::ui::Workbench::default();
    app.model.launcher = false;
    app.model.following = false;
    app.model.window = Window {
        total: 40,
        rows: (0..40)
            .map(|i| Row {
                id: i,
                first_sequence: i as u64 + 1,
                title: format!("s1 · Model r{i}"),
                excerpt: "Final response recorded".into(),
                status: "Recorded".into(),
                sources: "1 source record".into(),
                sections: vec![Section {
                    label: "Recorded thinking".into(),
                    text: "Long recorded thinking. ".repeat(30),
                }],
                needs_loading: false,
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    };
    let mut h = Harness::builder()
        .with_size([1440.0, 900.0])
        .build_ui_state(|ui, app: &mut playtime_ui::ui::Workbench| app.draw(ui), app);
    h.run();
    h.hover_at(egui::pos2(670.0, 600.0));
    h.event(egui::Event::MouseWheel {
        unit: egui::MouseWheelUnit::Point,
        delta: egui::vec2(0.0, -2400.0),
        phase: egui::TouchPhase::Move,
        modifiers: egui::Modifiers::NONE,
    });
    // Wheel scrolling animates over several frames.
    h.run_steps(8);
    // Every row shares one height, so the lowest visible row moves furthest.
    let (sequence, before) = (1..=40)
        .filter_map(|n| {
            h.query_by_label(&format!("Expand activity {n}"))
                .map(|node| (n, node.rect()))
        })
        .filter(|(_, rect)| rect.top() > 300.0 && rect.bottom() < 850.0)
        .last()
        .expect("a visible row after scrolling");
    assert!(sequence > 5, "the wheel did not scroll the timeline");
    for (from, to) in [("Expand", "Collapse"), ("Collapse", "Expand")] {
        h.get_by_label(&format!("{from} activity {sequence}"))
            .click();
        h.run();
        let after = h.get_by_label(&format!("{to} activity {sequence}")).rect();
        assert!(
            (after.top() - before.top()).abs() < 2.0,
            "{from} moved activity {sequence} from {} to {}",
            before.top(),
            after.top()
        );
    }
}

#[tokio::test]
async fn released_and_stranded_runs_say_so_and_offer_no_cancel() {
    use egui_kittest::{Harness, kittest::Queryable};
    let mut app = common::demo(false).await;
    app.model.content = None;
    let mut stranded = app.model.run.as_ref().unwrap().value.clone();
    stranded.orchestration = Orchestration {
        state: OrchestrationState::Reconciling,
        reason: Some("worker_unavailable".into()),
    };
    let prepared = prepare_run(stranded.clone());
    assert!(
        prepared.summary[0].starts_with("Reconciling · worker unavailable · ")
            && !prepared.summary[0].contains("Some("),
        "{}",
        prepared.summary[0]
    );
    app.model.runs = vec![stranded.clone()];
    app.model.run = Some(prepared);
    let mut h = Harness::builder()
        .with_size([1440.0, 900.0])
        .build_ui_state(|ui, app: &mut playtime_ui::ui::Workbench| app.draw(ui), app);
    h.run();
    let run = stranded.run_id.as_str().to_owned();
    h.get_by_label(&format!("Run {run} · Reconciling · needs release"));
    h.get_by_label_contains(&format!("playtime runs release {run} --reason TEXT"));
    h.get_by_label("Cancel run").click();
    h.run();

    // A cancel the worker already obeyed leaves nothing to cancel either.
    let mut stopped = cancelled(stranded.clone());
    stopped.cancellation.as_mut().unwrap().enforcement = Enforcement::Stopped;
    h.state_mut().model.run = Some(prepare_run(stopped));
    h.run();
    h.get_by_label("Cancel run").click();
    h.run();
    assert!(
        !h.state()
            .model
            .commands
            .iter()
            .any(|c| matches!(c.action, Action::Cancel(_)))
    );

    let mut released = stranded;
    released.orchestration = Orchestration {
        state: OrchestrationState::Terminal,
        reason: Some("operator_released".into()),
    };
    let summary = prepare_run(released.clone()).summary;
    assert!(
        summary[0].starts_with("Ended · released by operator · "),
        "{}",
        summary[0]
    );
    h.state_mut().model.runs = vec![released];
    h.run();
    h.get_by_label(&format!("Run {run} · Ended · released"));
}

#[tokio::test]
async fn a_busy_refusal_names_the_run_holding_the_slot() {
    let mut app = common::demo(true).await;
    let mut holder = app.model.run.as_ref().unwrap().value.clone();
    holder.orchestration = Orchestration {
        state: OrchestrationState::Reconciling,
        reason: Some("worker_unavailable".into()),
    };
    app.model.runs = vec![holder.clone()];
    app.model.saved = Some(unresolved_receipt(&app, "refused-busy"));
    app.model.launch();
    app.model.commands.clear();
    app.model.apply(Reply {
        tag: app.model.tag,
        run: None,
        data: Data::LaunchRefused("HTTP 409: run_busy".into(), true),
    });
    let Mutation::Rejected(message) = &app.model.launch else {
        panic!("{:?}", app.model.launch);
    };
    assert!(
        message.contains(&format!(
            "playtime runs release {} --reason TEXT",
            holder.run_id
        )),
        "{message}"
    );
    assert!(app.model.saved.is_none() && app.model.can_launch());
    assert!(
        app.model
            .commands
            .iter()
            .any(|c| matches!(c.action, Action::Receipts(None)))
    );
}

#[tokio::test]
async fn unpublished_archive_waits_and_clears_without_connection_failure() {
    transient_attachment_failure(1, "evidence_not_ready", false).await;
    transient_attachment_failure(0, "evidence_not_ready", true).await;
}

#[tokio::test]
async fn long_recipe_names_cannot_hide_run_state_or_release_action() {
    use egui_kittest::{Harness, kittest::Queryable};
    for width in [240.0, 180.0] {
        let mut app = common::long_recipe_runs().await;
        app.model.preferences.left_width = width;
        let mut h = Harness::builder()
            .with_size([1440.0, 900.0])
            .build_ui_state(|ui, app: &mut playtime_ui::ui::Workbench| app.draw(ui), app);
        h.run();
        let right = h
            .get_all_by_label("New experiment")
            .map(|node| node.rect())
            .min_by(|a, b| a.left().total_cmp(&b.left()))
            .unwrap()
            .right();
        let mut bottom = h.get_by_label("Refresh runs").rect().bottom();
        for (id, state) in [
            ("needs-release", "Reconciling · needs release"),
            ("running", "Running"),
            ("ended", "Ended"),
        ] {
            // A visible, untruncated label, separate from the button's accessible name.
            let label = h.get_by_label(state).rect();
            assert!(
                label.right() <= right,
                "{state} exceeds the sidebar at {width}"
            );
            assert!(label.top() > bottom, "run rows overlap at {width}");
            bottom = label.bottom();
            let button = h.get_by_label(&format!("Run {id} · {state}")).rect();
            assert!(button.bottom() <= label.top());
        }
        assert!(bottom < h.get_by_label("Pending launches").rect().top());
        h.get_by_label("Run needs-release · Reconciling · needs release")
            .click();
        h.run();
        assert_eq!(
            h.state().model.selected.as_ref().unwrap().as_str(),
            "needs-release"
        );
    }
}
