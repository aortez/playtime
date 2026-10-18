#![allow(dead_code)]
pub mod faults;
use playtime_client::{Client, Observation, PendingLaunch};
use playtime_core::RunMode;
use playtime_protocol::api::*;
use playtime_server::{
    auth::Token,
    fixture::Fixture,
    http::{RunningServer, Server},
};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio_util::sync::CancellationToken;

pub struct Harness {
    pub fixture: Arc<Fixture>,
    pub server: RunningServer,
    pub client: Client,
    pub token: Token,
    pub raw: reqwest::Client,
}
impl Harness {
    pub async fn new() -> Self {
        Self::limits(ApiLimits::default()).await
    }
    pub async fn limits(limits: ApiLimits) -> Self {
        let fixture = Arc::new(Fixture::with_limits(
            ServiceId::new("service-test").unwrap(),
            limits,
        ));
        let token = Token::generate().unwrap();
        let server = Server::new(fixture.clone(), token.clone())
            .unwrap()
            .listen("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let client = Client::connect(
            &server.endpoint(),
            token.clone(),
            Some(ServiceId::new("service-test").unwrap()),
        )
        .await
        .unwrap();
        Self {
            fixture,
            server,
            client,
            token,
            raw: reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
        }
    }
    pub fn get(&self, path: &str) -> reqwest::RequestBuilder {
        self.raw
            .get(format!("{}/api/v1/{path}", self.server.endpoint()))
            .bearer_auth(self.token.expose())
    }
    pub fn post(&self, path: &str) -> reqwest::RequestBuilder {
        self.raw
            .post(format!("{}/api/v1/{path}", self.server.endpoint()))
            .bearer_auth(self.token.expose())
            .header(SERVICE_HEADER, "service-test")
    }
    pub async fn pending(&self, key: &str) -> PendingLaunch {
        let preview = self
            .client
            .preview(
                &RecipeId::new("fixture-example").unwrap(),
                RunMode::Simulation,
            )
            .await
            .unwrap();
        self.client
            .prepare_launch(
                LaunchRequest {
                    recipe_id: preview.recipe_id,
                    mode: preview.mode,
                    preview_revision: preview.preview_revision,
                },
                Some(IdempotencyKey::new(key).unwrap()),
            )
            .unwrap()
    }
}
pub async fn next(stream: &mut playtime_client::EventStream) -> Observation {
    tokio::time::timeout(
        Duration::from_secs(3),
        stream.next(&CancellationToken::new()),
    )
    .await
    .expect("stream deadline")
    .expect("valid stream")
}
pub async fn code(response: reqwest::Response, status: u16, code: &str) {
    assert_eq!(response.status().as_u16(), status);
    assert_eq!(response.headers()["cache-control"], "no-store");
    let value: serde_json::Value = response.json().await.unwrap();
    assert_eq!(value["code"], code);
    assert!(value["request_id"].is_string());
}
pub struct Temp(pub PathBuf);
impl Temp {
    pub fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "playtime-http-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        Self(path)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
