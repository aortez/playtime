use playtime_chat::Result;
use playtime_local::PrivateDirectory;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{fs::OpenOptions, io::Read, os::unix::fs::OpenOptionsExt, path::Path};

pub fn prompt(path: &Path) -> Result<String> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err("prompt file must be regular".into());
    }
    let mut bytes = Vec::new();
    file.take(playtime_engine::chat::MAX_USER_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > playtime_engine::chat::MAX_USER_BYTES {
        return Err("prompt file exceeds the 256 KiB turn limit".into());
    }
    let text = String::from_utf8(bytes)?;
    if text.trim().is_empty() {
        return Err("prompt file is empty".into());
    }
    Ok(text)
}

pub fn completion(
    path: &Path,
    launch_id: &str,
    prompt: &str,
    outcome: &str,
    local_tools: &str,
    session: &Path,
) -> Result<()> {
    let hash = Sha256::digest(prompt.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let directory = PrivateDirectory::open(path.parent().ok_or("receipt needs a parent")?, false)?;
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or("invalid receipt name")?;
    directory.write_new(
        name,
        &serde_json::to_vec(&json!({
            "schema_version":1,"launch_id":launch_id,"prompt_sha256":hash,
            "outcome":outcome,"local_tools":local_tools,"session_id":session.display().to_string()
        }))?,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn completion_binds_the_prompt_and_cannot_replace_another_receipt() {
        let root = std::env::temp_dir().join(format!(
            "playtime-completion-{}",
            playtime_local::Token::generate().unwrap().expose()
        ));
        PrivateDirectory::open(&root, true).unwrap();
        let path = root.join("receipt.json");
        completion(
            &path,
            "launch-example",
            "hello\n",
            "cancelled",
            "confirmed",
            Path::new("/private/session"),
        )
        .unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let receipt: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            receipt,
            json!({
                "schema_version":1,"launch_id":"launch-example",
                "prompt_sha256":"5891b5b522d5df086d0ff0b110fbd9d21bb4fc7163af34d08286a2e846f6be03",
                "outcome":"cancelled","local_tools":"confirmed",
                "session_id":"/private/session"
            })
        );
        assert!(completion(&path, "other", "changed", "failed", "unresolved", &root).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn prompt_file_preserves_large_utf8_and_rejects_bad_sources() {
        let root = std::env::temp_dir().join(format!(
            "playtime-prompt-{}",
            playtime_local::Token::generate().unwrap().expose()
        ));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("prompt");
        let text = format!("{}\n\r\ntrailing  \n", "λ".repeat(70_000));
        std::fs::write(&path, &text).unwrap();
        assert_eq!(prompt(&path).unwrap(), text);
        std::fs::write(&path, vec![b'x'; playtime_engine::chat::MAX_USER_BYTES + 1]).unwrap();
        assert!(prompt(&path).is_err());
        std::fs::write(&path, [0xff]).unwrap();
        assert!(prompt(&path).is_err());
        assert!(prompt(&root).is_err());
        let link = root.join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(prompt(&link).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
