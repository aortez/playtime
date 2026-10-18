//! Shared local credential/discovery filesystem policy for server and clients.
//! All opens are descriptor-relative and refuse links, devices and FIFOs.
mod state;
pub use state::{PrivateDirectory, lock_private, read_private, replace_private, resolve_path};

use playtime_protocol::api::Connection;
use rustix::fs::{AtFlags, Mode, OFlags, mkdirat, openat, renameat, unlinkat};
use std::{
    fs::File,
    io::{self, Read, Write},
    os::unix::fs::MetadataExt,
    path::{Component, Path, PathBuf},
};
use subtle::ConstantTimeEq;

fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "invalid or unsafe local credential/discovery file",
    )
}

#[derive(Clone)]
pub struct Token([u8; 32]);
impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token([REDACTED])")
    }
}
impl Token {
    pub fn generate() -> io::Result<Self> {
        let mut bytes = [0; 32];
        getrandom::fill(&mut bytes).map_err(|_| io::Error::other("OS randomness unavailable"))?;
        Ok(Self(bytes))
    }
    pub fn parse(value: &str) -> io::Result<Self> {
        if value.len() != 64
            || !value
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        {
            return Err(invalid());
        }
        let mut bytes = [0; 32];
        for (i, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&value[i * 2..i * 2 + 2], 16).map_err(|_| invalid())?;
        }
        Ok(Self(bytes))
    }
    /// Intended only for a sensitive Authorization header or secure file write.
    pub fn expose(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }
    pub fn matches(&self, candidate: &str) -> bool {
        // Constant-time comparison for the fixed-width credential, including
        // malformed hex of the same width. Never compare token string prefixes.
        self.expose().as_bytes().ct_eq(candidate.as_bytes()).into()
    }
}

struct Directory(File);
impl Directory {
    fn open(path: &Path, create: bool) -> io::Result<Self> {
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()?.join(path)
        };
        let mut directory = Self(File::open("/")?);
        let components: Vec<_> = path.components().collect();
        for (index, component) in components.iter().enumerate() {
            match component {
                Component::RootDir => (),
                Component::Normal(name) => {
                    directory = directory.child(name, create)?;
                    let metadata = directory.0.metadata()?;
                    let uid = rustix::process::geteuid().as_raw();
                    let is_final = index + 1 == components.len();
                    if (metadata.uid() != uid && (is_final || metadata.uid() != 0))
                        || (metadata.mode() & 0o022 != 0
                            && !(metadata.uid() == 0 && metadata.mode() & 0o1000 != 0 && !is_final))
                    {
                        return Err(invalid());
                    }
                }
                _ => return Err(invalid()),
            }
        }
        Ok(directory)
    }
    fn child(&self, name: &std::ffi::OsStr, create: bool) -> io::Result<Self> {
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        match openat(&self.0, name, flags, Mode::empty()) {
            Ok(fd) => Ok(Self(File::from(fd))),
            Err(rustix::io::Errno::NOENT) if create => {
                match mkdirat(&self.0, name, Mode::from_raw_mode(0o700)) {
                    Ok(()) | Err(rustix::io::Errno::EXIST) => (),
                    Err(error) => return Err(error.into()),
                }
                self.0.sync_all()?;
                Ok(Self(File::from(openat(
                    &self.0,
                    name,
                    flags,
                    Mode::empty(),
                )?)))
            }
            Err(error) => Err(error.into()),
        }
    }
    fn read(&self, name: &std::ffi::OsStr, limit: u64) -> io::Result<Vec<u8>> {
        let fd = openat(
            &self.0,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        let file = File::from(fd);
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || metadata.uid() != rustix::process::geteuid().as_raw()
            || metadata.mode() & 0o777 != 0o600
            || metadata.nlink() != 1
            || metadata.len() > limit
        {
            return Err(invalid());
        }
        let mut bytes = Vec::new();
        file.take(limit + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > limit {
            return Err(invalid());
        }
        Ok(bytes)
    }
    fn create(&self, name: &str, bytes: &[u8]) -> io::Result<()> {
        let fd = openat(
            &self.0,
            name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )?;
        let mut file = File::from(fd);
        file.write_all(bytes)?;
        file.sync_all()?;
        self.0.sync_all()
    }
}

pub struct Credentials {
    directory: Directory,
    pub token: Token,
    pub token_path: PathBuf,
    pub connection_path: PathBuf,
}
impl Credentials {
    pub fn initialize(root: &Path) -> io::Result<Self> {
        Directory::open(root, true)?;
        let directory_path = root.join(".playtime");
        let directory = Directory::open(&directory_path, true)?;
        let token = match directory.read("api-token".as_ref(), 65) {
            Ok(bytes) => token_bytes(&bytes)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let token = Token::generate()?;
                match directory.create("api-token", format!("{}\n", token.expose()).as_bytes()) {
                    Ok(()) => token,
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                        token_bytes(&directory.read("api-token".as_ref(), 65)?)?
                    }
                    Err(error) => return Err(error),
                }
            }
            Err(error) => return Err(error),
        };
        let absolute = if directory_path.is_absolute() {
            directory_path
        } else {
            std::env::current_dir()?.join(directory_path)
        };
        Ok(Self {
            directory,
            token,
            token_path: absolute.join("api-token"),
            connection_path: absolute.join("connection.json"),
        })
    }
    pub fn publish(&self, connection: &Connection) -> io::Result<()> {
        if connection.schema_version != 1
            || connection.token_file != self.token_path.to_string_lossy()
        {
            return Err(invalid());
        }
        match self.directory.read("connection.json".as_ref(), 16384) {
            Ok(_) => (),
            Err(error) if error.kind() == io::ErrorKind::NotFound => (),
            Err(error) => return Err(error),
        }
        let bytes = serde_json::to_vec(connection).map_err(|_| invalid())?;
        if bytes.len() > 16384 {
            return Err(invalid());
        }
        let temporary = format!(".connection-{}.tmp", Token::generate()?.expose());
        self.directory.create(&temporary, &bytes)?;
        let result = renameat(
            &self.directory.0,
            &temporary,
            &self.directory.0,
            "connection.json",
        );
        if result.is_err() {
            let _ = unlinkat(&self.directory.0, &temporary, AtFlags::empty());
        }
        result?;
        self.directory.0.sync_all()
    }
}
fn token_bytes(bytes: &[u8]) -> io::Result<Token> {
    let text = std::str::from_utf8(bytes).map_err(|_| invalid())?;
    Token::parse(text.strip_suffix('\n').unwrap_or(text))
}
pub fn read_connection(path: &Path) -> io::Result<(Connection, Token)> {
    let directory = Directory::open(path.parent().ok_or_else(invalid)?, false)?;
    let bytes = directory.read(path.file_name().ok_or_else(invalid)?, 16384)?;
    let connection: Connection = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
    if connection.schema_version != 1 {
        return Err(invalid());
    }
    let token_path = resolve_path(
        path.parent().ok_or_else(invalid)?,
        Path::new(&connection.token_file),
    );
    let token_dir = Directory::open(token_path.parent().ok_or_else(invalid)?, false)?;
    let token = token_bytes(&token_dir.read(token_path.file_name().ok_or_else(invalid)?, 65)?)?;
    Ok((connection, token))
}

pub fn read_token(path: &Path) -> io::Result<Token> {
    token_bytes(&read_private(path, 65)?)
}
