//! TOML configuration, validated before the daemon binds.

use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::time::Duration;

use kuroko_core::{DelegateId, LabelName, Limits, Millis};
use kuroko_linear::oauth::{OauthClient, Secret};
use kuroko_linear::{PromptTemplate, TriggerConfig, WebhookSecret};
use serde::Deserialize;

/// The file read when `--config` is absent.
pub const DEFAULT_PATH: &str = "kuroko.toml";

/// Overrides `[linear] secret` when it is set to a non-empty value.
pub const SECRET_ENV: &str = "KUROKO_LINEAR_SECRET";

/// Overrides `[linear.oauth] client_secret` when it is set to a non-empty value.
pub const CLIENT_SECRET_ENV: &str = "KUROKO_LINEAR_CLIENT_SECRET";

/// Overrides `[linear.oauth] install_secret` when it is set to a non-empty value.
pub const INSTALL_SECRET_ENV: &str = "KUROKO_LINEAR_INSTALL_SECRET";

/// Names a file holding `[linear.oauth] client_secret`, read when the value
/// variable is unset or empty.
pub const CLIENT_SECRET_FILE_ENV: &str = "KUROKO_LINEAR_CLIENT_SECRET_FILE";

/// Names a file holding `[linear.oauth] install_secret`, read when the value
/// variable is unset or empty.
pub const INSTALL_SECRET_FILE_ENV: &str = "KUROKO_LINEAR_INSTALL_SECRET_FILE";

const DEFAULT_BIND: &str = "127.0.0.1:8787";
const DEFAULT_STORE_PATH: &str = "kuroko.db";
const DEFAULT_TEMPLATE: &str = "Issue {identifier}: {title}\n\n{description}\n{url}";
const DEFAULT_WINDOW_SECS: i64 = 60;
const DEFAULT_TIMEOUT_SECS: u64 = 3600;
const DEFAULT_MAX_IN_FLIGHT: usize = 1;
const DEFAULT_MAX_QUEUED: usize = 64;

/// A validated configuration. Every field carries its default by this point.
#[derive(Clone, Debug)]
pub struct Config {
    pub bind: SocketAddr,
    pub secret: WebhookSecret,
    /// Absent when the operator has not registered an OAuth app yet. Webhooks
    /// work without it; only the install routes then answer `404`.
    pub oauth: Option<OauthConfig>,
    pub replay_window: Millis,
    pub triggers: TriggerConfig,
    pub template: PromptTemplate,
    pub limits: Limits,
    pub transport: TransportChoice,
    pub store: PathBuf,
}

/// The OAuth app registration and the secret that guards the install route.
///
/// `client_secret` and `install_secret` are [`Secret`], which renders as
/// `[redacted]`, so neither reaches a log line or a rendered config. Feed them
/// from `KUROKO_LINEAR_CLIENT_SECRET` or one of the `_FILE` variables rather
/// than the TOML, which the NixOS module writes to the world-readable store.
#[derive(Clone, Debug)]
pub struct OauthConfig {
    pub client: OauthClient,
    pub install_secret: Secret,
}

/// The one transport the operator configured.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransportChoice {
    Stdio {
        command: String,
        args: Vec<String>,
        timeout: Duration,
    },
    WebSocket {
        url: String,
        timeout: Duration,
    },
}

/// Why the daemon refuses to start.
#[derive(Debug)]
pub enum BootError {
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    Toml(toml::de::Error),
    Invalid(String),
}

impl std::fmt::Display for BootError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Read { path, source } => write!(f, "cannot read {}: {source}", path.display()),
            Self::Toml(error) => write!(f, "cannot parse the config: {error}"),
            Self::Invalid(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for BootError {}

impl Config {
    /// Reads `path`, then parses and validates it.
    ///
    /// # Errors
    ///
    /// Returns [`BootError`] when the file cannot be read, does not parse as TOML,
    /// or fails validation.
    pub fn load(path: &Path) -> Result<Self, BootError> {
        let text = std::fs::read_to_string(path).map_err(|source| BootError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        let mut config = Self::from_toml(&text, &EnvOverrides::from_environment()?)?;
        // A relative store path belongs beside the config, so a launch from another
        // working directory does not scatter the database and its WAL files.
        if config.store.is_relative()
            && let Some(parent) = path.parent()
        {
            config.store = parent.join(&config.store);
        }
        Ok(config)
    }

    /// Parses `text`, letting `env` override the two secrets.
    ///
    /// # Errors
    ///
    /// Returns [`BootError`] when the text does not parse as TOML or fails validation.
    pub fn from_toml(text: &str, env: &EnvOverrides) -> Result<Self, BootError> {
        let raw: RawConfig = toml::from_str(text).map_err(BootError::Toml)?;
        raw.resolve(env)
    }
}

/// The environment values a test can inject instead of mutating the process.
///
/// Every secret is already resolved to its value here, so [`Config::from_toml`]
/// stays a pure function of text and values and the filesystem is read once, at
/// boot.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EnvOverrides {
    pub webhook_secret: Option<String>,
    pub client_secret: Option<String>,
    pub install_secret: Option<String>,
}

impl EnvOverrides {
    /// Reads the process environment, including the files the `_FILE` variables
    /// name.
    ///
    /// # Errors
    ///
    /// Returns [`BootError::Read`] when a named file cannot be read, so a unit
    /// whose `LoadCredential` path is wrong stops at boot instead of starting
    /// without the secret.
    pub fn from_environment() -> Result<Self, BootError> {
        let client_file = std::env::var_os(CLIENT_SECRET_FILE_ENV).map(PathBuf::from);
        let install_file = std::env::var_os(INSTALL_SECRET_FILE_ENV).map(PathBuf::from);
        let client_value = std::env::var(CLIENT_SECRET_ENV).ok();
        let install_value = std::env::var(INSTALL_SECRET_ENV).ok();
        Ok(Self {
            webhook_secret: std::env::var(SECRET_ENV).ok(),
            client_secret: secret_override(client_value.as_deref(), client_file.as_deref())?,
            install_secret: secret_override(install_value.as_deref(), install_file.as_deref())?,
        })
    }
}

/// The value variable if it holds something, otherwise the file the `_FILE`
/// variable names.
///
/// The value wins when it is non-empty, so an operator can override a stale
/// credential without editing the unit. A trailing newline is not part of a
/// secret, and both `echo` and a hand-written credential file can add one. An
/// empty file counts as unset, which leaves the TOML value in charge.
fn secret_override(value: Option<&str>, file: Option<&Path>) -> Result<Option<String>, BootError> {
    if let Some(value) = value.filter(|value| !value.is_empty()) {
        return Ok(Some(value.to_owned()));
    }
    let Some(path) = file else {
        return Ok(None);
    };
    let text = std::fs::read_to_string(path).map_err(|source| BootError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(Some(text.trim_end().to_owned()).filter(|value| !value.is_empty()))
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    server: ServerSection,
    #[serde(default)]
    linear: LinearSection,
    #[serde(default)]
    trigger: TriggerSection,
    #[serde(default)]
    dispatch: DispatchSection,
    #[serde(default)]
    prompt: PromptSection,
    #[serde(default)]
    store: StoreSection,
    #[serde(default)]
    transport: RawTransport,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ServerSection {
    bind: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct LinearSection {
    secret: Option<String>,
    replay_window_secs: Option<i64>,
    #[serde(default)]
    oauth: Option<OauthSection>,
}

/// Read as `Option` fields so a partial table reports every missing key at once
/// instead of stopping at the first one serde requires.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct OauthSection {
    client_id: Option<String>,
    client_secret: Option<String>,
    redirect_uri: Option<String>,
    install_secret: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct TriggerSection {
    any_delegate: Option<bool>,
    #[serde(default)]
    delegate_ids: Vec<String>,
    #[serde(default)]
    label_names: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct DispatchSection {
    max_in_flight: Option<usize>,
    max_queued: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct PromptSection {
    template: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoreSection {
    path: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTransport {
    stdio: Option<StdioSection>,
    websocket: Option<WebSocketSection>,
}

impl RawTransport {
    /// Resolves the one transport the operator configured.
    fn resolve(self) -> Result<TransportChoice, BootError> {
        match (self.stdio, self.websocket) {
            (Some(stdio), None) => stdio.resolve(),
            (None, Some(websocket)) => websocket.resolve(),
            (None, None) => Err(invalid(
                "configure exactly one of `[transport.stdio]` or `[transport.websocket]`: neither is set"
                    .to_owned(),
            )),
            (Some(_), Some(_)) => Err(invalid(
                "configure exactly one of `[transport.stdio]` or `[transport.websocket]`: both are set"
                    .to_owned(),
            )),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StdioSection {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    timeout_secs: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WebSocketSection {
    url: String,
    timeout_secs: Option<u64>,
}

impl RawConfig {
    fn resolve(self, env: &EnvOverrides) -> Result<Config, BootError> {
        let Self {
            server,
            linear,
            trigger,
            dispatch,
            prompt,
            store,
            transport,
        } = self;

        let secret = env
            .webhook_secret
            .as_deref()
            .filter(|value| !value.is_empty())
            .or(linear.secret.as_deref())
            .filter(|value| !value.is_empty())
            .map(WebhookSecret::new)
            .ok_or_else(|| {
                invalid(format!(
                    "set `[linear] secret` in the config or {SECRET_ENV} in the environment"
                ))
            })?;

        let oauth = linear.oauth.map(|oauth| oauth.resolve(env)).transpose()?;

        let bind_text = server.bind.as_deref().unwrap_or(DEFAULT_BIND);
        let bind = bind_text
            .parse::<SocketAddr>()
            .map_err(|_| invalid(format!("`[server] bind` `{bind_text}` is not an address")))?;

        let window_secs = linear.replay_window_secs.unwrap_or(DEFAULT_WINDOW_SECS);
        if window_secs < 0 {
            return Err(invalid(
                "`[linear] replay_window_secs` must not be negative".to_owned(),
            ));
        }

        let limits = Limits {
            in_flight: limit(
                dispatch.max_in_flight,
                DEFAULT_MAX_IN_FLIGHT,
                "[dispatch] max_in_flight",
            )?,
            max_queued: limit(
                dispatch.max_queued,
                DEFAULT_MAX_QUEUED,
                "[dispatch] max_queued",
            )?,
        };

        let transport = transport.resolve()?;

        if trigger.delegate_ids.iter().any(String::is_empty) {
            return Err(invalid(
                "`[trigger] delegate_ids` must not contain an empty id".to_owned(),
            ));
        }
        if trigger.label_names.iter().any(String::is_empty) {
            return Err(invalid(
                "`[trigger] label_names` must not contain an empty name".to_owned(),
            ));
        }

        let store_text = store.path.as_deref().unwrap_or(DEFAULT_STORE_PATH);
        if store_text.is_empty() {
            return Err(invalid("`[store] path` must not be empty".to_owned()));
        }

        Ok(Config {
            bind,
            secret,
            oauth,
            replay_window: Millis(window_secs.saturating_mul(1000)),
            triggers: TriggerConfig {
                any_delegate: trigger.any_delegate.unwrap_or(true),
                delegate_ids: trigger
                    .delegate_ids
                    .into_iter()
                    .map(DelegateId::new)
                    .collect(),
                label_names: trigger
                    .label_names
                    .into_iter()
                    .map(LabelName::new)
                    .collect(),
            },
            template: PromptTemplate::new(
                prompt
                    .template
                    .unwrap_or_else(|| DEFAULT_TEMPLATE.to_owned()),
            ),
            limits,
            transport,
            store: PathBuf::from(store_text),
        })
    }
}

impl OauthSection {
    /// Builds the OAuth config, or names every key the table is missing.
    fn resolve(self, env: &EnvOverrides) -> Result<OauthConfig, BootError> {
        let Self {
            client_id,
            client_secret,
            redirect_uri,
            install_secret,
        } = self;

        let client_secret = env
            .client_secret
            .as_deref()
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .or(client_secret);
        let install_secret = env
            .install_secret
            .as_deref()
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .or(install_secret);
        let keys = [
            ("client_id", client_id.as_deref()),
            ("client_secret", client_secret.as_deref()),
            ("redirect_uri", redirect_uri.as_deref()),
            ("install_secret", install_secret.as_deref()),
        ];
        let missing = keys
            .iter()
            .filter(|(_, value)| value.is_none_or(str::is_empty))
            .map(|(name, _)| *name)
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            return Err(invalid(format!(
                "`[linear.oauth]` sets client_id, client_secret, redirect_uri, and install_secret together, or not at all; missing {}",
                missing.join(", ")
            )));
        }

        let (Some(client_id), Some(client_secret), Some(redirect_uri), Some(install_secret)) =
            (client_id, client_secret, redirect_uri, install_secret)
        else {
            return Err(invalid(
                "`[linear.oauth]` must set all four keys or none".to_owned(),
            ));
        };
        if !(redirect_uri.starts_with("https://") || redirect_uri.starts_with("http://")) {
            return Err(invalid(format!(
                "`[linear.oauth] redirect_uri` `{redirect_uri}` must start with http:// or https://"
            )));
        }

        Ok(OauthConfig {
            client: OauthClient {
                client_id,
                client_secret: Secret::new(client_secret),
                redirect_uri,
            },
            install_secret: Secret::new(install_secret),
        })
    }
}

impl StdioSection {
    fn resolve(self) -> Result<TransportChoice, BootError> {
        if self.command.is_empty() {
            return Err(invalid(
                "`[transport.stdio] command` must not be empty".to_owned(),
            ));
        }
        Ok(TransportChoice::Stdio {
            command: self.command,
            args: self.args,
            timeout: seconds(self.timeout_secs, "[transport.stdio] timeout_secs")?,
        })
    }
}

impl WebSocketSection {
    fn resolve(self) -> Result<TransportChoice, BootError> {
        if !(self.url.starts_with("ws://") || self.url.starts_with("wss://")) {
            return Err(invalid(format!(
                "`[transport.websocket] url` `{}` must start with ws:// or wss://",
                self.url
            )));
        }
        Ok(TransportChoice::WebSocket {
            url: self.url,
            timeout: seconds(self.timeout_secs, "[transport.websocket] timeout_secs")?,
        })
    }
}

fn limit(value: Option<usize>, fallback: usize, name: &str) -> Result<NonZeroUsize, BootError> {
    NonZeroUsize::new(value.unwrap_or(fallback))
        .ok_or_else(|| invalid(format!("`{name}` must be at least 1")))
}

fn seconds(value: Option<u64>, name: &str) -> Result<Duration, BootError> {
    let secs = value.unwrap_or(DEFAULT_TIMEOUT_SECS);
    if secs == 0 {
        return Err(invalid(format!("`{name}` must be at least 1")));
    }
    Ok(Duration::from_secs(secs))
}

const fn invalid(message: String) -> BootError {
    BootError::Invalid(message)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::path::Path;

    use super::secret_override;

    #[test]
    fn the_value_wins_over_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("secret");
        std::fs::write(&file, "from-file").unwrap();

        let resolved = secret_override(Some("from-value"), Some(&file)).unwrap();

        assert_eq!(resolved.as_deref(), Some("from-value"));
    }

    #[test]
    fn an_empty_value_falls_back_to_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("secret");
        std::fs::write(&file, "from-file\n").unwrap();

        assert_eq!(
            secret_override(Some(""), Some(&file)).unwrap().as_deref(),
            Some("from-file")
        );
        assert_eq!(
            secret_override(None, Some(&file)).unwrap().as_deref(),
            Some("from-file")
        );
    }

    #[test]
    fn neither_a_value_nor_a_file_resolves_to_nothing() {
        assert_eq!(secret_override(None, None).unwrap(), None);
    }

    #[test]
    fn an_empty_file_resolves_to_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("secret");
        std::fs::write(&file, "\n").unwrap();

        assert_eq!(secret_override(None, Some(&file)).unwrap(), None);
    }

    #[test]
    fn an_unreadable_file_names_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("absent");

        let message = secret_override(None, Some(Path::new(&missing)))
            .unwrap_err()
            .to_string();

        assert!(message.contains("absent"), "{message}");
    }
}
