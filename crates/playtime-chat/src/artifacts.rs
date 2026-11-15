//! User-driven artifact operations shared by the console and offline CLI.
//! All file reads, image decoding and viewer launches run outside rendering.
use crate::Result;
use clap::{Parser, Subcommand};
use image::{ImageFormat, ImageReader, Limits};
use playtime_harness::{
    Session,
    artifacts::{self, Artifacts, MAX_FILE_BYTES, MAX_SOURCE_BYTES, RetainedFile},
};
use playtime_local::PrivateDirectory;
use rustix::fs::{Mode, OFlags, open, openat};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    fs::{DirBuilder, File},
    io::{Cursor, Read},
    os::unix::fs::{DirBuilderExt, MetadataExt},
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
};

#[derive(Debug, Subcommand, Serialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum ArtifactCommand {
    /// List the session's explicitly retained artifacts.
    List,
    /// Show metadata and verify the retained bytes.
    Info { id: String },
    /// Copy a project file into the session, associated with its last completed turn.
    Retain {
        file: PathBuf,
        #[arg(long)]
        caption: String,
        /// Optional UTF-8 diagram source, retained alongside its rendered PNG/JPEG.
        #[arg(long)]
        source: Option<PathBuf>,
    },
    /// Export verified bytes and metadata into a new private directory.
    Export {
        id: String,
        #[arg(long)]
        output: PathBuf,
    },
    /// Explicitly open a retained PNG/JPEG using the desktop's image viewer.
    Open { id: String },
}
#[derive(Parser)]
#[command(name = "/artifact", disable_version_flag = true)]
struct ConsoleCommand {
    #[command(subcommand)]
    command: ArtifactCommand,
}
pub fn parse_action(text: &str) -> Result<ArtifactCommand> {
    if text.trim() == "/artifacts" {
        return Ok(ArtifactCommand::List);
    }
    let words = shlex::split(text).ok_or("Unclosed quote in artifact command")?;
    Ok(ConsoleCommand::try_parse_from(words)?.command)
}

/// Walk relative components through directory descriptors. No symlink, FIFO,
/// absolute filename or parent traversal may escape the selected project.
fn read_project(workspace: &Path, relative: &Path, source: bool) -> Result<RetainedFile> {
    let components: Vec<_> = relative.components().collect();
    if components.is_empty()
        || components
            .iter()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(
            "artifact file must be a relative project path without parent traversal".into(),
        );
    }
    let root = workspace.canonicalize()?;
    let dir_flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mut directory = open("/", dir_flags, Mode::empty())?;
    for component in root.components().skip(1) {
        directory = openat(&directory, component.as_os_str(), dir_flags, Mode::empty())?;
    }
    for component in &components[..components.len() - 1] {
        directory = openat(&directory, component.as_os_str(), dir_flags, Mode::empty())?;
    }
    let mut file = File::from(openat(
        &directory,
        components.last().unwrap().as_os_str(),
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
    let before = file.metadata()?;
    let limit = if source {
        MAX_SOURCE_BYTES
    } else {
        MAX_FILE_BYTES
    };
    if !before.is_file() || before.len() == 0 || before.len() > limit {
        return Err("artifact must be a nonempty regular file within the size limit".into());
    }
    let mut bytes = Vec::new();
    (&mut file).take(limit + 1).read_to_end(&mut bytes)?;
    let after = file.metadata()?;
    if bytes.len() as u64 != before.len()
        || before.len() != after.len()
        || before.mtime() != after.mtime()
        || before.mtime_nsec() != after.mtime_nsec()
        || before.ctime() != after.ctime()
        || before.ctime_nsec() != after.ctime_nsec()
    {
        return Err(
            "artifact changed while it was being copied; try after the build finishes".into(),
        );
    }
    let media_type = if source {
        std::str::from_utf8(&bytes)?;
        "text/plain"
    } else {
        match image::guess_format(&bytes) {
            Ok(format @ (ImageFormat::Png | ImageFormat::Jpeg)) => {
                validate_image(&bytes, format)?;
                if format == ImageFormat::Png {
                    "image/png"
                } else {
                    "image/jpeg"
                }
            }
            _ => "application/octet-stream",
        }
    };
    Ok(RetainedFile {
        name: relative
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or("artifact filename must be UTF-8")?
            .into(),
        media_type: media_type.into(),
        bytes,
    })
}
fn validate_image(bytes: &[u8], format: ImageFormat) -> Result<()> {
    let mut limits = Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(64 * 1024 * 1024);
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    reader.limits(limits);
    reader.decode()?;
    Ok(())
}

pub fn execute(
    command: ArtifactCommand,
    session_path: &Path,
    workspace: Option<&Path>,
    held: Option<&mut Session>,
) -> Result<Value> {
    execute_with_opener(command, session_path, workspace, held, &|path| {
        let mut child = Command::new("/usr/bin/xdg-open")
            .arg(path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        // Reap the desktop helper without blocking the console on viewer lifetime.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(())
    })
}

fn execute_with_opener(
    command: ArtifactCommand,
    session_path: &Path,
    workspace: Option<&Path>,
    held: Option<&mut Session>,
    opener: &dyn Fn(&Path) -> Result<()>,
) -> Result<Value> {
    let store = Artifacts::open(session_path)?;
    let result = match command {
        ArtifactCommand::List => json!({"catalog": store.list()?}),
        ArtifactCommand::Info { id } => {
            let record = store.info(&id)?;
            store.read(&id, false)?;
            if record.source.is_some() {
                store.read(&id, true)?;
            }
            json!({"artifact": record, "verified": true})
        }
        ArtifactCommand::Retain {
            file,
            caption,
            source,
        } => {
            let workspace = workspace.ok_or("retain requires an explicit --workspace")?;
            if session_path
                .canonicalize()?
                .starts_with(workspace.canonicalize()?)
            {
                return Err("session state must be outside the project tool root".into());
            }
            let file = read_project(workspace, &file, false)?;
            let source = source
                .map(|path| read_project(workspace, &path, true))
                .transpose()?;
            let mut owned;
            let session = if let Some(held) = held {
                held
            } else {
                owned = artifacts::open_for_retention(session_path)?;
                &mut owned
            };
            json!({"artifact": session.retain_artifact(caption, file, source)?})
        }
        ArtifactCommand::Export { id, output } => {
            let record = store.info(&id)?;
            let bytes = store.read(&id, false)?;
            let source = record
                .source
                .as_ref()
                .map(|_| store.read(&id, true))
                .transpose()?;
            // Never overwrite an existing destination, including a symlink. A
            // failed export may leave a partial directory; it is never retried.
            DirBuilder::new().mode(0o700).create(&output)?;
            let directory = PrivateDirectory::open(&output, false)?;
            let output = output.canonicalize()?;
            directory.write_new(&artifacts::blob_name(&record, false)?, &bytes)?;
            if let Some(source) = source {
                directory.write_new(&artifacts::blob_name(&record, true)?, &source)?;
            }
            directory.write_new("artifact.json", &serde_json::to_vec_pretty(&record)?)?;
            json!({"artifact": record, "exported_to": output})
        }
        ArtifactCommand::Open { id } => {
            let record = store.info(&id)?;
            let format = match record.file.media_type.as_str() {
                "image/png" => ImageFormat::Png,
                "image/jpeg" => ImageFormat::Jpeg,
                _ => return Err(
                    "only retained PNG/JPEG images can be opened; export other files explicitly"
                        .into(),
                ),
            };
            validate_image(&store.read(&id, false)?, format)?;
            let path = session_path
                .canonicalize()?
                .join(artifacts::blob_name(&record, false)?);
            opener(&path)?;
            json!({"artifact": record, "viewer_launch_requested": true})
        }
    };
    Ok(json!({"artifact_api_version": 1, "result": result}))
}

pub fn display(value: &Value) -> String {
    let result = &value["result"];
    if let Some(records) = result["catalog"]["artifacts"].as_array() {
        if records.is_empty() {
            return "No retained artifacts. Use /artifact retain PATH --caption \"Description\" after a completed turn.".into();
        }
        let mut text = String::from("Retained artifacts (private session copies):\n");
        for record in records {
            text.push_str(&format!(
                "{} · {} · {} bytes\n  {}\n",
                record["id"].as_str().unwrap_or("?"),
                record["kind"].as_str().unwrap_or("?"),
                record["file"]["size"],
                record["caption"].as_str().unwrap_or("?")
            ));
        }
        text.push_str(
            "/artifact info ID · /artifact open ID · /artifact export ID --output NEW_DIRECTORY",
        );
        text
    } else {
        let record = &result["artifact"];
        let mut text = format!(
            "Artifact {} · {}\n{}\n{} · {} bytes · {}\nSHA-256: {}\nSource turn: {}",
            record["id"].as_str().unwrap_or("?"),
            record["kind"].as_str().unwrap_or("?"),
            record["caption"].as_str().unwrap_or("?"),
            record["file"]["original_name"].as_str().unwrap_or("?"),
            record["file"]["size"],
            record["file"]["media_type"].as_str().unwrap_or("?"),
            record["file"]["sha256"].as_str().unwrap_or("?"),
            record["source_turn"].as_str().unwrap_or("?")
        );
        if let Some(name) = record["source"]["original_name"].as_str() {
            text.push_str(&format!("\nRetained diagram source: {name}"));
        }
        if result["viewer_launch_requested"] == true {
            text.push_str("\nViewer launch requested for the retained session copy.");
        } else if let Some(path) = result["exported_to"].as_str() {
            text.push_str(&format!(
                "\nExported verified files and artifact.json to {path}"
            ));
        } else if result["verified"] == true {
            text.push_str("\nRetained files verified.");
        } else {
            text.push_str("\nRetained in this session. This file has not been sent to the model.");
        }
        text
    }
}
