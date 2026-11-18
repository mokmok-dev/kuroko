#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

use hmac::{Hmac, KeyInit, Mac};
use kuroko::config::{Config, EnvOverrides, TransportChoice};
use kuroko_core::{Millis, TaskSpec};
use kuroko_linear::{Gate, Intake};
use serde_json::json;
use sha2::Sha256;

const SECRET: &str = "whsec_file";
const NOW: Millis = Millis(1_700_000_000_000);

fn minimal() -> String {
    format!(
        r#"
[linear]
secret = "{SECRET}"

[transport.stdio]
command = "cat"
"#
    )
}

fn error(text: &str) -> String {
    Config::from_toml(text, &EnvOverrides::default())
        .unwrap_err()
        .to_string()
}

fn sign(secret: &str, body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(body);
    hex::encode(mac.finalize().into_bytes())
}

fn body() -> Vec<u8> {
    json!({
        "action": "create",
        "type": "Issue",
        "webhookTimestamp": NOW.0,
        "data": {
            "id": "issue-a",
            "identifier": "MOK-150",
            "title": "Ship it",
            "delegate": { "id": "agent-1" },
        },
    })
    .to_string()
    .into_bytes()
}

fn admits(config: &Config, secret: &str) -> bool {
    let gate = Gate::new(
        config.secret.clone(),
        config.replay_window,
        config.triggers.clone(),
        config.template.clone(),
    );
    let body = body();
    matches!(
        gate.accept(Some(&sign(secret, &body)), &body, NOW),
        Ok(Intake::Admit(_))
    )
}

#[test]
fn the_defaults_match_the_documented_ones() {
    let config = Config::from_toml(&minimal(), &EnvOverrides::default()).unwrap();

    assert_eq!(
        config.bind,
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8787)
    );
    assert_eq!(config.replay_window, Millis(60_000));
    assert_eq!(config.limits.in_flight.get(), 1);
    assert_eq!(config.limits.max_queued.get(), 64);
    assert_eq!(config.store, PathBuf::from("kuroko.db"));
    assert!(config.triggers.any_delegate);
    assert_eq!(config.triggers.delegate_ids, Vec::new());
    assert_eq!(config.triggers.label_names, Vec::new());
    assert_eq!(
        config.transport,
        TransportChoice::Stdio {
            command: "cat".to_owned(),
            args: Vec::new(),
            timeout: Duration::from_secs(3600),
        }
    );
    assert_eq!(
        config.template.render(&TaskSpec {
            identifier: "MOK-150".to_owned(),
            title: "Ship it".to_owned(),
            description: Some("Body".to_owned()),
            url: Some("https://linear.app/mok/issue/MOK-150".to_owned()),
        }),
        "Issue MOK-150: Ship it\n\nBody\nhttps://linear.app/mok/issue/MOK-150"
    );
}

#[test]
fn the_environment_secret_overrides_the_file() {
    let config = Config::from_toml(
        &minimal(),
        &EnvOverrides {
            webhook_secret: Some(String::from("whsec_env")),
            ..EnvOverrides::default()
        },
    )
    .unwrap();

    assert!(admits(&config, "whsec_env"));
    assert!(!admits(&config, SECRET));
}

#[test]
fn an_empty_environment_secret_keeps_the_file_secret() {
    let config = Config::from_toml(
        &minimal(),
        &EnvOverrides {
            webhook_secret: Some(String::new()),
            ..EnvOverrides::default()
        },
    )
    .unwrap();

    assert!(admits(&config, SECRET));
}

#[test]
fn a_websocket_transport_carries_its_url_and_timeout() {
    let text = format!(
        r#"
[linear]
secret = "{SECRET}"

[transport.websocket]
url = "wss://agent.example/ws"
timeout_secs = 30
"#
    );
    let config = Config::from_toml(&text, &EnvOverrides::default()).unwrap();

    assert_eq!(
        config.transport,
        TransportChoice::WebSocket {
            url: "wss://agent.example/ws".to_owned(),
            timeout: Duration::from_secs(30),
        }
    );
}

#[test]
fn two_transports_are_a_boot_error() {
    let text = format!(
        "{}\n[transport.websocket]\nurl = \"ws://127.0.0.1:9001\"\n",
        minimal()
    );

    assert!(error(&text).contains("both are set"), "{}", error(&text));
}

#[test]
fn no_transport_is_a_boot_error() {
    let text = format!("[linear]\nsecret = \"{SECRET}\"\n");

    assert!(error(&text).contains("neither is set"), "{}", error(&text));
}

#[test]
fn a_missing_secret_names_the_environment_variable() {
    let text = "[transport.stdio]\ncommand = \"cat\"\n".to_owned();

    assert!(
        error(&text).contains("KUROKO_LINEAR_SECRET"),
        "{}",
        error(&text)
    );
}

#[test]
fn a_websocket_url_must_be_ws_or_wss() {
    let text = format!(
        "[linear]\nsecret = \"{SECRET}\"\n\n[transport.websocket]\nurl = \"http://agent.example\"\n"
    );

    assert!(
        error(&text).contains("must start with ws://"),
        "{}",
        error(&text)
    );
}

#[test]
fn a_stdio_transport_needs_a_command() {
    let text = format!("[linear]\nsecret = \"{SECRET}\"\n\n[transport.stdio]\nargs = []\n");

    assert!(error(&text).contains("command"), "{}", error(&text));
}

#[test]
fn a_zero_limit_is_a_boot_error() {
    let text = format!("{}\n[dispatch]\nmax_queued = 0\n", minimal());

    assert!(
        error(&text).contains("`[dispatch] max_queued` must be at least 1"),
        "{}",
        error(&text)
    );
}

#[test]
fn a_store_path_can_be_configured() {
    let text = format!(
        "{}\n[store]\npath = \"/var/lib/kuroko/tasks.db\"\n",
        minimal()
    );
    let config = Config::from_toml(&text, &EnvOverrides::default()).unwrap();

    assert_eq!(config.store, PathBuf::from("/var/lib/kuroko/tasks.db"));
}

#[test]
fn an_empty_store_path_is_a_boot_error() {
    let text = format!("{}\n[store]\npath = \"\"\n", minimal());

    assert!(
        error(&text).contains("`[store] path` must not be empty"),
        "{}",
        error(&text)
    );
}

#[test]
fn a_relative_store_path_resolves_beside_the_config() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("kuroko.toml");
    std::fs::write(
        &path,
        format!("{}\n[store]\npath = \"tasks.db\"\n", minimal()),
    )
    .unwrap();

    let config = Config::load(&path).unwrap();

    assert_eq!(config.store, dir.path().join("tasks.db"));
}

#[test]
fn an_empty_delegate_id_is_a_boot_error() {
    let text = format!("{}\n[trigger]\ndelegate_ids = [\"\"]\n", minimal());

    assert!(
        error(&text).contains("must not contain an empty id"),
        "{}",
        error(&text)
    );
}

#[test]
fn an_empty_label_name_is_a_boot_error() {
    let text = format!("{}\n[trigger]\nlabel_names = [\"\"]\n", minimal());

    assert!(
        error(&text).contains("must not contain an empty name"),
        "{}",
        error(&text)
    );
}

#[test]
fn an_unparsable_bind_is_a_boot_error() {
    let text = format!("[server]\nbind = \"here\"\n{}", minimal());

    assert!(
        error(&text).contains("is not an address"),
        "{}",
        error(&text)
    );
}

const OAUTH: &str = r#"
[linear.oauth]
client_id = "client-abc"
client_secret = "secret-xyz"
redirect_uri = "https://kuroko.example/linear/oauth/callback"
install_secret = "install-1"
"#;

#[test]
fn an_absent_oauth_table_leaves_oauth_unset() {
    let config = Config::from_toml(&minimal(), &EnvOverrides::default()).unwrap();

    assert!(config.oauth.is_none());
}

#[test]
fn a_full_oauth_table_is_accepted() {
    let text = format!("{}{OAUTH}", minimal());
    let config = Config::from_toml(&text, &EnvOverrides::default()).unwrap();
    let oauth = config.oauth.unwrap();

    assert_eq!(oauth.client.client_id, "client-abc");
    assert_eq!(
        oauth.client.redirect_uri,
        "https://kuroko.example/linear/oauth/callback"
    );
    assert!(oauth.client.client_secret.matches("secret-xyz"));
    assert!(oauth.install_secret.matches("install-1"));
}

#[test]
fn a_partial_oauth_table_names_every_missing_key() {
    let text = format!(
        "{}\n[linear.oauth]\nclient_id = \"client-abc\"\n",
        minimal()
    );
    let message = error(&text);
    let missing = message.split("; missing ").nth(1).unwrap_or_default();

    for key in ["client_secret", "redirect_uri", "install_secret"] {
        assert!(missing.contains(key), "{key} missing from {message}");
    }
    assert!(!missing.contains("client_id"), "{message}");
}

#[test]
fn a_partial_oauth_table_does_not_name_the_keys_it_has() {
    let text = format!(
        "{}\n[linear.oauth]\nclient_id = \"c\"\nclient_secret = \"s\"\nredirect_uri = \"https://x.example/cb\"\n",
        minimal()
    );
    let message = error(&text);
    let missing = message.split("; missing ").nth(1).unwrap_or_default();

    assert_eq!(missing, "install_secret", "{message}");
}

#[test]
fn an_empty_oauth_value_counts_as_missing() {
    let text =
        format!("{}{OAUTH}", minimal()).replace("client_id = \"client-abc\"", "client_id = \"\"");
    let message = error(&text);
    let missing = message.split("; missing ").nth(1).unwrap_or_default();

    assert_eq!(missing, "client_id", "{message}");
}

#[test]
fn the_environment_client_secret_overrides_the_file() {
    let text = format!("{}{OAUTH}", minimal());
    let env = EnvOverrides {
        client_secret: Some(String::from("env-secret")),
        ..EnvOverrides::default()
    };
    let config = Config::from_toml(&text, &env).unwrap();
    let oauth = config.oauth.unwrap();

    assert!(oauth.client.client_secret.matches("env-secret"));
    assert!(!oauth.client.client_secret.matches("secret-xyz"));
}

#[test]
fn an_empty_environment_client_secret_keeps_the_file_value() {
    let text = format!("{}{OAUTH}", minimal());
    let env = EnvOverrides {
        client_secret: Some(String::new()),
        ..EnvOverrides::default()
    };
    let config = Config::from_toml(&text, &env).unwrap();

    assert!(
        config
            .oauth
            .unwrap()
            .client
            .client_secret
            .matches("secret-xyz")
    );
}

#[test]
fn the_environment_client_secret_cannot_invent_a_partial_table() {
    let text = format!(
        "{}\n[linear.oauth]\nclient_id = \"client-abc\"\n",
        minimal()
    );
    let env = EnvOverrides {
        client_secret: Some(String::from("env-secret")),
        ..EnvOverrides::default()
    };
    let message = Config::from_toml(&text, &env).unwrap_err().to_string();

    assert!(message.contains("redirect_uri"), "{message}");
    assert!(message.contains("install_secret"), "{message}");
}

#[test]
fn an_unknown_oauth_key_is_a_boot_error() {
    let text = format!("{}{OAUTH}client_uri = \"x\"\n", minimal());

    assert!(error(&text).contains("client_uri"), "{}", error(&text));
}

#[test]
fn an_oauth_redirect_uri_must_be_a_url() {
    let text = format!("{}{OAUTH}", minimal())
        .replace("https://kuroko.example/linear/oauth/callback", "kuroko");

    assert!(
        error(&text).contains("must start with http:// or https://"),
        "{}",
        error(&text)
    );
}

#[test]
fn an_oauth_config_never_prints_its_secrets() {
    let text = format!("{}{OAUTH}", minimal());
    let config = Config::from_toml(&text, &EnvOverrides::default()).unwrap();
    let rendered = format!("{:?}", config.oauth.unwrap());

    assert!(!rendered.contains("secret-xyz"), "{rendered}");
    assert!(!rendered.contains("install-1"), "{rendered}");
    assert!(rendered.contains("[REDACTED]"), "{rendered}");
}

#[test]
fn the_environment_install_secret_overrides_the_file() {
    let text = format!("{}{OAUTH}", minimal());
    let env = EnvOverrides {
        install_secret: Some(String::from("env-install")),
        ..EnvOverrides::default()
    };
    let oauth = Config::from_toml(&text, &env).unwrap().oauth.unwrap();

    assert!(oauth.install_secret.matches("env-install"));
    assert!(!oauth.install_secret.matches("install-1"));
}

#[test]
fn the_environment_install_secret_cannot_invent_a_partial_table() {
    let text = format!(
        "{}\n[linear.oauth]\nclient_id = \"client-abc\"\n",
        minimal()
    );
    let env = EnvOverrides {
        install_secret: Some(String::from("env-install")),
        ..EnvOverrides::default()
    };
    let message = Config::from_toml(&text, &env).unwrap_err().to_string();

    assert!(message.contains("client_secret"), "{message}");
}

#[test]
fn an_empty_environment_install_secret_keeps_the_file_value() {
    let text = format!("{}{OAUTH}", minimal());
    let env = EnvOverrides {
        install_secret: Some(String::new()),
        ..EnvOverrides::default()
    };
    let oauth = Config::from_toml(&text, &env).unwrap().oauth.unwrap();

    assert!(oauth.install_secret.matches("install-1"));
}
