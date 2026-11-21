//! Project environment metadata is prepared outside the tool workspace. The
//! frozen image and inventory become part of the conversation configuration.
use crate::Result;
use playtime_local::PrivateDirectory;
use serde_json::Value;
use std::path::Path;

pub const LEGACY_INVENTORY: &str = "The selected project is mounted writable at /work in a Linux Docker workshop. Commands run as uid 1000 with no external network. The image provides Bash, Git, Python 3.12, Node/npm, GCC/G++, Make, rg and SQLite. Rust/Cargo and Go are not installed. Use only available tools; do not attempt package downloads. Files persist across turns; background processes and /tmp do not. Session records, model control, host credentials and Docker control are outside the workspace. There is no aggregate disk quota on project files.";

pub fn inventory(image: &Value, workspace: &Path) -> Result<String> {
    let Some(environment) = image.get("project_environment") else {
        // Preserve the exact original prompt so existing sessions stay compatible.
        return Ok(LEGACY_INVENTORY.into());
    };
    if environment["schema_version"] != 1 || environment["workspace"].as_str() != workspace.to_str()
    {
        return Err("prepared environment belongs to another workspace or schema".into());
    }
    let inventory = environment["inventory"]
        .as_str()
        .filter(|s| !s.trim().is_empty() && s.len() <= 16384)
        .ok_or("invalid prepared environment inventory")?;
    Ok(format!(
        "The selected project is mounted writable at /work in its prepared development container. Commands run as uid 1000 with no external network. Project files and project-local build caches persist across turns; processes and /tmp are reset after each turn. Session records, model control, host credentials and Docker control are outside the workspace. There is no aggregate project disk quota. Use the installed tools and prepared dependencies; missing dependencies require a separate environment preparation. {inventory}"
    ))
}

/// Publish after session creation, while its lease is held. Refuse conflicting
/// metadata rather than changing the environment of an existing conversation.
pub fn save(session: &Path, image: &[u8]) -> Result<()> {
    if image.len() > 65536 {
        return Err("image lock exceeds 64 KiB".into());
    }
    let directory = PrivateDirectory::open(session, false)?;
    match directory.read("environment.lock.json", 65536) {
        Ok(saved) if saved == image => Ok(()),
        Ok(_) => Err("saved session environment differs".into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            directory.write_new("environment.lock.json", image)?;
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}
