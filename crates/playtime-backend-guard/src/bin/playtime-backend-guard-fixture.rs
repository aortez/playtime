//! Explicitly invoked CPU fixture executable; never a production guard daemon.
use playtime_backend_guard::{
    Fault, Guard, Journal, PublicationObserver, fixture, ports::SystemClock, transport::Server,
    wire::*,
};
use serde::{Deserialize, Serialize};
use std::{
    io::{Read, Write},
    path::PathBuf,
    sync::Arc,
};
use tokio_util::sync::CancellationToken;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    identity: BackendIdentity,
    credential: Capability,
    ipc: PathBuf,
    state: PathBuf,
    backend: Option<fixture::Endpoint>,
    create: bool,
    mode: fixture::Mode,
    barrier: Option<Barrier>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Barrier {
    socket: PathBuf,
    event: String,
    point: String,
    fail: bool,
}
impl PublicationObserver for Barrier {
    fn at(&self, event: &Event, point: Fault) -> std::io::Result<()> {
        let event = serde_json::to_value(event)?;
        if event["kind"].as_str() == Some(&self.event) && format!("{point:?}") == self.point {
            let mut connection = std::os::unix::net::UnixStream::connect(&self.socket)?;
            connection.write_all(b"at barrier\n")?;
            let mut reply = [0];
            connection.read_exact(&mut reply)?;
            if self.fail {
                return Err(std::io::Error::other("injected private journal failure"));
            }
        }
        Ok(())
    }
}
fn main() {
    let args: Vec<_> = std::env::args_os().collect();
    if args.len() == 2 && args[1] == "runner" {
        loop {
            std::thread::park();
        }
    }
    let result = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            if args.len() != 3 {
                return Err(Failure::Invalid);
            }
            let config: Config = serde_json::from_slice(
                &playtime_local::read_private(std::path::Path::new(&args[2]), CONTROL_BYTES as u64)
                    .map_err(|_| Failure::Invalid)?,
            )
            .map_err(|_| Failure::Invalid)?;
            if args[1] == "backend" {
                let listener = tokio::net::UnixListener::bind(config.ipc.join("backend.sock"))
                    .map_err(|_| Failure::Invalid)?;
                println!("ready");
                fixture::serve_owned(
                    listener,
                    config.identity,
                    config.credential,
                    std::env::current_exe().map_err(|_| Failure::Invalid)?,
                    config.mode,
                )
                .await
            } else if args[1] == "guard" {
                let clock = Arc::new(SystemClock::new()?);
                let remote = Arc::new(fixture::Remote {
                    endpoint: config.backend.ok_or(Failure::Invalid)?,
                });
                let mut journal = if config.create {
                    Journal::create(&config.state)?
                } else {
                    Journal::open(&config.state)?
                };
                if let Some(barrier) = config.barrier {
                    journal = journal.with_observer(Arc::new(barrier));
                }
                let guard = Guard::start(
                    journal,
                    config.create.then_some(config.identity),
                    remote.clone(),
                    remote,
                    clock.clone(),
                )
                .await?;
                let server = Server::bind(&config.ipc)?;
                println!("ready");
                server
                    .serve(guard, config.credential, clock, CancellationToken::new())
                    .await
            } else {
                Err(Failure::Invalid)
            }
        });
    if let Err(error) = result {
        eprintln!("CPU fixture failed: {error:?}");
        std::process::exit(1);
    }
}
