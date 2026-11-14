use super::*;
use playtime_local::PrivateDirectory;
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{self, Read},
    os::unix::{ffi::OsStringExt, fs::MetadataExt},
    path::{Path, PathBuf},
};

/// Identity of a local directory, not merely the pathname used to select it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectRoot {
    path: PathBuf,
    device: u64,
    inode: u64,
    uid: u32,
}
impl ProjectRoot {
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub(crate) fn verify(&self) -> Result<()> {
        let metadata = fs::symlink_metadata(&self.path).map_err(|_| changed())?;
        if !metadata.is_dir()
            || metadata.dev() != self.device
            || metadata.ino() != self.inode
            || metadata.uid() != self.uid
            || metadata.mode() & 0o002 != 0
            || fs::canonicalize(&self.path).map_err(|_| changed())? != self.path
        {
            return Err(changed());
        }
        Ok(())
    }
    pub(crate) fn verify_probe(&self, value: &Value) -> Result<()> {
        if value
            != &json!({"device":self.device,"inode":self.inode,"uid":1000,"gid":1000,"writable":true})
        {
            return Err(changed());
        }
        Ok(())
    }
}

/// Cooperative exclusive editing lease, held outside the model's writable tree.
/// Host-side editors must not replace directories or add external hard links
/// while it is held. It does not fence unrelated programs running as this user.
pub struct ProjectDirectory {
    root: ProjectRoot,
    state: PrivateDirectory,
    _lease: File,
}
impl ProjectDirectory {
    /// Blocking filesystem admission. `private_state` must already exist outside
    /// the project; use the same state root for all consoles. Call off the async
    /// executor. Additional protected paths include session/config directories.
    pub fn open(project: &Path, private_state: &Path, protected: &[PathBuf]) -> io::Result<Self> {
        let path = fs::canonicalize(project)?;
        let private_state = fs::canonicalize(private_state)?;
        let executable = fs::canonicalize(std::env::current_exe()?)?;
        let text = path.to_str().ok_or_else(unsafe_project)?;
        if text.len() > 4096
            || text.chars().any(|c| c.is_control() || c == ',' || c == '"')
            || private_state.starts_with(&path)
            || executable.starts_with(&path)
        {
            return Err(unsafe_project());
        }
        for protected in protected {
            if fs::canonicalize(protected)?.starts_with(&path) {
                return Err(unsafe_project());
            }
        }
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.uid() != rustix::process::geteuid().as_raw() {
            return Err(unsafe_project());
        }
        let root = ProjectRoot {
            path,
            device: metadata.dev(),
            inode: metadata.ino(),
            uid: metadata.uid(),
        };
        root.verify().map_err(io::Error::other)?;
        // Device/inode aliases contend on the same lock, including a renamed
        // directory. Canonical path spelling alone would permit duplicate leases.
        let leases = private_state.join(format!("project-{}-{}", root.device, root.inode));
        let state = PrivateDirectory::open(&leases, true)?;
        let _lease = state.lock()?;
        if state.names()?.iter().any(|name| name == "active") {
            return Err(io::Error::other(
                "project has an unresolved prior container; reconcile its private active proposal before editing",
            ));
        }
        no_child_mounts(&root.path)?;
        scan(&root)?;
        no_child_mounts(&root.path)?;
        root.verify().map_err(io::Error::other)?;
        Ok(Self {
            root,
            state,
            _lease,
        })
    }
    pub fn root(&self) -> &ProjectRoot {
        &self.root
    }
    pub(crate) fn reserve(&self, proposal: &ProjectProposal) -> Result<()> {
        self.state
            .write_new("active", &serde_json::to_vec(proposal).expect("proposal"))
            .map_err(|_| {
                Error::new(
                    Failure::Journal,
                    "project_reserve",
                    "cannot reserve project or another turn is unresolved",
                )
            })
    }
    pub(crate) fn release(&self, proposal: &ProjectProposal) -> Result<()> {
        let bytes = self.state.read("active", 65536).map_err(|_| {
            Error::new(
                Failure::Journal,
                "project_release",
                "cannot read project reservation",
            )
        })?;
        if bytes != serde_json::to_vec(proposal).expect("proposal") {
            return Err(Error::new(
                Failure::Conflict,
                "project_release",
                "reservation belongs to another turn",
            ));
        }
        self.state.remove("active").map_err(|_| {
            Error::new(
                Failure::Journal,
                "project_release",
                "cannot release project reservation",
            )
        })
    }
}

fn unsafe_project() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "project must be an owned directory outside private state, with no special files or external hard links",
    )
}
fn changed() -> Error {
    Error::new(
        Failure::ChangedIdentity,
        "project_identity",
        "selected project directory changed or mounted identity differs",
    )
}

fn scan(root: &ProjectRoot) -> io::Result<()> {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut directories = vec![root.path.clone()];
    let mut links: BTreeMap<(u64, u64), (u64, u64)> = BTreeMap::new();
    let mut count = 0;
    while let Some(directory) = directories.pop() {
        for entry in fs::read_dir(directory)? {
            count += 1;
            if count > 500_000 || std::time::Instant::now() >= deadline {
                return Err(io::Error::other(
                    "project admission scan exceeded 500000 entries or 10 seconds",
                ));
            }
            let entry = entry?;
            let metadata = fs::symlink_metadata(entry.path())?;
            if metadata.dev() != root.device {
                return Err(unsafe_project());
            }
            if metadata.is_dir() {
                directories.push(entry.path());
            } else if metadata.is_file() || metadata.is_symlink() {
                let value = links
                    .entry((metadata.dev(), metadata.ino()))
                    .or_insert((metadata.nlink(), 0));
                if value.0 != metadata.nlink() {
                    return Err(unsafe_project());
                }
                value.1 += 1;
            } else {
                // In particular, never expose a host Unix socket to commands.
                return Err(unsafe_project());
            }
        }
    }
    if links.values().any(|(links, present)| links != present) {
        return Err(unsafe_project());
    }
    Ok(())
}

fn no_child_mounts(root: &Path) -> io::Result<()> {
    let mut bytes = Vec::new();
    File::open("/proc/self/mountinfo")?
        .take(2 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 2 * 1024 * 1024 {
        return Err(unsafe_project());
    }
    validate_mounts(root, &bytes)
}
fn validate_mounts(root: &Path, bytes: &[u8]) -> io::Result<()> {
    // A same-device bind mount can hide files from the admission scan while
    // Docker's nonrecursive bind would expose the covered directory. Reject all
    // child mountpoints, not just entries whose st_dev differs from the root.
    for line in bytes.split(|b| *b == b'\n').filter(|line| !line.is_empty()) {
        let field = line
            .split(|b| *b == b' ')
            .nth(4)
            .ok_or_else(unsafe_project)?;
        let mut path = Vec::new();
        let mut index = 0;
        while index < field.len() {
            let byte = field[index];
            index += 1;
            if byte == b'\\' {
                let digits = field.get(index..index + 3).ok_or_else(unsafe_project)?;
                if digits.iter().any(|b| !(b'0'..=b'7').contains(b)) {
                    return Err(unsafe_project());
                }
                let value = u16::from(digits[0] - b'0') * 64
                    + u16::from(digits[1] - b'0') * 8
                    + u16::from(digits[2] - b'0');
                path.push(u8::try_from(value).map_err(|_| unsafe_project())?);
                index += 3;
            } else {
                path.push(byte);
            }
        }
        let path = PathBuf::from(std::ffi::OsString::from_vec(path));
        if !path.is_absolute() || (path != root && path.starts_with(root)) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "project contains a nested mount or mount table is malformed",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn child_mounts_are_rejected_even_with_the_same_device_and_escaped_names() {
        let root = Path::new("/project with space");
        assert!(
            validate_mounts(
                root,
                b"36 35 8:0 / /project\\040with\\040space rw - ext4 disk rw\n"
            )
            .is_ok()
        );
        assert!(
            validate_mounts(
                root,
                b"37 36 8:0 /elsewhere /project\\040with\\040space/covered rw - ext4 disk rw\n"
            )
            .is_err()
        );
        assert!(
            validate_mounts(
                root,
                b"38 36 8:0 / /project\\040with\\040space-other rw - ext4 disk rw\n"
            )
            .is_ok()
        );
        assert!(validate_mounts(root, b"truncated\n").is_err());
        assert!(validate_mounts(root, b"38 36 8:0 / /bad\\999 rw - ext4 disk rw\n").is_err());
    }
}
