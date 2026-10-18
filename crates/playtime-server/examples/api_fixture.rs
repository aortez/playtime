//! CPU-only bounded scripted service. See docs/rust-http-api.md.
use playtime_protocol::api::{Connection, ServiceId, StreamEndStatus};
use playtime_server::{
    application::Application, auth::Credentials, fixture::Fixture, http::Server,
};
use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut root = None;
    let mut listen: SocketAddr = "127.0.0.1:8766".parse()?;
    let mut lifetime = 120u64;
    let mut identity = ServiceId::new("fixture-service")?;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let value = args.next().ok_or("expected a flag value")?;
        match arg.as_str() {
            "--root" => root = Some(PathBuf::from(value)),
            "--listen" => listen = value.parse()?,
            "--lifetime-seconds" => lifetime = value.parse()?,
            "--service-id" => identity = ServiceId::new(value)?,
            _ => return Err("unknown flag".into()),
        }
    }
    if lifetime == 0 || lifetime > 3600 {
        return Err("lifetime must be 1..3600 seconds".into());
    }
    if !listen.ip().is_loopback() {
        return Err("only loopback listeners are supported".into());
    }
    let root = root.ok_or("--root is required")?;
    let credentials = Credentials::initialize(&root)?;
    let fixture = Arc::new(Fixture::new(identity.clone()));
    let server = Server::new(fixture.clone(), credentials.token.clone())?;
    let running = server.listen(listen).await?;
    credentials.publish(&Connection {
        schema_version: 1,
        endpoint: running.endpoint(),
        service_id: identity,
        token_file: credentials.token_path.to_string_lossy().into_owned(),
    })?;
    println!("Scripted HTTP fixture: {}", running.endpoint());
    println!("Discovery: {}", credentials.connection_path.display());
    println!(
        "No controller, model, workshop, worker, or durable admission ledger. Lifetime: {lifetime}s."
    );
    let deadline = tokio::time::Instant::now() + Duration::from_secs(lifetime);
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => break,
            _ = tokio::time::sleep(Duration::from_millis(150)) => {
                for run in fixture.runs(Default::default()).await?.items {
                    if run.orchestration.state == playtime_protocol::api::OrchestrationState::Terminal { continue; }
                    if run.recipe_id.as_str() == "fixture-empty" {
                        fixture.reveal(&run.run_id, 0, Some(StreamEndStatus::Incomplete));
                        continue;
                    }
                    let sequence = run.last_confirmed_frontier.unwrap().prefix.sequence;
                    let next = (sequence + 1).min(4) as usize;
                    fixture.reveal(&run.run_id, next, (next == 4).then_some(StreamEndStatus::CommittedCompletion));
                }
            }
        }
    }
    running.stop().await?;
    Ok(())
}
