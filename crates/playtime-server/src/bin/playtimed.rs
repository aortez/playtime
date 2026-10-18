use playtime_protocol::api::Connection;
use playtime_server::{
    application::Application,
    auth::Credentials,
    daemon::{Daemon, test_supervisor::CpuTestSupervisor},
    http::Server,
    lifecycle::{ports::Supervisor, systemd::SystemdSupervisor},
};
use std::{net::SocketAddr, path::PathBuf, sync::Arc};
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut root = None;
    let mut bundle = None;
    let mut cpu_test = false;
    let mut listen: SocketAddr = "127.0.0.1:8766".parse()?;
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--root" => root = args.next().map(PathBuf::from),
            "--bundle" => bundle = args.next().map(PathBuf::from),
            "--listen" => listen = args.next().ok_or("--listen needs an address")?.parse()?,
            "--cpu-test-supervisor" => cpu_test = true,
            _ => return Err("usage: playtimed --root <existing-private-root> --bundle <frozen-bundle> [--listen 127.0.0.1:8766] [--cpu-test-supervisor]".into()),
        }
    }
    let root = root.ok_or("--root is required")?;
    let bundle = bundle
        .ok_or("--bundle is required; build it explicitly with xtask before running the daemon")?;
    if !listen.ip().is_loopback() {
        return Err("only loopback listeners are supported".into());
    }
    let supervisor: Arc<dyn Supervisor> = if cpu_test {
        Arc::new(CpuTestSupervisor::default())
    } else {
        Arc::new(SystemdSupervisor::connect().await?)
    };
    let daemon = Arc::new(Daemon::open(&root, &bundle, supervisor, cpu_test).await?);
    let credentials = Credentials::initialize(&root)?;
    let server = Server::new(daemon.clone(), credentials.token.clone())?
        .listen(listen)
        .await?;
    credentials.publish(&Connection {
        schema_version: 1,
        endpoint: server.endpoint(),
        service_id: daemon.capabilities().service_id,
        token_file: credentials.token_path.to_string_lossy().into_owned(),
    })?;
    println!(
        "playtimed {} ({})",
        server.endpoint(),
        if cpu_test {
            "CPU test supervisor; no systemd enforcement"
        } else {
            "systemd user supervisor"
        }
    );
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! { _ = tokio::signal::ctrl_c() => (), _ = terminate.recv() => () }
    server.stop().await?;
    Ok(())
}
