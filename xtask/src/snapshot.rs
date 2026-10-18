use crate::Result;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fs, path::Path, process::Command};

#[derive(Serialize, Deserialize)]
#[serde(tag = "encoding", rename_all = "snake_case")]
enum Contents {
    Utf8 { text: String },
    Bytes { bytes: Vec<u8> },
}
impl Contents {
    fn bytes(&self) -> &[u8] {
        match self {
            Self::Utf8 { text } => text.as_bytes(),
            Self::Bytes { bytes } => bytes,
        }
    }
}
#[derive(Serialize, Deserialize)]
pub struct Snapshot {
    schema_version: u32,
    capture_method: String,
    pub revision: Option<String>,
    pub dirty: bool,
    files: BTreeMap<String, Contents>,
}
impl Snapshot {
    pub fn bytes(&self, path: &str) -> Result<Vec<u8>> {
        Ok(self
            .files
            .get(path)
            .ok_or("missing snapshot input")?
            .bytes()
            .to_vec())
    }
    pub fn materialize(&self, root: &Path) -> Result<()> {
        for (name, contents) in &self.files {
            let path = root.join(name);
            fs::create_dir_all(path.parent().unwrap())?;
            fs::write(path, contents.bytes())?;
        }
        Ok(())
    }
}
pub fn capture(root: &Path) -> Result<Snapshot> {
    let revision = crate::output(
        Command::new("git")
            .current_dir(root)
            .args(["rev-parse", "HEAD"]),
    )
    .ok();
    let mut files = BTreeMap::new();
    for name in [
        "Cargo.toml",
        "Cargo.lock",
        "rust-toolchain.toml",
        "crates",
        "xtask",
    ] {
        collect(root, &root.join(name), &mut files)?;
    }
    // Capture project build configuration, never Cargo credentials/cache files.
    for name in [".cargo/config", ".cargo/config.toml"] {
        match fs::symlink_metadata(root.join(name)) {
            Ok(_) => collect(root, &root.join(name), &mut files)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
    }
    validate_paths(&files)?;
    // Compare captured bytes to the selected revision, not a later live `status`.
    let mut dirty = revision.is_none();
    if let Some(revision) = &revision {
        for (path, contents) in &files {
            let output = Command::new("git")
                .current_dir(root)
                .args(["show", &format!("{revision}:{path}")])
                .output()?;
            if !output.status.success() || output.stdout != contents.bytes() {
                dirty = true;
                break;
            }
        }
        // A removed input must also make the captured source dirty.
        let paths = crate::output(Command::new("git").current_dir(root).args([
            "ls-tree",
            "-r",
            "--name-only",
            revision,
            "--",
            "Cargo.toml",
            "Cargo.lock",
            "rust-toolchain.toml",
            "crates",
            "xtask",
            ".cargo",
        ]))?;
        dirty |= paths.lines().any(|name| !files.contains_key(name));
    }
    Ok(Snapshot {
        schema_version: 1,
        capture_method: "staged_first_party_snapshot_before_compilation".into(),
        revision,
        dirty,
        files,
    })
}

// Refuse a path dependency outside the captured tree. An absolute dependency,
// or one escaping the staged workspace, could otherwise compile today's live
// checkout despite a correctly recorded snapshot digest.
fn validate_paths(files: &BTreeMap<String, Contents>) -> Result<()> {
    for (name, contents) in files {
        if name == "Cargo.toml" || name.ends_with("/Cargo.toml") {
            let value: toml::Value = toml::from_str(std::str::from_utf8(contents.bytes())?)?;
            validate_table(&value, Path::new(name).parent().unwrap(), files)?;
        }
    }
    Ok(())
}
fn validate_table(
    value: &toml::Value,
    base: &Path,
    files: &BTreeMap<String, Contents>,
) -> Result<()> {
    if let Some(table) = value.as_table() {
        if let Some(path) = table.get("path").and_then(toml::Value::as_str) {
            let mut resolved = base.to_path_buf();
            for part in Path::new(path).components() {
                match part {
                    std::path::Component::Normal(name) => resolved.push(name),
                    std::path::Component::CurDir => (),
                    std::path::Component::ParentDir if resolved.pop() => (),
                    _ => return Err("manifest path escapes captured workspace".into()),
                }
            }
            let target = resolved.to_str().ok_or("non-UTF8 dependency path")?;
            let manifest = resolved.join("Cargo.toml");
            if !files.contains_key(target) && !files.contains_key(manifest.to_str().unwrap()) {
                return Err(format!("manifest path is not captured: {target}").into());
            }
        }
        for value in table.values() {
            validate_table(value, base, files)?;
        }
    } else if let Some(array) = value.as_array() {
        for value in array {
            validate_table(value, base, files)?;
        }
    }
    Ok(())
}

fn collect(root: &Path, path: &Path, files: &mut BTreeMap<String, Contents>) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.is_dir() {
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            if ["target", ".git", ".playtime", "runs"]
                .iter()
                .any(|name| entry.file_name() == *name)
            {
                continue;
            }
            collect(root, &entry.path(), files)?;
        }
    } else if metadata.is_file() {
        let relative = path
            .strip_prefix(root)?
            .to_str()
            .ok_or("non-UTF8 source name")?;
        let bytes = playtime_worker::PrivateDirectory::open(root)?.read(
            relative,
            playtime_storage::StorageLimits::default().content_bytes,
        )?;
        if bytes.len() as u64 > playtime_storage::StorageLimits::default().content_bytes {
            return Err("source input exceeds archive content limit".into());
        }
        let contents = match String::from_utf8(bytes) {
            Ok(text) => Contents::Utf8 { text },
            Err(error) => Contents::Bytes {
                bytes: error.into_bytes(),
            },
        };
        files.insert(
            path.strip_prefix(root)?
                .to_str()
                .ok_or("non-UTF8 source name")?
                .into(),
            contents,
        );
        let limits = playtime_storage::StorageLimits::default();
        if files.len() as u64 > limits.references
            || files.values().map(|c| c.bytes().len() as u64).sum::<u64>() > limits.content_bytes
        {
            return Err("first-party snapshot exceeds archive bounds".into());
        }
    } else {
        return Err(format!(
            "source snapshot refuses links/special files: {}",
            path.display()
        )
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn capture_checks_dependency_containment_and_uses_captured_bytes() {
        let mut files = BTreeMap::new();
        files.insert(
            "Cargo.toml".into(),
            Contents::Utf8 {
                text: "[workspace.dependencies]\na = { path = 'crates/a' }".into(),
            },
        );
        files.insert(
            "crates/a/Cargo.toml".into(),
            Contents::Utf8 {
                text: "[package]\nname = 'a'".into(),
            },
        );
        validate_paths(&files).unwrap();
        for path in ["/tmp/live-checkout", "../../live-checkout", "missing"] {
            files.insert(
                "Cargo.toml".into(),
                Contents::Utf8 {
                    text: format!("[workspace.dependencies]\na = {{ path = '{path}' }}"),
                },
            );
            assert!(validate_paths(&files).is_err());
        }
    }

    #[test]
    fn staged_bytes_survive_checkout_edits_and_exclude_credentials() {
        let root = std::env::temp_dir().join(format!(
            "pt-snapshot-{}",
            playtime_worker_control::random_id().unwrap().as_str()
        ));
        fs::create_dir(&root).unwrap();
        for dir in ["crates", "xtask", ".cargo", ".playtime", "runs"] {
            fs::create_dir(root.join(dir)).unwrap();
        }
        for (name, bytes) in [
            ("Cargo.toml", "[workspace]\nmembers=[]"),
            ("Cargo.lock", "lock"),
            ("rust-toolchain.toml", "[toolchain]\nchannel='1.94.1'"),
            ("crates/input.bin", "original"),
            (".cargo/config.toml", "[build]\njobs=2"),
            (".cargo/credentials.toml", "never-capture-this-credential"),
            (".playtime/token", "never-capture-this-credential"),
            ("runs/output", "never-capture-this-credential"),
        ] {
            fs::write(root.join(name), bytes).unwrap();
        }
        let snapshot = capture(&root).unwrap();
        assert!(snapshot.dirty);
        assert!(
            !String::from_utf8(serde_json::to_vec(&snapshot).unwrap())
                .unwrap()
                .contains("never-capture")
        );
        fs::write(root.join("crates/input.bin"), b"changed after capture").unwrap();
        let staged = root.join("staged");
        snapshot.materialize(&staged).unwrap();
        assert_eq!(
            fs::read(staged.join("crates/input.bin")).unwrap(),
            b"original"
        );
        std::os::unix::fs::symlink(
            root.join(".cargo/credentials.toml"),
            root.join("crates/link"),
        )
        .unwrap();
        assert!(capture(&root).is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
