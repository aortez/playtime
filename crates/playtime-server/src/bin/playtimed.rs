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
    let mut docker = false;
    let mut docker_executable = PathBuf::from("/usr/bin/docker");
    let mut docker_socket = PathBuf::from("/var/run/docker.sock");
    let mut ollama_endpoint = None;
    let mut listen: SocketAddr = "127.0.0.1:8766".parse()?;
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--root" => root = args.next().map(PathBuf::from),
            "--bundle" => bundle = args.next().map(PathBuf::from),
            "--listen" => listen = args.next().ok_or("--listen needs an address")?.parse()?,
            "--cpu-test-supervisor" => cpu_test = true,
            "--docker-scripted" => docker = true,
            "--docker-executable" => docker_executable = args.next().ok_or("missing Docker executable")?.into(),
            "--docker-socket" => docker_socket = args.next().ok_or("missing Docker socket")?.into(),
            "--ollama-endpoint" => ollama_endpoint = Some(args.next().ok_or("missing Ollama endpoint")?),
            _ => return Err("usage: playtimed --root <existing-private-root> --bundle <frozen-bundle> [--listen 127.0.0.1:8766] [--cpu-test-supervisor] [--docker-scripted [--docker-executable <path>] [--docker-socket <path>] [--ollama-endpoint <http-url>]]".into()),
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
    let daemon = Arc::new(
        Daemon::open_with_docker(
            &root,
            &bundle,
            supervisor,
            cpu_test,
            docker.then_some((docker_executable.as_path(), docker_socket.as_path())),
            ollama_endpoint.as_deref(),
        )
        .await?,
    );
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
