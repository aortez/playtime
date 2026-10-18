use clap::{Args, Parser, Subcommand, ValueEnum};
use playtime_core::{RunId, RunMode, SessionId};
use playtime_protocol::api::{ArtifactId, IdempotencyKey, PreviewRevision, RecipeId};
use std::{ffi::OsString, path::PathBuf, time::Duration};

#[derive(Clone, Copy, Debug, Default, PartialEq, ValueEnum)]
pub enum Format {
    #[default]
    Text,
    Json,
    Jsonl,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, ValueEnum)]
#[value(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Experiment,
    ScriptedSmoke,
    LiveSmoke,
    Simulation,
}
impl From<Mode> for RunMode {
    fn from(v: Mode) -> Self {
        match v {
            Mode::Experiment => Self::Experiment,
            Mode::ScriptedSmoke => Self::ScriptedSmoke,
            Mode::LiveSmoke => Self::LiveSmoke,
            Mode::Simulation => Self::Simulation,
        }
    }
}
macro_rules! id_parser {
    ($function:ident, $ty:ty) => {
        fn $function(value: &str) -> Result<$ty, String> {
            <$ty>::new(value).map_err(|_| "expected an opaque service ID, not a path".into())
        }
    };
}
id_parser!(run_id, RunId);
id_parser!(session_id, SessionId);
id_parser!(recipe_id, RecipeId);
id_parser!(artifact_id, ArtifactId);
id_parser!(key, IdempotencyKey);
id_parser!(revision, PreviewRevision);

#[derive(Debug, Parser)]
#[command(
    name = "playtime",
    version,
    about = "Native Playtime service client (Rust development)"
)]
pub struct Cli {
    #[arg(long, global = true, conflicts_with_all = ["endpoint", "token_file"])]
    pub connection_file: Option<PathBuf>,
    #[arg(long, global = true, requires = "token_file")]
    pub endpoint: Option<String>,
    #[arg(long, global = true, requires = "endpoint")]
    pub token_file: Option<PathBuf>,
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,
    #[arg(long, global = true)]
    pub state_dir: Option<PathBuf>,
    #[arg(long, global = true, value_enum, default_value = "text")]
    pub format: Format,
    #[arg(long, global = true, default_value = "30m", value_parser = duration)]
    pub timeout: Duration,
    #[command(subcommand)]
    pub command: Command,
}
#[derive(Debug, Subcommand)]
pub enum Command {
    #[command(subcommand)]
    Service(Service),
    #[command(subcommand)]
    Recipes(Recipes),
    Run(Launch),
    #[command(subcommand)]
    Launches(Launches),
    #[command(subcommand)]
    Runs(Runs),
    Events(Events),
    #[command(subcommand)]
    Checkpoints(Checkpoints),
    #[command(subcommand)]
    Artifacts(Artifacts),
    #[command(external_subcommand)]
    Reserved(Vec<OsString>),
}
#[derive(Debug, Subcommand)]
pub enum Service {
    Status,
}
#[derive(Debug, Subcommand)]
pub enum Recipes {
    List(Page),
    Preview {
        #[arg(value_parser = recipe_id)]
        recipe_id: RecipeId,
        #[arg(long, value_enum, default_value = "experiment")]
        mode: Mode,
    },
}
#[derive(Debug, Args, Default)]
pub struct Page {
    #[arg(long)]
    pub cursor: Option<String>,
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..=1000))]
    pub limit: Option<u32>,
}
impl Page {
    pub fn query(&self) -> playtime_protocol::api::PageQuery {
        playtime_protocol::api::PageQuery {
            cursor: self.cursor.clone(),
            limit: self.limit,
        }
    }
}
#[derive(Debug, Args, Default)]
pub struct Observe {
    #[arg(long, conflicts_with = "wait")]
    pub follow: bool,
    #[arg(long)]
    pub wait: bool,
}
#[derive(Debug, Args)]
pub struct Launch {
    #[arg(value_parser = recipe_id)]
    pub recipe_id: RecipeId,
    #[arg(long, value_enum, default_value = "experiment")]
    pub mode: Mode,
    #[arg(long, value_parser = revision)]
    pub preview_revision: Option<PreviewRevision>,
    #[arg(long, value_parser = key)]
    pub idempotency_key: Option<IdempotencyKey>,
    #[command(flatten)]
    pub observe: Observe,
}
#[derive(Debug, Subcommand)]
pub enum Launches {
    List(Page),
    Show {
        #[arg(value_parser = key)]
        key: IdempotencyKey,
    },
    Retry {
        #[arg(value_parser = key)]
        key: IdempotencyKey,
        #[command(flatten)]
        observe: Observe,
    },
}
#[derive(Debug, Subcommand)]
pub enum Runs {
    List(Page),
    Show {
        #[arg(value_parser = run_id)]
        run_id: RunId,
    },
    Wait {
        #[arg(value_parser = run_id)]
        run_id: RunId,
    },
    Cancel {
        #[arg(value_parser = run_id)]
        run_id: RunId,
        #[arg(long)]
        wait: bool,
    },
}
#[derive(Debug, Args)]
pub struct Events {
    #[arg(value_parser = run_id)]
    pub run_id: RunId,
    #[arg(long, conflicts_with = "cursor_file")]
    pub cursor: Option<String>,
    #[arg(long)]
    pub cursor_file: Option<PathBuf>,
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..=1000))]
    pub limit: Option<u32>,
    #[arg(long, conflicts_with = "follow")]
    pub all: bool,
    #[arg(long)]
    pub follow: bool,
}
#[derive(Debug, Subcommand)]
pub enum Checkpoints {
    List {
        #[arg(value_parser = run_id)]
        run_id: RunId,
        #[command(flatten)]
        page: Page,
    },
    Show {
        #[arg(value_parser = run_id)]
        run_id: RunId,
        #[arg(value_parser = session_id)]
        session_id: SessionId,
    },
}
#[derive(Debug, Subcommand)]
pub enum Artifacts {
    List {
        #[arg(value_parser = run_id)]
        run_id: RunId,
        #[command(flatten)]
        page: Page,
    },
    Show {
        #[arg(value_parser = run_id)]
        run_id: RunId,
        #[arg(value_parser = artifact_id)]
        artifact_id: ArtifactId,
    },
    Download {
        #[arg(value_parser = run_id)]
        run_id: RunId,
        #[arg(value_parser = artifact_id)]
        artifact_id: ArtifactId,
        #[arg(long)]
        output: PathBuf,
        #[arg(long)]
        force: bool,
    },
}
pub fn duration(text: &str) -> Result<Duration, String> {
    let (number, unit) = text.split_at(
        text.find(|c: char| !c.is_ascii_digit())
            .unwrap_or(text.len()),
    );
    let factor = match unit {
        "ms" => 1,
        "s" => 1000,
        "m" => 60_000,
        "h" => 3_600_000,
        _ => return Err("duration requires ms, s, m, or h (for example 60s)".into()),
    };
    let milliseconds = number
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(factor))
        .filter(|n| *n > 0)
        .ok_or("duration must be positive and fit in a 64-bit millisecond count")?;
    Ok(Duration::from_millis(milliseconds))
}
impl Cli {
    pub fn validate(&self) -> Result<(), &'static str> {
        let streaming = match &self.command {
            Command::Run(v) => v.observe.follow,
            Command::Launches(Launches::Retry { observe, .. }) => observe.follow,
            Command::Events(v) => v.follow || v.all,
            _ => false,
        };
        if streaming && self.format == Format::Json {
            return Err("--format json cannot stream --follow or --all; use jsonl or text");
        }
        if let Command::Events(v) = &self.command
            && v.cursor
                .as_ref()
                .is_some_and(|c| playtime_protocol::api::EventCursor::decode(c).is_err())
        {
            return Err("invalid event cursor");
        }
        Ok(())
    }
    pub fn format_hint(args: &[OsString]) -> Format {
        let mut result = Format::Text;
        for (i, arg) in args.iter().enumerate() {
            let value = if arg == "--format" {
                args.get(i + 1).and_then(|s| s.to_str())
            } else {
                arg.to_str().and_then(|s| s.strip_prefix("--format="))
            };
            if let Some(value) = value
                && let Ok(format) = Format::from_str(value, false)
            {
                result = format;
            }
        }
        result
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn grammar_and_offline_validation() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
        for args in [
            vec!["playtime", "run", "recipe", "--follow", "--format", "json"],
            vec!["playtime", "events", "run", "--all", "--format", "json"],
        ] {
            assert!(Cli::try_parse_from(args).unwrap().validate().is_err());
        }
        assert!(
            Cli::try_parse_from(["playtime", "runs", "wait", "run", "--timeout", "0s"]).is_err()
        );
        assert!(
            Cli::try_parse_from([
                "playtime",
                "--endpoint",
                "http://localhost:1",
                "service",
                "status"
            ])
            .is_err()
        );
        assert!(Cli::try_parse_from(["playtime", "run", "recipe", "--follow", "--wait"]).is_err());
        assert!(Cli::try_parse_from(["playtime", "recipes", "list", "--limit", "1001"]).is_err());
    }
}
