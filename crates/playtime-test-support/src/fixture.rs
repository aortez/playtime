//! Thin scripted replies over #35's Fixture. No controller/archive implementation.
use async_trait::async_trait;
use playtime_core::{ArchivePath, RunId, SessionId, StopReason};
use playtime_protocol::{
    api::*,
    archive::ContentRef,
    bridge::{FileContent, FileVersion},
    execution::CaptureIndex,
    v1::UsageSummary,
};
use playtime_server::{
    application::*,
    fixture::{Fixture, digest},
};
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
        if self.scenario == "capture" {
            // A continuous run: its final workspace is a capture, not a
            // checkpoint.
            view.harness = Some(ArchivedHarness {
                configuration: view.input_manifest.clone().expect("fixture manifest"),
                harness: HarnessView::continuous(Default::default()).expect("continuous harness"),
            });
        }
        view
    }
    /// The "capture" scenario's final capture: the index of a continuous
    /// run's stopped workspace and the files it retains, as the daemon lists
    /// them beside the fixture's other artifacts. One retained file's artifact
    /// is deliberately missing from the listing, and one name is not a path.
    fn capture(run: &RunId) -> Vec<(Artifact, Vec<u8>)> {
        let reference = |path: &str, bytes: &[u8]| ContentRef {
            path: ArchivePath::new(path).unwrap(),
            sha256: digest(bytes),
            size_bytes: bytes.len() as u64,
        };
        let retained = |path: &str, content: ContentRef| FileVersion {
            path: path.into(),
            size_bytes: content.size_bytes,
            mode: 0o644,
            entry_type: "0".into(),
            sha256: Some(content.sha256.clone()),
            link: None,
            content: FileContent::Retained { content },
        };
        let unavailable = |path: &str, entry_type: &str, reason: &str| FileVersion {
            path: path.into(),
            size_bytes: 0,
            mode: 0o644,
            entry_type: entry_type.into(),
            sha256: None,
            link: None,
            content: FileContent::Unavailable {
                reason: reason.into(),
            },
        };
        let image = include_bytes!("../../playtime-server/tests/fixtures/pixel.png").as_slice();
        let note = b"captured note\n".as_slice();
        let image_ref = reference("artifacts/final/files/0", image);
        let note_ref = reference("artifacts/final/files/1", note);
        let index = CaptureIndex {
            schema_version: 2,
            run_id: run.clone(),
            workspace: reference("artifacts/final/workspace.tar", b"synthetic workspace tar"),
            files: vec![
                unavailable("out", "5", "directory"),
                retained("out/saved <image>.png", image_ref.clone()),
                retained("notes/readme.txt", note_ref.clone()),
                retained(
                    "unlisted.txt",
                    reference("artifacts/final/files/2", b"unlisted\n"),
                ),
                unavailable("big.bin", "0", "oversized"),
                retained("../escape.txt", note_ref.clone()),
            ],
            source_map: reference("artifacts/final/source-map.json", b"{}"),
        };
        let index_bytes = serde_json::to_vec(&index).unwrap();
        let artifact = |id: &str, name: &str, kind: &str, content: ContentRef| Artifact {
            artifact_id: ArtifactId::new(id).unwrap(),
            name: name.into(),
            kind: kind.into(),
            content,
            session_id: None,
            published_sequence: 4,
            frozen_input: false,
        };
        vec![
            (
                artifact(
                    "final-capture-index",
                    "capture-index-v2.json",
                    "final_capture_index",
                    reference("artifacts/final/capture-index-v2.json", &index_bytes),
                ),
                index_bytes,
            ),
            (
                artifact(
                    "captured-image",
                    "out/saved <image>.png",
                    "file_version",
                    image_ref,
                ),
                image.to_vec(),
            ),
            (
                artifact(
                    "captured-note",
                    "notes/readme.txt",
                    "file_version",
                    note_ref,
                ),
                note.to_vec(),
            ),
        ]
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
        let mut history = self
            .fixture
            .history(id, cursor, follow, limit, bytes)
            .await?;
        if self.scenario == "conflicting-frontier" && follow && history.after_sequence == 0 {
            // Streams read many records at once. Serve record one alone, so
            // the next read is the one that contradicts it.
            history.events.truncate(1);
            history.has_more = true;
        }
        if self.scenario == "conflicting-frontier" && follow && history.after_sequence == 1 {
            // Deliberately contradict the prefix advertised with record one.
            // The real HTTP client must reject record two before emitting it.
            history.frontier.prefix.sha256 =
                playtime_server::fixture::digest(b"conflicting fixture prefix");
        }
        Ok(history)
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
        let mut page = self.fixture.artifacts(id.clone(), q).await?;
        if self.scenario == "capture" && page.next_cursor.is_none() {
            page.items
                .extend(Self::capture(&id).into_iter().map(|(artifact, _)| artifact));
        }
        Ok(page)
    }
    async fn artifact(&self, id: RunId, a: ArtifactId) -> Result<Artifact> {
        if self.scenario == "capture"
            && let Some((artifact, _)) = Self::capture(&id)
                .into_iter()
                .find(|(artifact, _)| artifact.artifact_id == a)
        {
            return Ok(artifact);
        }
        self.fixture.artifact(id, a).await
    }
    async fn download(&self, id: RunId, a: ArtifactId) -> Result<Download> {
        if self.scenario == "capture"
            && let Some((artifact, bytes)) = Self::capture(&id)
                .into_iter()
                .find(|(artifact, _)| artifact.artifact_id == a)
        {
            return Ok(Download {
                artifact,
                reader: Box::pin(std::io::Cursor::new(bytes)),
            });
        }
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
