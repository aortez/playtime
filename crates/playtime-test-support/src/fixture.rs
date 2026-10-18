//! Thin scripted replies over #35's Fixture. No controller/archive implementation.
use async_trait::async_trait;
use playtime_core::{RunId, SessionId, StopReason};
use playtime_protocol::{api::*, v1::UsageSummary};
use playtime_server::{application::*, fixture::Fixture};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::watch;

pub struct Script {
    pub fixture: Arc<Fixture>,
    pub scenario: String,
    lost: AtomicBool,
}
impl Script {
    pub fn new(service: ServiceId, scenario: String) -> Self {
        let limits = ApiLimits {
            handler_seconds: 1,
            ..Default::default()
        };
        Self {
            fixture: Arc::new(Fixture::with_limits(service, limits)),
            scenario,
            lost: AtomicBool::new(false),
        }
    }
    fn usage() -> UsageSummary {
        serde_json::from_str(include_str!(
            "../../playtime-protocol/tests/fixtures/usage-unknown.json"
        ))
        .unwrap()
    }
    fn project(&self, mut view: RunView) -> RunView {
        if view.usage.is_some() {
            view.usage = Some(Self::usage());
            if self.scenario == "abnormal" || view.cancellation.is_some() {
                view.outcome.as_mut().unwrap().reason = StopReason::Interrupted;
            }
        }
        if view.orchestration.state == OrchestrationState::Terminal && self.scenario != "unclean" {
            view.cleanup = Cleanup::Complete;
            if let Some(intent) = &mut view.cancellation {
                intent.enforcement = Enforcement::Stopped;
                intent.uncertainty = None;
            }
        }
        view
    }
    fn script(&self, run: &RunId) {
        let reason = if self.scenario == "abnormal" || self.scenario == "hold" {
            "interrupted"
        } else {
            "agent_finished"
        };
        self.fixture.replace_pending_record(run, 3, serde_json::json!({"reason":reason,"usage":Self::usage(),"cleanup":{"status":if self.scenario == "unclean" {"unknown"} else {"complete"}},"last_checkpoint":null}));
    }
}
#[async_trait]
impl Application for Script {
    fn capabilities(&self) -> Capabilities {
        self.fixture.capabilities()
    }
    async fn recipes(&self, q: PageQuery) -> Result<Page<Recipe>> {
        self.fixture.recipes(q).await
    }
    async fn preview(&self, id: RecipeId, q: PreviewRequest) -> Result<Preview> {
        self.fixture.preview(id, q).await
    }
    async fn admit(&self, key: IdempotencyKey, request: LaunchRequest) -> Result<LaunchResponse> {
        let empty = request.recipe_id.as_str() == "fixture-empty";
        let mut result = self.fixture.admit(key, request).await?;
        if !result.replayed {
            if !empty {
                self.script(&result.run_id);
            }
            if self.scenario != "hold" {
                let fixture = self.fixture.clone();
                let run = result.run_id.clone();
                tokio::spawn(async move {
                    if empty {
                        fixture.reveal(&run, 0, Some(StreamEndStatus::Incomplete));
                        return;
                    }
                    for n in 1..=4 {
                        tokio::time::sleep(Duration::from_millis(80)).await;
                        fixture.reveal(
                            &run,
                            n,
                            (n == 4).then_some(StreamEndStatus::CommittedCompletion),
                        );
                    }
                });
            }
            if self.scenario == "lost-launch" && !self.lost.swap(true, Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(1500)).await;
            }
        }
        result.run = self.project(result.run);
        Ok(result)
    }
    async fn launch(&self, key: IdempotencyKey) -> Result<LaunchResponse> {
        let mut r = self.fixture.launch(key).await?;
        r.run = self.project(r.run);
        Ok(r)
    }
    async fn runs(&self, q: PageQuery) -> Result<Page<RunView>> {
        let mut p = self.fixture.runs(q).await?;
        p.items = p.items.into_iter().map(|r| self.project(r)).collect();
        Ok(p)
    }
    async fn run(&self, id: RunId) -> Result<RunView> {
        Ok(self.project(self.fixture.run(id).await?))
    }
    async fn cancel(&self, id: RunId) -> Result<CancelResponse> {
        let mut r = self.fixture.cancel(id.clone()).await?;
        if self.scenario == "hold" {
            let fixture = self.fixture.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(100)).await;
                fixture.reveal(&id, 4, Some(StreamEndStatus::Interrupted));
            });
        }
        r.run = self.project(r.run);
        r.intent = r.run.cancellation.clone();
        Ok(r)
    }
    async fn history(
        &self,
        id: RunId,
        cursor: Option<EventCursor>,
        follow: bool,
        limit: u32,
        bytes: usize,
    ) -> Result<History> {
        self.fixture.history(id, cursor, follow, limit, bytes).await
    }
    async fn subscribe(&self, id: RunId) -> Result<watch::Receiver<LiveState>> {
        self.fixture.subscribe(id).await
    }
    async fn checkpoints(&self, id: RunId, q: PageQuery) -> Result<Page<Checkpoint>> {
        self.fixture.checkpoints(id, q).await
    }
    async fn checkpoint(&self, id: RunId, session: SessionId) -> Result<Checkpoint> {
        self.fixture.checkpoint(id, session).await
    }
    async fn artifacts(&self, id: RunId, q: PageQuery) -> Result<Page<Artifact>> {
        self.fixture.artifacts(id, q).await
    }
    async fn artifact(&self, id: RunId, a: ArtifactId) -> Result<Artifact> {
        self.fixture.artifact(id, a).await
    }
    async fn download(&self, id: RunId, a: ArtifactId) -> Result<Download> {
        let mut d = self.fixture.download(id, a).await?;
        if self.scenario == "corrupt" {
            d.reader = Box::pin(std::io::Cursor::new(vec![
                b'x';
                d.artifact.content.size_bytes
                    as usize
            ]));
        }
        Ok(d)
    }
}
