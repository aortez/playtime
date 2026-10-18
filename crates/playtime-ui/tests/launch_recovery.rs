use playtime_client::state::ReceiptStore;
use playtime_core::{RunId, RunMode, SessionId};
use playtime_protocol::api::*;
use playtime_server::{application::*, http::Server};
use playtime_test_support::{TestRoot, fixture::Script};
use playtime_ui::{backend::Backend, model::*};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::watch;

#[derive(Clone, Copy, Debug)]
enum Failure {
    Lookup(&'static str),
    Retry(&'static str),
    InvalidLookup,
    MissingReceipt,
}
struct RecoveryFixture {
    inner: Script,
    failure: Mutex<Option<Failure>>,
}
#[async_trait::async_trait]
impl Application for RecoveryFixture {
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
        if let Some(Failure::Retry(code)) = *self.failure.lock().unwrap() {
            return Err(ApiError::new(code));
        }
        self.inner.admit(key, request).await
    }
    async fn launch(&self, key: IdempotencyKey) -> Result<LaunchResponse> {
        let failure = *self.failure.lock().unwrap();
        if let Some(Failure::Lookup(code)) = failure {
            return Err(ApiError::new(code));
        }
        let mut reply = self.inner.launch(key).await?;
        if matches!(failure, Some(Failure::InvalidLookup)) {
            reply.run_id = RunId::new("mismatched-response").unwrap();
        }
        Ok(reply)
    }
    async fn runs(&self, q: PageQuery) -> Result<Page<RunView>> {
        self.inner.runs(q).await
    }
    async fn run(&self, id: RunId) -> Result<RunView> {
        self.inner.run(id).await
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

async fn until(model: &mut Model, backend: &mut Backend, predicate: impl Fn(&Model) -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            backend.pump(model);
            if predicate(model) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}
async fn failed_recovery(failure: Failure, resolve_by_retry: bool) {
    let root = TestRoot::new().unwrap();
    let credentials = playtime_local::Credentials::initialize(&root.join("service")).unwrap();
    let service = ServiceId::new("review-lookup-outage").unwrap();
    let app = Arc::new(RecoveryFixture {
        inner: Script::new(service.clone(), "lost-launch".into()),
        failure: Mutex::new(None),
    });
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
        connection_file: Some(credentials.connection_path.clone()),
        config: Some(config),
        state_dir: Some(root.join("state")),
        ..Default::default()
    });
    until(&mut model, &mut backend, |m| {
        m.connection == ConnectionState::Connected
    })
    .await;
    model.choose_recipe(
        RecipeId::new("fixture-example").unwrap(),
        RunMode::Simulation,
    );
    until(&mut model, &mut backend, |m| m.can_launch()).await;
    model.launch();
    until(&mut model, &mut backend, |m| {
        matches!(m.launch, Mutation::Unresolved(_))
    })
    .await;
    let saved = model.saved.clone().unwrap();
    let key = saved.key.clone();
    let store = ReceiptStore::new(&root.join("state"));
    assert_eq!(store.read(&key).unwrap().as_ref(), Some(&saved));
    let runs = app.inner.fixture.runs(PageQuery::default()).await.unwrap();
    assert_eq!(runs.items.len(), 1);
    let original_run = runs.items[0].run_id.clone();
    assert!(!model.can_launch());
    *app.failure.lock().unwrap() = Some(failure);
    let held_receipt = root.join("held-receipt.json");
    if matches!(failure, Failure::MissingReceipt) {
        std::fs::rename(store.path(&key), &held_receipt).unwrap();
    }
    model.command(Action::Recover(
        key.clone(),
        matches!(failure, Failure::Retry(_)),
    ));
    until(&mut model, &mut backend, |m| {
        !matches!(m.launch, Mutation::Submitting)
    })
    .await;
    let lookup_state = model.launch.clone();
    let fresh_launch_enabled = model.can_launch();
    if fresh_launch_enabled {
        *app.failure.lock().unwrap() = None;
        model.launch();
        until(&mut model, &mut backend, |m| {
            matches!(m.launch, Mutation::Accepted)
        })
        .await;
    }
    let run_count = app
        .inner
        .fixture
        .runs(PageQuery::default())
        .await
        .unwrap()
        .items
        .len();
    assert!(
        matches!(lookup_state, Mutation::Unresolved(_)) && !fresh_launch_enabled && run_count == 1,
        "{failure:?} resolved ambiguity to {lookup_state:?}; fresh launch enabled={fresh_launch_enabled}; remote runs={run_count}; original key={key}"
    );
    assert_eq!(model.saved.as_ref(), Some(&saved));
    if matches!(failure, Failure::MissingReceipt) {
        std::fs::rename(&held_receipt, store.path(&key)).unwrap();
    }
    assert_eq!(store.read(&key).unwrap().as_ref(), Some(&saved));

    model.choose_recipe(saved.request.recipe_id.clone(), saved.request.mode);
    until(&mut model, &mut backend, |m| m.preview.is_some()).await;
    assert!(
        !model.can_launch(),
        "refresh cannot resolve the failed recovery"
    );

    *app.failure.lock().unwrap() = None;
    model.command(Action::Recover(key.clone(), resolve_by_retry));
    until(&mut model, &mut backend, |m| {
        matches!(m.launch, Mutation::Accepted)
    })
    .await;
    let acknowledged = model.saved.as_ref().unwrap();
    assert_eq!(acknowledged.pending(), saved.pending());
    assert_eq!(acknowledged.run_id.as_ref(), Some(&original_run));
    assert_eq!(store.read(&key).unwrap().as_ref(), Some(acknowledged));
    let runs = app.inner.fixture.runs(PageQuery::default()).await.unwrap();
    assert_eq!(runs.items.len(), 1);
    assert_eq!(runs.items[0].run_id, original_run);
    drop(backend);
    server.stop().await.unwrap();
}

#[tokio::test]
async fn failed_lookup_must_not_unlock_a_fresh_launch_after_a_lost_reply() {
    failed_recovery(Failure::Lookup("backend_unavailable"), false).await;
}
#[tokio::test]
async fn missing_lookup_keeps_original_key_for_explicit_retry() {
    failed_recovery(Failure::Lookup("not_found"), true).await;
}
#[tokio::test]
async fn invalid_lookup_response_does_not_resolve_the_original_launch() {
    failed_recovery(Failure::InvalidLookup, false).await;
}
#[tokio::test]
async fn failed_retry_keeps_original_key_for_later_lookup() {
    failed_recovery(Failure::Retry("backend_unavailable"), false).await;
}
#[tokio::test]
async fn rejected_retry_does_not_reject_the_original_launch() {
    failed_recovery(Failure::Retry("stale_preview"), true).await;
}
#[tokio::test]
async fn local_recovery_failure_preserves_original_receipt_and_ambiguity() {
    failed_recovery(Failure::MissingReceipt, false).await;
}
