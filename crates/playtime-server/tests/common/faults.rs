use async_trait::async_trait;
use playtime_core::{RunId, SessionId};
use playtime_protocol::api::*;
use playtime_server::{application::*, fixture::Fixture};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::watch;

type Hook = Mutex<Option<Box<dyn FnOnce() + Send>>>;
pub struct Faults {
    pub inner: Arc<Fixture>,
    pub before_subscribe: Hook,
    pub after_subscribe: Hook,
    pub after_history: Hook,
    pub admission_delay: Duration,
    pub preview_size: usize,
    pub bad_download: bool,
    pub stalled_download: bool,
    pub writers: Mutex<Vec<tokio::io::DuplexStream>>,
}
impl Faults {
    pub fn new(inner: Arc<Fixture>) -> Self {
        Self {
            inner,
            before_subscribe: Mutex::new(None),
            after_subscribe: Mutex::new(None),
            after_history: Mutex::new(None),
            admission_delay: Duration::ZERO,
            preview_size: 0,
            bad_download: false,
            stalled_download: false,
            writers: Mutex::new(vec![]),
        }
    }
    fn fire(hook: &Hook) {
        if let Some(call) = hook.lock().unwrap().take() {
            call();
        }
    }
}
#[async_trait]
impl Application for Faults {
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }
    async fn model_review(&self, request: ModelInspectRequest) -> Result<ModelRegistrationReview> {
        self.inner.model_review(request).await
    }
    async fn register_model(
        &self,
        key: IdempotencyKey,
        request: ModelRegistrationRequest,
    ) -> Result<ModelRegistrationResponse> {
        let result = self.inner.register_model(key, request).await;
        tokio::time::sleep(self.admission_delay).await;
        result
    }
    async fn model_registration_lookup(
        &self,
        key: IdempotencyKey,
    ) -> Result<ModelRegistrationResponse> {
        self.inner.model_registration_lookup(key).await
    }
    async fn recipes(&self, q: PageQuery) -> Result<Page<Recipe>> {
        self.inner.recipes(q).await
    }
    async fn recipe_model_settings(&self, id: RecipeId) -> Result<RecipeSettingsView> {
        self.inner.recipe_model_settings(id).await
    }
    async fn recipe_definition(&self, id: RecipeId) -> Result<RecipeDefinition> {
        self.inner.recipe_definition(id).await
    }
    async fn recipe_save(
        &self,
        key: IdempotencyKey,
        request: RecipeMutation,
    ) -> Result<RecipeSaveResponse> {
        let result = self.inner.recipe_save(key, request).await;
        tokio::time::sleep(self.admission_delay).await;
        result
    }
    async fn recipe_save_lookup(&self, key: IdempotencyKey) -> Result<RecipeSaveResponse> {
        self.inner.recipe_save_lookup(key).await
    }
    async fn saved_recipes(
        &self,
        query: PageQuery,
        history: Option<RecipeId>,
    ) -> Result<Page<Recipe>> {
        self.inner.saved_recipes(query, history).await
    }
    async fn saved_recipe(&self, id: RecipeId) -> Result<SavedRecipe> {
        self.inner.saved_recipe(id).await
    }
    async fn preview(&self, r: RecipeId, q: PreviewRequest) -> Result<Preview> {
        let mut value = self.inner.preview(r, q).await?;
        if self.preview_size > 0 {
            value.prompt = "x".repeat(self.preview_size);
        }
        Ok(value)
    }
    async fn admit(&self, key: IdempotencyKey, q: LaunchRequest) -> Result<LaunchResponse> {
        let result = self.inner.admit(key, q).await;
        tokio::time::sleep(self.admission_delay).await;
        result
    }
    async fn launch(&self, key: IdempotencyKey) -> Result<LaunchResponse> {
        self.inner.launch(key).await
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
        c: Option<EventCursor>,
        follow: bool,
        limit: u32,
        bytes: usize,
    ) -> Result<History> {
        let result = self.inner.history(id, c, follow, limit, bytes).await;
        Self::fire(&self.after_history);
        result
    }
    async fn subscribe(&self, id: RunId) -> Result<watch::Receiver<LiveState>> {
        Self::fire(&self.before_subscribe);
        let result = self.inner.subscribe(id).await;
        Self::fire(&self.after_subscribe);
        result
    }
    async fn checkpoints(&self, id: RunId, q: PageQuery) -> Result<Page<Checkpoint>> {
        self.inner.checkpoints(id, q).await
    }
    async fn checkpoint(&self, id: RunId, s: SessionId) -> Result<Checkpoint> {
        self.inner.checkpoint(id, s).await
    }
    async fn artifacts(&self, id: RunId, q: PageQuery) -> Result<Page<Artifact>> {
        self.inner.artifacts(id, q).await
    }
    async fn artifact(&self, id: RunId, a: ArtifactId) -> Result<Artifact> {
        self.inner.artifact(id, a).await
    }
    async fn download(&self, id: RunId, a: ArtifactId) -> Result<Download> {
        let mut download = self.inner.download(id, a).await?;
        if self.bad_download {
            download.reader = Box::pin(std::io::Cursor::new(vec![
                b'x';
                download.artifact.content.size_bytes
                    as usize
            ]));
        }
        if self.stalled_download {
            let (reader, writer) = tokio::io::duplex(16);
            self.writers.lock().unwrap().push(writer);
            download.reader = Box::pin(reader);
        }
        Ok(download)
    }
}
