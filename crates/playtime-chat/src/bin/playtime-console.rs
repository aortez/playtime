//! Offline driver for the production composer and renderer. It has no model or
//! tool adapters. Replay is presentation only and cannot resume saved effects.
use clap::Parser;
use playtime_chat::{
    Result,
    console::{Action, Console, Observation, Presentation, markdown::clean},
};
use playtime_local::PrivateDirectory;
use serde_json::json;
use std::{
    collections::VecDeque,
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
    time::Duration,
};
use tokio_util::sync::CancellationToken;

#[derive(Parser)]
#[command(
    about = "Drive Playtime's console offline; no inference or tool execution",
    version
)]
struct Args {
    /// Replay presentation observations from JSONL (at most 4 MiB).
    #[arg(long)]
    replay: Option<PathBuf>,
    /// Render the fixture/replay to plain output and exit.
    #[arg(long)]
    plain: bool,
    /// Emit versioned presentation JSON rather than rendered text.
    #[arg(long, requires = "plain")]
    json: bool,
    /// Write explicit UI actions to a new mode-0600 file (for PTY automation).
    #[arg(long)]
    actions: Option<PathBuf>,
    /// Delay between fixture chunks, in milliseconds.
    #[arg(long, default_value_t = 35, value_parser = clap::value_parser!(u64).range(1..=1000))]
    tick_ms: u64,
    /// Start playing immediately, while leaving the composer editable.
    #[arg(long)]
    autoplay: bool,
    /// Print one raw private session record; never opens a runtime or replays it.
    #[arg(long, requires = "record", conflicts_with_all = ["replay", "actions", "autoplay"])]
    session: Option<PathBuf>,
    /// Exact events-*.jsonl or result-*.json basename within --session.
    #[arg(long, requires = "session")]
    record: Option<String>,
    /// Browse retained artifacts from a saved session, without starting a model.
    #[arg(long, conflicts_with_all = ["session", "replay", "autoplay"])]
    artifact_session: Option<PathBuf>,
    /// Enable explicit artifact retention from this project in the saved browser.
    #[arg(long, requires = "artifact_session")]
    workspace: Option<PathBuf>,
}
fn observations(args: &Args) -> Result<Vec<Observation>> {
    let text = if let Some(path) = &args.replay {
        let mut text = String::new();
        File::open(path)?
            .take(4 * 1024 * 1024 + 1)
            .read_to_string(&mut text)?;
        if text.len() > 4 * 1024 * 1024 {
            return Err("replay exceeds 4 MiB".into());
        }
        text
    } else {
        include_str!("../../tests/fixtures/console-v1.jsonl").into()
    };
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).map_err(Into::into))
        .collect()
}
fn chunks(events: &[Observation]) -> VecDeque<Observation> {
    events
        .iter()
        .flat_map(|event| match event {
            Observation::Delta { text, thinking } => text
                .chars()
                .collect::<Vec<_>>()
                .chunks(16)
                .map(|chars| Observation::Delta {
                    text: chars.iter().collect(),
                    thinking: *thinking,
                })
                .collect(),
            event => vec![event.clone()],
        })
        .collect()
}
#[tokio::main]
async fn main() {
    if let Err(error) = run(Args::parse()).await {
        eprintln!("playtime-console: {}", clean(&error.to_string()));
        std::process::exit(1);
    }
}
async fn run(args: Args) -> Result<()> {
    if let (Some(session), Some(record)) = (&args.session, &args.record) {
        if !((record.starts_with("events-") && record.ends_with(".jsonl"))
            || (record.starts_with("result-") && record.ends_with(".json")))
        {
            return Err("expected an events-*.jsonl or result-*.json record basename".into());
        }
        let bytes = PrivateDirectory::open(session, false)?.read(record, 8 * 1024 * 1024)?;
        println!("{}", clean(std::str::from_utf8(&bytes)?));
        return Ok(());
    }
    let events = observations(&args)?;
    if let Some(path) = &args.artifact_session
        && (args.plain || !Console::supported())
    {
        let value = playtime_chat::artifacts::execute(
            playtime_chat::artifacts::ArtifactCommand::List,
            path,
            None,
            None,
        )?;
        if args.json {
            println!("{}", serde_json::to_string(&value)?);
        } else {
            println!("{}", clean(&playtime_chat::artifacts::display(&value)));
        }
        return Ok(());
    }
    if args.plain || !Console::supported() {
        let mut view = Presentation::default();
        for event in events {
            if args.json {
                println!(
                    "{}",
                    json!({"console_observation_version":1,"observation":event})
                );
            } else {
                for line in view.observe(event) {
                    println!("{line}");
                }
            }
        }
        for line in view.finish() {
            println!("{line}");
        }
        return Ok(());
    }
    let mut log = args
        .actions
        .as_ref()
        .map(|path| {
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path)
        })
        .transpose()?;
    let banner = if let Some(path) = &args.artifact_session {
        format!(
            "Playtime · SAVED ARTIFACTS\nSession: {}\n/artifacts lists retained files; /artifact open ID opens a retained image. /help for commands; /quit exits.",
            path.display()
        )
    } else {
        "Playtime console · OFFLINE FIXTURE\nNo model or project tools. Type a prompt to run a scripted stream; /quit exits.".into()
    };
    let mut console = if args.artifact_session.is_some() {
        Console::start_artifact_browser(banner)?
    } else {
        Console::start(banner)?
    };
    if let Some(path) = &args.artifact_session {
        let path = path.clone();
        let value = tokio::task::spawn_blocking(move || {
            playtime_chat::artifacts::execute(
                playtime_chat::artifacts::ArtifactCommand::List,
                &path,
                None,
                None,
            )
        })
        .await??;
        console.notice(playtime_chat::artifacts::display(&value))?;
    }
    let mut pending = if args.autoplay {
        chunks(&events)
    } else {
        VecDeque::new()
    };
    let mut cancel = CancellationToken::new();
    if !pending.is_empty() {
        console.set_active(Some(cancel.clone()))?;
    }
    let mut timer = tokio::time::interval(Duration::from_millis(args.tick_ms));
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut hangup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
    loop {
        tokio::select! {
            action=console.actions.recv()=>{
                let action=action.unwrap_or(Action::Quit);
                if let Some(log)=&mut log { writeln!(log,"{}",serde_json::to_string(&action)?)?;log.flush()?; }
                match action {
                    Action::Submit(_)=>{
                        if args.artifact_session.is_some() { console.notice("Saved artifact browser: use /artifacts or /artifact commands. Model submission is unavailable.")?;console.set_active(None)?; }
                        else {cancel=CancellationToken::new();console.set_active(Some(cancel.clone()))?;pending=chunks(&events);}
                    },
                    Action::Artifact(command)=>{
                        if let Some(path)=&args.artifact_session {
                            let path=path.clone();
                            let workspace=args.workspace.clone();
                            let result=tokio::task::spawn_blocking(move || playtime_chat::artifacts::execute(command,&path,workspace.as_deref(),None)).await?;
                            console.notice(match result { Ok(value)=>playtime_chat::artifacts::display(&value),Err(error)=>format!("Artifact: {error}") })?;
                        } else { console.notice("Use --artifact-session PATH to browse saved artifacts, or playtime-artifacts --help.")?; }
                    },
                    Action::Stop=>{cancel.cancel();pending.clear();console.notice("Fixture stopped; no effects were executed.")?;console.set_active(None)?;},
                    Action::Quit=>break,
                }
            }
            _=timer.tick(),if !pending.is_empty()=>{
                if cancel.is_cancelled() { pending.clear();console.set_active(None)?; }
                else if let Some(event)=pending.pop_front() { console.observe(event)?;if pending.is_empty(){console.set_active(None)?;} }
            }
            _=tokio::signal::ctrl_c()=>{pending.clear();console.notice("Fixture interrupted")?;console.set_active(None)?;},
            _=terminate.recv()=>break,
            _=hangup.recv()=>break,
        }
    }
    console.finish()
}
