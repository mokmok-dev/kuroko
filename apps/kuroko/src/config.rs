//! TOML configuration, validated before the daemon binds.

use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::time::Duration;

use kuroko_core::{DelegateId, LabelName, Limits, Millis};
use kuroko_linear::{PromptTemplate, TriggerConfig, WebhookSecret};
use serde::Deserialize;

/// The file read when `--config` is absent.
pub const DEFAULT_PATH: &str = "kuroko.toml";

/// Overrides `[linear] secret` when it is set to a non-empty value.
pub const SECRET_ENV: &str = "KUROKO_LINEAR_SECRET";

const DEFAULT_BIND: &str = "127.0.0.1:8787";
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
    pub replay_window: Millis,
    pub triggers: TriggerConfig,
    pub template: PromptTemplate,
    pub limits: Limits,
    pub transport: TransportChoice,
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
        let secret = std::env::var(SECRET_ENV).ok();
        Self::from_toml(&text, secret.as_deref())
    }

    /// Parses `text` and applies `secret_override` over `[linear] secret`.
    ///
    /// # Errors
    ///
    /// Returns [`BootError`] when the text does not parse as TOML or fails validation.
    pub fn from_toml(text: &str, secret_override: Option<&str>) -> Result<Self, BootError> {
        let raw: RawConfig = toml::from_str(text).map_err(BootError::Toml)?;
        raw.resolve(secret_override)
    }
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
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct TriggerSection {
    any_delegate: Option<bool>,
    delegate_ids: Vec<String>,
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
struct RawTransport {
    stdio: Option<StdioSection>,
    websocket: Option<WebSocketSection>,
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
    fn resolve(self, secret_override: Option<&str>) -> Result<Config, BootError> {
        let Self {
            server,
            linear,
            trigger,
            dispatch,
            prompt,
            transport,
        } = self;

        let secret = secret_override
            .filter(|value| !value.is_empty())
            .or(linear.secret.as_deref())
            .filter(|value| !value.is_empty())
            .map(WebhookSecret::new)
            .ok_or_else(|| {
                invalid(format!(
                    "set `[linear] secret` in the config or {SECRET_ENV} in the environment"
                ))
            })?;

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

        let transport = match (transport.stdio, transport.websocket) {
            (Some(stdio), None) => stdio.resolve()?,
            (None, Some(websocket)) => websocket.resolve()?,
            (None, None) => {
                return Err(invalid(
                    "configure exactly one of `[transport.stdio]` or `[transport.websocket]`: neither is set"
                        .to_owned(),
                ));
            }
            (Some(_), Some(_)) => {
                return Err(invalid(
                    "configure exactly one of `[transport.stdio]` or `[transport.websocket]`: both are set"
                        .to_owned(),
                ));
            }
        };

        Ok(Config {
            bind,
            secret,
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
