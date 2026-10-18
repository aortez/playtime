//! Shared CLI/workbench connection precedence. Call on a blocking I/O task,
//! never from a UI render loop. Environment is captured explicitly for tests.
use crate::{Client, Error, Token};
use playtime_local::{read_connection, read_token, resolve_path};
use playtime_protocol::api::ServiceId;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs::File,
    io::Read,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Default)]
pub struct Options {
    pub connection_file: Option<PathBuf>,
    pub endpoint: Option<String>,
    pub token_file: Option<PathBuf>,
    pub config: Option<PathBuf>,
    pub state_dir: Option<PathBuf>,
}
#[derive(Clone, Debug, Default)]
pub struct Environment(pub BTreeMap<String, OsString>);
impl Environment {
    pub fn capture() -> Self {
        let names = [
            "PLAYTIME_CONNECTION_FILE",
            "PLAYTIME_ENDPOINT",
            "PLAYTIME_TOKEN_FILE",
            "PLAYTIME_CONFIG",
            "PLAYTIME_STATE_DIR",
            "XDG_CONFIG_HOME",
            "XDG_STATE_HOME",
            "HOME",
        ];
        Self(
            names
                .into_iter()
                .filter_map(|n| std::env::var_os(n).map(|v| (n.to_owned(), v)))
                .collect(),
        )
    }
    fn path(&self, name: &str) -> Option<PathBuf> {
        self.0
            .get(name)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    }
    fn string(&self, name: &str) -> Result<Option<String>, ConfigError> {
        self.0
            .get(name)
            .filter(|v| !v.is_empty())
            .map(|v| {
                v.to_str()
                    .map(str::to_owned)
                    .ok_or(ConfigError("non-UTF-8 connection setting"))
            })
            .transpose()
    }
    fn xdg(&self, name: &str, fallback: &str) -> Result<PathBuf, ConfigError> {
        if let Some(path) = self.path(name).filter(|p| p.is_absolute()) {
            return Ok(path);
        }
        let home = self
            .path("HOME")
            .filter(|p| p.is_absolute())
            .ok_or(ConfigError(
                "absolute HOME required when XDG directory is unset or relative",
            ))?;
        Ok(home.join(fallback))
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigError(pub &'static str);
impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for ConfigError {}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    connection_file: Option<PathBuf>,
    endpoint: Option<String>,
    token_file: Option<PathBuf>,
    state_dir: Option<PathBuf>,
}
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Source {
    Discovery {
        path: PathBuf,
    },
    Endpoint {
        endpoint: String,
        token_file: PathBuf,
    },
}
#[derive(Clone, Debug, Serialize)]
pub struct Resolved {
    pub source: Option<Source>,
    pub state_dir: PathBuf,
    pub config_file: Option<PathBuf>,
    /// Only command options constitute an explicit selection allowing endpoint relocation.
    pub explicit_connection: bool,
}
pub struct LoadedConnection {
    pub endpoint: String,
    pub expected_service: Option<ServiceId>,
    token: Token,
}
impl LoadedConnection {
    pub async fn connect(self) -> crate::Result<Client> {
        Client::connect(&self.endpoint, self.token, self.expected_service).await
    }
}
impl Resolved {
    pub fn load(&self) -> Result<LoadedConnection, ConfigError> {
        match self.source.as_ref().ok_or(ConfigError(
            "no connection configured; select --connection-file or --endpoint with --token-file",
        ))? {
            Source::Discovery { path } => {
                let (connection, token) = read_connection(path).map_err(|_| {
                    ConfigError(
                        "connection descriptor or credential is missing, invalid, or unsafe",
                    )
                })?;
                validate_endpoint(&connection.endpoint)?;
                Ok(LoadedConnection {
                    endpoint: normalize_endpoint(&connection.endpoint)?,
                    expected_service: Some(connection.service_id),
                    token,
                })
            }
            Source::Endpoint {
                endpoint,
                token_file,
            } => {
                let token = read_token(token_file)
                    .map_err(|_| ConfigError("credential file is missing, invalid, or unsafe"))?;
                Ok(LoadedConnection {
                    endpoint: normalize_endpoint(endpoint)?,
                    expected_service: None,
                    token,
                })
            }
        }
    }
}
pub fn normalize_endpoint(value: &str) -> Result<String, ConfigError> {
    validate_endpoint(value)?;
    Ok(reqwest::Url::parse(value)
        .unwrap()
        .to_string()
        .trim_end_matches('/')
        .to_owned())
}
pub fn validate_endpoint(value: &str) -> Result<(), ConfigError> {
    let url = reqwest::Url::parse(value).map_err(|_| ConfigError("invalid endpoint"))?;
    if url.scheme() != "http"
        || !url.username().is_empty()
        || url.password().is_some()
        || !matches!(url.path(), "" | "/")
        || url.query().is_some()
        || url.fragment().is_some()
        || url.port_or_known_default() == Some(0)
        || !matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"))
    {
        return Err(ConfigError(
            "endpoint must be loopback HTTP without credentials, path, query, or fragment",
        ));
    }
    Ok(())
}
fn source(
    connection: Option<PathBuf>,
    endpoint: Option<String>,
    token: Option<PathBuf>,
    base: &Path,
) -> Result<Option<Source>, ConfigError> {
    match (connection, endpoint, token) {
        (None, None, None) => Ok(None),
        (Some(path), None, None) if !path.as_os_str().is_empty() => Ok(Some(Source::Discovery {
            path: resolve_path(base, &path),
        })),
        (None, Some(endpoint), Some(token)) if !token.as_os_str().is_empty() => {
            Ok(Some(Source::Endpoint {
                endpoint: normalize_endpoint(&endpoint)?,
                token_file: resolve_path(base, &token),
            }))
        }
        _ => Err(ConfigError(
            "choose one complete connection source: connection-file OR endpoint plus token-file",
        )),
    }
}

/// Resolves local state even when no connection exists (for offline receipts).
/// An explicitly selected config must exist and validate, including when flags
/// override its connection. No field is borrowed across connection sources.
pub fn resolve(options: &Options, env: &Environment, cwd: &Path) -> Result<Resolved, ConfigError> {
    if !cwd.is_absolute() {
        return Err(ConfigError("current directory must be absolute"));
    }
    let explicit = source(
        options.connection_file.clone(),
        options.endpoint.clone(),
        options.token_file.clone(),
        cwd,
    )?;
    let from_env = if explicit.is_none() {
        source(
            env.path("PLAYTIME_CONNECTION_FILE"),
            env.string("PLAYTIME_ENDPOINT")?,
            env.path("PLAYTIME_TOKEN_FILE"),
            cwd,
        )?
    } else {
        None
    };
    let selected_config = options
        .config
        .clone()
        .or_else(|| env.path("PLAYTIME_CONFIG"));
    let config_path = selected_config
        .clone()
        .map(|p| resolve_path(cwd, &p))
        .or_else(|| {
            env.xdg("XDG_CONFIG_HOME", ".config")
                .ok()
                .map(|p| p.join("playtime/client.toml"))
        });
    let mut config_file = None;
    let mut config_source = None;
    let mut config_state = None;
    if let Some(path) = config_path {
        match std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(
                rustix::fs::OFlags::NOFOLLOW.bits() as i32
                    | rustix::fs::OFlags::NONBLOCK.bits() as i32,
            )
            .open(&path)
        {
            Ok(file) => {
                let config = read_config(file)?;
                let parent = path.parent().ok_or(ConfigError("invalid config path"))?;
                config_source = source(
                    config.connection_file,
                    config.endpoint,
                    config.token_file,
                    parent,
                )?;
                if config_source.is_none() {
                    return Err(ConfigError(
                        "client config requires one complete connection source",
                    ));
                }
                config_state = config.state_dir.map(|p| resolve_path(parent, &p));
                config_file = Some(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && selected_config.is_none() => (),
            Err(_) => {
                return Err(ConfigError(
                    "selected client config is missing, unreadable, or unsafe",
                ));
            }
        }
    }
    let explicit_connection = explicit.is_some();
    let mut source = explicit.or(from_env).or(config_source);
    if source.is_none() {
        let path = cwd.join(".playtime/connection.json");
        match std::fs::symlink_metadata(&path) {
            Ok(_) => source = Some(Source::Discovery { path }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(_) => return Err(ConfigError("cannot inspect local connection descriptor")),
        }
    }
    let state_dir = if let Some(path) = options
        .state_dir
        .clone()
        .or_else(|| env.path("PLAYTIME_STATE_DIR"))
    {
        if path.as_os_str().is_empty() {
            return Err(ConfigError("empty state directory"));
        }
        resolve_path(cwd, &path)
    } else if let Some(path) = config_state {
        path
    } else {
        env.xdg("XDG_STATE_HOME", ".local/state")?.join("playtime")
    };
    Ok(Resolved {
        source,
        state_dir,
        config_file,
        explicit_connection,
    })
}
fn read_config(file: File) -> Result<Config, ConfigError> {
    let metadata = file
        .metadata()
        .map_err(|_| ConfigError("cannot inspect client config"))?;
    if !metadata.is_file() || metadata.len() > 65536 {
        return Err(ConfigError(
            "client config must be a regular file of at most 64 KiB",
        ));
    }
    let mut bytes = Vec::new();
    file.take(65537)
        .read_to_end(&mut bytes)
        .map_err(|_| ConfigError("cannot read client config"))?;
    if bytes.len() > 65536 {
        return Err(ConfigError("client config exceeds 64 KiB"));
    }
    toml::from_str(
        std::str::from_utf8(&bytes).map_err(|_| ConfigError("client config must be UTF-8"))?,
    )
    .map_err(|_| ConfigError("invalid client TOML or unknown/duplicate settings"))
}
impl From<ConfigError> for Error {
    fn from(_: ConfigError) -> Self {
        Self::UnsupportedEndpoint
    }
}
