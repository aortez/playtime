use playtime_local::Credentials;
use playtime_protocol::api::{Connection, ServiceId};
use playtime_server::http::Server;
use playtime_test_support::fixture::Script;
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::signal::unix::{SignalKind, signal};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut root = None;
    let mut scenario = "normal".to_owned();
    let mut service = ServiceId::new("cli-fixture")?;
    let mut listen = "127.0.0.1:0".parse()?;
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let value = args.next().ok_or("expected flag value")?;
        match flag.as_str() {
            "--root" => root = Some(PathBuf::from(value)),
            "--scenario" => scenario = value,
            "--service-id" => service = ServiceId::new(value)?,
            "--listen" => listen = value.parse()?,
            _ => return Err("unknown flag".into()),
        }
    }
    if !matches!(
        scenario.as_str(),
        "normal" | "hold" | "unclean" | "abnormal" | "lost-launch" | "corrupt"
    ) {
        return Err("unknown scenario".into());
    }
    let root = root.ok_or("--root required")?;
    let credentials = Credentials::initialize(&root)?;
    let script = Arc::new(Script::new(service.clone(), scenario));
    let server = Server::new(script.clone(), credentials.token.clone())?
        .listen(listen)
        .await?;
    credentials.publish(&Connection {
        schema_version: 1,
        endpoint: server.endpoint(),
        service_id: service,
        token_file: credentials.token_path.to_string_lossy().into_owned(),
    })?;
    println!(
        "Scripted CLI fixture: {} (no controller or workers)",
        server.endpoint()
    );
    let mut term = signal(SignalKind::terminate())?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(180);
    loop {
        tokio::select! { _ = term.recv() => break, _ = tokio::time::sleep_until(deadline) => break, _ = tokio::time::sleep(Duration::from_millis(50)) => () }
        let calls = script.fixture.calls();
        let trace: Vec<_> = calls
            .iter()
            .rev()
            .take(1000)
            .rev()
            .map(|c| serde_json::json!({"operation":c.operation,"id":c.id}))
            .collect();
        std::fs::write(root.join("requests.json"), serde_json::to_vec(&trace)?)?;
    }
    server.stop().await?;
    Ok(())
}
