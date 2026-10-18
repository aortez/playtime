mod args;
mod observe;
mod output;
mod run;
use args::Cli;
use clap::Parser;
use output::{Failure, Output};
use std::sync::{Arc, Mutex};
use tokio::signal::unix::{SignalKind, signal};
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() {
    let argv: Vec<_> = std::env::args_os().collect();
    let format = Cli::format_hint(&argv);
    let output = Output(format);
    let args = match Cli::try_parse_from(argv) {
        Ok(args) => args,
        Err(error)
            if matches!(
                error.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            ) =>
        {
            let code = if error.print().is_ok() { 0 } else { 7 };
            std::process::exit(code);
        }
        Err(error) => {
            let failure = Failure::new(2, "invalid_options", error.to_string());
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(1),
                output.diagnostic("error", failure.data),
            )
            .await;
            std::process::exit(2);
        }
    };
    let recovery = Arc::new(Mutex::new(None));
    let cancel = CancellationToken::new();
    let result = match (
        signal(SignalKind::interrupt()),
        signal(SignalKind::terminate()),
    ) {
        (Ok(mut interrupt), Ok(mut terminate)) => tokio::select! {
            _ = interrupt.recv() => { cancel.cancel(); Err(Failure::new(130, "signal", "SIGINT: client detached; the run was not cancelled")) },
            _ = terminate.recv() => { cancel.cancel(); Err(Failure::new(143, "signal", "SIGTERM: client detached; the run was not cancelled")) },
            result = run::execute(&args, output, recovery.clone(), cancel.clone()) => result,
        },
        _ => Err(Failure::new(1, "signals", "cannot install signal handlers")),
    };
    let code = match result {
        Ok(code) => code,
        Err(mut failure) => {
            if let Some(context) = recovery.lock().unwrap().clone() {
                failure = failure.context(context);
            }
            let exit = failure.exit;
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(1),
                output.diagnostic("error", failure.data),
            )
            .await;
            exit
        }
    };
    // A blocked downstream pipe must not keep a signalled observer alive via
    // Tokio's blocking stdout task. All successful output frames were flushed
    // before acknowledgment; process exit never sends cancellation to a service.
    std::process::exit(code);
}
