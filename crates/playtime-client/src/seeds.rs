//! Local seed files, read once for upload. The service receives their bytes and
//! never a client path.
use playtime_protocol::api::{SEED_ENCODED_BYTES, SEED_FILE_BYTES, SEED_PATH_BYTES, SeedUpload};
use rustix::fs::{Mode, OFlags};
use sha2::{Digest, Sha256};
use std::{fs::File, io::Read, path::Path};

/// Reads one regular UTF-8 file as an upload named after the file. Links,
/// directories, special files and files too large or not text are refused. The
/// bytes are read once, so later edits to the original never reach a draft.
pub fn read_seed_file(path: &Path) -> Result<SeedUpload, String> {
    let shown = path.display();
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| name.len() <= SEED_PATH_BYTES && !name.chars().any(char::is_control))
        .ok_or_else(|| {
            format!("{shown}: the file name must be plain text of at most {SEED_PATH_BYTES} bytes")
        })?;
    // Opening a FIFO without O_NONBLOCK would wait for a writer.
    let fd = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| {
        if error == rustix::io::Errno::LOOP {
            format!("{shown} is a symbolic link")
        } else {
            format!("{shown}: {error}")
        }
    })?;
    let file = File::from(fd);
    let metadata = file.metadata().map_err(|e| format!("{shown}: {e}"))?;
    if !metadata.file_type().is_file() {
        return Err(format!("{shown} isn't a regular file"));
    }
    let mut bytes = Vec::new();
    file.take(SEED_FILE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("{shown}: {e}"))?;
    if bytes.len() > SEED_FILE_BYTES {
        return Err(format!("{shown} has more than {SEED_FILE_BYTES} bytes"));
    }
    let content = String::from_utf8(bytes).map_err(|_| format!("{shown} isn't UTF-8 text"))?;
    if serde_json::to_string(&content).map_or(true, |encoded| encoded.len() > SEED_ENCODED_BYTES) {
        return Err(format!(
            "{shown} has more than {SEED_ENCODED_BYTES} bytes once JSON escapes it"
        ));
    }
    Ok(SeedUpload {
        path: name.to_owned(),
        size_bytes: content.len() as u64,
        sha256: Sha256::digest(content.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
        content,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn reads_regular_text_files_and_refuses_everything_else() {
        let root = std::env::temp_dir().join(format!("playtime-seeds-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let text = root.join("notes.txt");
        std::fs::write(&text, "palette: blue\n").unwrap();
        let upload = read_seed_file(&text).unwrap();
        assert_eq!(upload.path, "notes.txt");
        assert_eq!(upload.content, "palette: blue\n");
        assert_eq!(upload.size_bytes, 14);
        assert_eq!(
            upload.sha256,
            "134abf6b3cb99513a87508b0e510eb59a688f4e473345445b4106ece815e0954"
        );
        std::fs::write(root.join("big.txt"), "x".repeat(SEED_FILE_BYTES + 1)).unwrap();
        std::fs::write(root.join("binary.bin"), [0xff, 0xfe, 0x00]).unwrap();
        std::fs::write(root.join("escaped.txt"), "\u{1}".repeat(40_000)).unwrap();
        symlink(&text, root.join("link.txt")).unwrap();
        rustix::fs::mknodat(
            rustix::fs::CWD,
            root.join("pipe"),
            rustix::fs::FileType::Fifo,
            Mode::RUSR | Mode::WUSR,
            0,
        )
        .unwrap();
        for (name, expected) in [
            ("big.txt", "more than 131072 bytes"),
            ("binary.bin", "isn't UTF-8 text"),
            ("escaped.txt", "once JSON escapes it"),
            ("link.txt", "is a symbolic link"),
            ("pipe", "isn't a regular file"),
            ("missing.txt", "No such file"),
        ] {
            let error = read_seed_file(&root.join(name)).unwrap_err();
            assert!(error.contains(expected), "{name}: {error}");
        }
        assert!(
            read_seed_file(&root)
                .unwrap_err()
                .contains("isn't a regular file")
        );
        std::fs::remove_dir_all(&root).unwrap();
    }
}
