#[tokio::main]
async fn main() -> std::process::ExitCode {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() != 2 || args[0] != "--bootstrap" {
        eprintln!("usage: playtime-worker --bootstrap <private-file>");
        return std::process::ExitCode::FAILURE;
    }
    match playtime_worker::execution::serve(std::path::Path::new(&args[1])).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            std::process::ExitCode::FAILURE
        }
    }
}
