use playtime_backend_host::{
    host::{self, Config, OwnerPaths},
    systemd::Manager,
};
#[tokio::main]
async fn main() {
    if run().await.is_err() {
        eprintln!("backend host closed: verification/enforcement unavailable");
        std::process::exit(1);
    }
}
async fn run() -> playtime_backend_host::Result<()> {
    use playtime_backend_host::wire::Failure;
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() != 1 {
        return Err(Failure::Invalid);
    }
    let config = Config::load()?;
    match args[0].to_str() {
        Some("prepare-generation") => {
            if rustix::process::geteuid().as_raw() != 0 {
                return Err(Failure::Unauthorized);
            }
            let remote = host::Remote {
                socket: "/run/playtime-backend-owner/owner.sock".into(),
                credential: config.credential,
                owner_uid: 0,
            };
            remote.prepare_generation().await?;
            Ok(())
        }
        Some("guard") => host::serve_guard(config, false).await,
        Some("guard-initialize") => host::serve_guard(config, true).await,
        Some("owner") | Some("owner-initialize") => {
            if rustix::process::geteuid().as_raw() != 0 {
                return Err(Failure::Unauthorized);
            }
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
            let manager = loop {
                host::notify(b"WATCHDOG=1")?;
                if let Ok(manager) = Manager::capture(
                    "ollama.service",
                    config.backend_id.clone(),
                    config.guard_id.clone(),
                    config.generation,
                    config.profile_sha256.clone(),
                    false,
                )
                .await
                {
                    break manager;
                }
                if tokio::time::Instant::now() >= deadline {
                    return Err(Failure::Expired);
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            };
            host::serve_owner(
                manager,
                config,
                OwnerPaths {
                    state: "/var/lib/playtime-backend-owner".into(),
                    socket: "/run/playtime-backend-owner/owner.sock".into(),
                },
                args[0] == "owner-initialize",
            )
            .await
        }
        _ => Err(Failure::Invalid),
    }
}
