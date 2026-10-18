use std::path::PathBuf;
#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let mut backend = None;
    let mut cli = None;
    let mut fixture = None;
    while let Some(flag) = args.next() {
        let value = args.next().unwrap_or_default();
        match flag.as_str() {
            "--backend" => backend = Some(value),
            "--cli" => cli = Some(PathBuf::from(value)),
            "--fixture" => fixture = Some(PathBuf::from(value)),
            _ => {
                eprintln!("unknown driver option");
                std::process::exit(2);
            }
        }
    }
    if backend.as_deref() == Some("simulation") {
        eprintln!(
            "Simulation gate unavailable: #20's production playtimed/test-supervisor adapter, #42 archives, and #43 worker-loss recovery are required. No fixture fallback or skip."
        );
        std::process::exit(1);
    }
    if backend.as_deref() != Some("fixture") || cli.is_none() || fixture.is_none() {
        eprintln!("usage: cli-e2e --backend fixture --cli PATH --fixture PATH");
        std::process::exit(2);
    }
    match playtime_test_support::suite::run(cli.as_deref().unwrap(), fixture.as_deref()).await {
        Ok(count) => println!(
            "{count} real CLI/HTTP fixture process cases passed (not production simulation)"
        ),
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}
