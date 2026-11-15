use clap::Parser;
use playtime_chat::{
    Result,
    artifacts::{self, ArtifactCommand},
    console::markdown::clean,
};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    about = "Retain and inspect private session artifacts offline; no model or tools",
    version
)]
struct Args {
    #[arg(long)]
    session: PathBuf,
    /// Selected project root; required only for retain. File paths are relative to it.
    #[arg(long)]
    workspace: Option<PathBuf>,
    #[arg(long)]
    json: bool,
    #[command(subcommand)]
    command: ArtifactCommand,
}
fn run(args: Args) -> Result<()> {
    let result = artifacts::execute(args.command, &args.session, args.workspace.as_deref(), None)?;
    if args.json {
        println!("{}", serde_json::to_string(&result)?);
    } else {
        println!("{}", clean(&artifacts::display(&result)));
    }
    Ok(())
}
fn main() {
    if let Err(error) = run(Args::parse()) {
        eprintln!("playtime-artifacts: {}", clean(&error.to_string()));
        std::process::exit(1);
    }
}
