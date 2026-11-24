//! Local session discovery for the console. Selection is a preview, not a lease:
//! the normal launch must reopen and validate the chosen session before use.
use crate::Result;
use playtime_core::{RunId, Sha256Digest};
use playtime_engine::chat::ConversationConfig;
use playtime_harness::Session;
use playtime_local::PrivateDirectory;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{self, BufRead, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Ready,
    InUse,
    Interrupted,
    SettingsDiffer,
    Unavailable,
}

#[derive(Debug, Serialize)]
pub struct Entry {
    pub path: PathBuf,
    pub modified_unix_seconds: u64,
    pub preview: String,
    pub state: State,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Head {
    schema_version: u32,
    configuration: Sha256Digest,
    checkpoint: Option<RunId>,
    active_turn: Option<RunId>,
}

fn preview(text: &str) -> String {
    text.chars()
        .take(100)
        .flat_map(|c| {
            if c.is_control() {
                c.escape_default().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect()
}

/// The configuration a session saved with `saved` model binding resumes under, or
/// `None` for an unrelated model. A lock that pins its Strata server release still
/// requires everything the same lock without it did, so sessions saved under that
/// `earlier` fingerprint keep their binding.
pub fn binding(
    expected: &ConversationConfig,
    earlier: Option<&Sha256Digest>,
    saved: &Sha256Digest,
) -> Option<ConversationConfig> {
    if *saved == expected.model {
        return Some(expected.clone());
    }
    (earlier == Some(saved)).then(|| ConversationConfig {
        model: saved.clone(),
        ..expected.clone()
    })
}

/// The configuration to open an existing session directory with. Anything this
/// can't read falls back to `expected`, and the harness then refuses it as usual.
pub fn resume_config(
    expected: &ConversationConfig,
    earlier: Option<&Sha256Digest>,
    session: &Path,
) -> ConversationConfig {
    PrivateDirectory::open(session, false)
        .ok()
        .and_then(|directory| directory.read("config.json", 1024 * 1024).ok())
        .and_then(|bytes| serde_json::from_slice::<ConversationConfig>(&bytes).ok())
        .and_then(|saved| binding(expected, earlier, &saved.model))
        .unwrap_or_else(|| expected.clone())
}

fn inspect(
    path: PathBuf,
    modified: u64,
    expected: &ConversationConfig,
    earlier: Option<&Sha256Digest>,
) -> Result<Option<Entry>> {
    let directory = PrivateDirectory::open(&path, false)?;
    let config: ConversationConfig =
        serde_json::from_slice(&directory.read("config.json", 1024 * 1024)?)?;
    let Some(expected) = binding(expected, earlier, &config.model) else {
        return Ok(None);
    };
    if config.workspace != expected.workspace {
        return Ok(None);
    }
    let head: Head = serde_json::from_slice(&directory.read("head.json", 4096)?)?;
    if head.schema_version != 1 || head.configuration != config.fingerprint()? {
        return Err("invalid saved session metadata".into());
    }
    // An opened console can leave diagnostics without any conversation to resume.
    if head.checkpoint.is_none()
        && head.active_turn.is_none()
        && !directory.names()?.iter().any(|n| n == "interruption.json")
    {
        return Ok(None);
    }
    // Read only bounded display metadata, never the event transcript. The
    // harness remains authoritative for checkpoint validation and fencing.
    let input = head
        .active_turn
        .as_ref()
        .or(head.checkpoint.as_ref())
        .and_then(|turn| {
            directory
                .read(&format!("input-{turn}.json"), 1024 * 1024)
                .ok()
        })
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|value| value.get("input")?.as_str().map(preview))
        .or_else(|| {
            let bytes = directory.read("interruption.json", 64 * 1024).ok()?;
            let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
            value["report"]["pending_input"]["text"]
                .as_str()
                .map(preview)
        })
        .unwrap_or_else(|| "(no saved message)".into());
    let state = match Session::try_open(&path, &expected) {
        Ok(_) => State::Ready,
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => State::InUse,
        Err(_) if head.active_turn.is_some() => State::Interrupted,
        Err(_) if config != expected => State::SettingsDiffer,
        Err(_) => State::Unavailable,
    };
    Ok(Some(Entry {
        path,
        modified_unix_seconds: modified,
        preview: input,
        state,
    }))
}

/// The latest 20 nonempty matching sessions among the 200 most recently touched session
/// directories. Unsafe files and unrelated models/workspaces are not offered.
pub fn catalog(
    root: &Path,
    expected: &ConversationConfig,
    earlier: Option<&Sha256Digest>,
) -> Result<Vec<Entry>> {
    let directory = PrivateDirectory::open(root, false)?;
    let mut candidates = Vec::new();
    for name in directory.names()? {
        let path = root.join(name);
        let metadata = fs::symlink_metadata(&path)?;
        if !metadata.is_dir() {
            continue;
        }
        let modified = fs::symlink_metadata(path.join("head.json"))
            .and_then(|m| m.modified())
            .unwrap_or(UNIX_EPOCH)
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        candidates.push((modified, path));
    }
    candidates.sort_by(|a, b| b.cmp(a));
    Ok(candidates
        .into_iter()
        .take(200)
        .filter_map(|(modified, path)| inspect(path, modified, expected, earlier).ok().flatten())
        .take(20)
        .collect())
}

#[derive(Debug, Serialize)]
pub struct Choice {
    pub session: Option<PathBuf>,
    pub cancelled: bool,
    pub recover_interrupted: Option<PathBuf>,
    pub acknowledge_uncertain_tools: bool,
}

pub fn select(
    entries: &[Entry],
    config: &ConversationConfig,
    earlier: Option<&Sha256Digest>,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> Result<Choice> {
    writeln!(output, "\n  0  New conversation")?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    for (i, entry) in entries.iter().enumerate() {
        let age = now.saturating_sub(entry.modified_unix_seconds);
        let age = if age < 3600 {
            format!("{}m ago", age / 60)
        } else if age < 86400 {
            format!("{}h ago", age / 3600)
        } else {
            format!("{}d ago", age / 86400)
        };
        let status = match entry.state {
            State::Ready => "",
            State::InUse => " [in use]",
            State::Interrupted => " [interrupted; r<number> to inspect/recover]",
            State::SettingsDiffer => " [settings differ]",
            State::Unavailable => " [cannot resume]",
        };
        writeln!(output, " {:>2}  {age:>8}  {}{status}", i + 1, entry.preview)?;
    }
    writeln!(output, "  q  Cancel")?;
    loop {
        write!(output, "Session [0]: ")?;
        output.flush()?;
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 || line.trim().eq_ignore_ascii_case("q") {
            return Ok(Choice {
                session: None,
                cancelled: true,
                recover_interrupted: None,
                acknowledge_uncertain_tools: false,
            });
        }
        if line.trim().is_empty() || line.trim() == "0" {
            return Ok(Choice {
                session: None,
                cancelled: false,
                recover_interrupted: None,
                acknowledge_uncertain_tools: false,
            });
        }
        if let Some(index) = line
            .trim()
            .strip_prefix('r')
            .and_then(|s| s.parse::<usize>().ok())
            && let Some(entry) = index.checked_sub(1).and_then(|i| entries.get(i))
            && entry.state == State::Interrupted
        {
            match Session::inspect_interrupted(
                &entry.path,
                &resume_config(config, earlier, &entry.path),
            ) {
                Ok(report) => {
                    // Escape terminal controls in archived user/model/tool text.
                    let text = report.context()?;
                    for c in text.chars() {
                        if c.is_control() && c != '\n' {
                            write!(output, "{}", c.escape_default())?;
                        } else {
                            write!(output, "{c}")?;
                        }
                    }
                    writeln!(
                        output,
                        "\nRecovery creates a new session and replays nothing. {} tool outcomes are unknown; files may have partial changes.",
                        report.uncertain_tools
                    )?;
                    write!(
                        output,
                        "Type recover to accept this report and create the fork; Enter cancels: "
                    )?;
                    output.flush()?;
                    let mut confirm = String::new();
                    if input.read_line(&mut confirm)? != 0 && confirm.trim() == "recover" {
                        return Ok(Choice {
                            session: None,
                            cancelled: false,
                            recover_interrupted: Some(entry.path.clone()),
                            acknowledge_uncertain_tools: report.uncertain_tools > 0,
                        });
                    }
                }
                Err(error) => writeln!(output, "Recovery unavailable: {error}")?,
            }
            continue;
        }
        if let Ok(index) = line.trim().parse::<usize>()
            && let Some(entry) = index.checked_sub(1).and_then(|i| entries.get(i))
            && entry.state == State::Ready
        {
            return Ok(Choice {
                session: Some(entry.path.clone()),
                cancelled: false,
                recover_interrupted: None,
                acknowledge_uncertain_tools: false,
            });
        }
        writeln!(
            output,
            "Choose a resumable session, 0 for new, or q to cancel."
        )?;
    }
}
