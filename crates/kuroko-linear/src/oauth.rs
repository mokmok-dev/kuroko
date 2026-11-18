//! Wire types and the two HTTP calls for installing the app over OAuth.
//!
//! Linear answers the token endpoint with the scopes it granted, so the install
//! flow keeps whatever it sends: a scope this build does not know is carried
//! through rather than dropped.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Mutex, MutexGuard, PoisonError};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use form_urlencoded::Serializer;
use kuroko_core::Millis;
use rand::RngCore;
use serde_json::Value;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// 32 bytes of entropy, which base64url without padding renders as 43
/// characters.
const STATE_BYTES: usize = 32;

const AUTHORIZE_BASE: &str = "https://linear.app/oauth/authorize";
const TOKEN_URL: &str = "https://api.linear.app/oauth/token";
const GRAPHQL_URL: &str = "https://api.linear.app/graphql";
const FORM_CONTENT_TYPE: &str = "application/x-www-form-urlencoded";
const JSON_CONTENT_TYPE: &str = "application/json";

/// `viewer.id` is the app user Linear creates for the installation in this
/// workspace, so it is the id the row is keyed by.
const VIEWER_BODY: &str = r#"{"query":"query Me { viewer { id } }"}"#;

/// How long a browser has to come back from Linear before its state expires.
pub const INSTALL_STATE_TTL: Millis = Millis(600_000);

/// How much of an access token's life is treated as already spent.
///
/// Five minutes, as Linear's own examples use. A token is renewed this long
/// before it expires, so a call that starts inside the window cannot outlive it.
pub const REFRESH_SKEW: Millis = Millis(300_000);

/// The one refusal Linear sends when a refresh token was already used or
/// withdrawn, which no retry can fix.
const INVALID_GRANT: &str = "invalid_grant";

/// Both install requests get this long before the route gives up, well inside a
/// browser's patience for a page that never loads.
pub const HTTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(4);

/// The scopes an agent installation asks for, in the space separated form
/// [`Scopes::parse`] reads.
pub const AGENT_SCOPES: &str = "read write app:assignable app:mentionable";

/// A value that must never reach a log line, a rendered config, or a `Debug`.
///
/// Deliberately not `PartialEq`: [`Secret::matches`] is the only comparison, so a
/// caller cannot reach for `==` and compare the bytes in variable time.
#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    #[must_use]
    pub fn new(secret: impl Into<String>) -> Self {
        Self(secret.into())
    }

    /// Reports whether `candidate` is this secret.
    ///
    /// Both sides are hashed to a fixed width first, so a wrong length costs the
    /// same comparison as a wrong byte and the timing says nothing about the
    /// secret's length.
    #[must_use]
    pub fn matches(&self, candidate: &str) -> bool {
        let expected = Sha256::digest(self.0.as_bytes());
        let provided = Sha256::digest(candidate.as_bytes());
        expected.as_slice().ct_eq(provided.as_slice()).into()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}

/// A POST body the install flow needs sent.
#[derive(Clone, Copy, Debug)]
pub struct PostRequest<'a> {
    pub url: &'a str,
    pub content_type: &'a str,
    /// The whole `Authorization` header value, when the call needs one.
    pub authorization: Option<&'a str>,
    pub body: &'a [u8],
}

/// A response, as much of it as the install reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

/// Why a POST never produced a response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HttpError {
    /// The request outlived [`HTTP_TIMEOUT`].
    Timeout,
    Other(String),
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeout => f.write_str("the request timed out"),
            Self::Other(detail) => write!(f, "the request failed: {detail}"),
        }
    }
}

impl std::error::Error for HttpError {}

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The one transport operation the install needs. A `reqwest` client implements
/// it in the daemon, and a test supplies a stub.
pub trait HttpPost: Send + Sync {
    fn post<'a>(
        &'a self,
        request: PostRequest<'a>,
    ) -> BoxFuture<'a, Result<HttpResponse, HttpError>>;
}

/// The app's OAuth client registration, as Linear's application settings show it.
#[derive(Clone, Debug)]
pub struct OauthClient {
    pub client_id: String,
    /// Linear requires the secret on the token request even for a public client.
    pub client_secret: Secret,
    pub redirect_uri: String,
}

impl OauthClient {
    /// The URL the operator's browser opens to start an install.
    #[must_use]
    pub fn authorize_url(&self, state: StateToken) -> String {
        AuthorizeRequest {
            client_id: self.client_id.clone(),
            redirect_uri: self.redirect_uri.clone(),
            state,
            scopes: Scopes::parse(AGENT_SCOPES),
        }
        .url(AUTHORIZE_BASE)
    }

    /// The form body Linear's token endpoint expects for `code`.
    #[must_use]
    pub fn token_body(&self, code: &str) -> String {
        let mut form = Serializer::new(String::new());
        form.append_pair("code", code);
        form.append_pair("redirect_uri", &self.redirect_uri);
        form.append_pair("client_id", &self.client_id);
        form.append_pair("client_secret", &self.client_secret.0);
        form.append_pair("grant_type", "authorization_code");
        form.finish()
    }

    /// Exchanges `code` for a token.
    ///
    /// # Errors
    ///
    /// Returns [`InstallError::Http`] when the request never answers,
    /// [`InstallError::Token`] when Linear refuses and says why,
    /// [`InstallError::TokenStatus`] when it refuses without a readable reason,
    /// and [`InstallError::Token`] for a 2xx body that is not a usable grant.
    pub async fn exchange(
        &self,
        http: &dyn HttpPost,
        code: &str,
    ) -> Result<TokenResponse, InstallError> {
        let body = self.token_body(code);
        let response = http
            .post(PostRequest {
                url: TOKEN_URL,
                content_type: FORM_CONTENT_TYPE,
                authorization: None,
                body: body.as_bytes(),
            })
            .await
            .map_err(InstallError::Http)?;
        if !(200..300).contains(&response.status) {
            // Linear names the reason in the body even on a 4xx, and that reason is
            // the only thing an operator can act on.
            return Err(match TokenResponse::parse(&response.body) {
                Err(error @ TokenError::Rejected { .. }) => InstallError::Token(error),
                _ => InstallError::TokenStatus(response.status),
            });
        }
        TokenResponse::parse(&response.body).map_err(InstallError::Token)
    }

    /// The form body Linear's token endpoint expects to rotate `refresh_token`.
    #[must_use]
    pub fn refresh_body(&self, refresh_token: &str) -> String {
        let mut form = Serializer::new(String::new());
        form.append_pair("grant_type", "refresh_token");
        form.append_pair("refresh_token", refresh_token);
        form.append_pair("client_id", &self.client_id);
        form.append_pair("client_secret", &self.client_secret.0);
        form.finish()
    }

    /// Trades `refresh_token` for a fresh pair.
    ///
    /// Linear rotates the refresh token on every use, so a failure is split by
    /// whether another attempt could work: only [`RefreshError::Revoked`] means
    /// the credential itself is gone.
    ///
    /// # Errors
    ///
    /// Returns [`RefreshError::Revoked`] when Linear answers `invalid_grant`, and
    /// [`RefreshError::Transient`] for a transport failure or any other refusal.
    pub async fn refresh(
        &self,
        http: &dyn HttpPost,
        refresh_token: &str,
    ) -> Result<TokenResponse, RefreshError> {
        let body = self.refresh_body(refresh_token);
        let response = http
            .post(PostRequest {
                url: TOKEN_URL,
                content_type: FORM_CONTENT_TYPE,
                authorization: None,
                body: body.as_bytes(),
            })
            .await
            .map_err(|error| RefreshError::Transient(error.to_string()))?;
        let refused = !(200..300).contains(&response.status);
        match TokenResponse::parse(&response.body) {
            Ok(token) if !refused => Ok(token),
            Ok(_) => Err(RefreshError::Transient(format!(
                "the token endpoint answered with status {}",
                response.status
            ))),
            // A withdrawn grant is named in the body whatever the status, and it
            // is the one refusal a retry can only make worse.
            Err(TokenError::Rejected { error, .. }) if error == INVALID_GRANT => {
                Err(RefreshError::Revoked)
            }
            Err(error) if refused => Err(RefreshError::Transient(format!(
                "the token endpoint answered with status {}: {error}",
                response.status
            ))),
            Err(error) => Err(RefreshError::Transient(error.to_string())),
        }
    }

    /// Reads the app user id Linear assigned this workspace.
    ///
    /// # Errors
    ///
    /// Returns the same transport failures as [`Self::exchange`],
    /// [`InstallError::ViewerStatus`] for a status outside 2xx, and
    /// [`InstallError::ViewerErrors`] or [`InstallError::ViewerIdMissing`] when
    /// the answered body carries no id.
    pub async fn viewer_id(
        &self,
        http: &dyn HttpPost,
        access_token: &str,
    ) -> Result<String, InstallError> {
        let authorization = format!("Bearer {access_token}");
        let response = http
            .post(PostRequest {
                url: GRAPHQL_URL,
                content_type: JSON_CONTENT_TYPE,
                authorization: Some(&authorization),
                body: VIEWER_BODY.as_bytes(),
            })
            .await
            .map_err(InstallError::Http)?;
        if !(200..300).contains(&response.status) {
            return Err(InstallError::ViewerStatus(response.status));
        }
        parse_viewer(&response.body)
    }
}

/// Reads `data.viewer.id` out of a GraphQL answer.
fn parse_viewer(raw: &[u8]) -> Result<String, InstallError> {
    let body: Value = serde_json::from_slice(raw).map_err(|_| InstallError::ViewerNotJson)?;
    if let Some(errors) = body.get("errors").and_then(Value::as_array) {
        let detail = errors
            .iter()
            .filter_map(|error| error.get("message").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("; ");
        return Err(InstallError::ViewerErrors(if detail.is_empty() {
            String::from("the viewer query answered with errors")
        } else {
            detail
        }));
    }
    body.get("data")
        .and_then(|data| data.get("viewer"))
        .and_then(|viewer| viewer.get("id"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .ok_or(InstallError::ViewerIdMissing)
}

/// One workspace installation, ready to persist.
#[derive(Clone, PartialEq, Eq)]
pub struct Installation {
    pub app_user_id: String,
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Millis,
    pub scopes: Scopes,
    pub installed_at: Millis,
}

impl fmt::Debug for Installation {
    // Redacted by hand: a derived `Debug` would print the tokens the moment an
    // error or a log line touches an installation.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Installation")
            .field("app_user_id", &self.app_user_id)
            .field("access_token", &"[redacted]")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "[redacted]"),
            )
            .field("expires_at", &self.expires_at)
            .field("scopes", &self.scopes)
            .field("installed_at", &self.installed_at)
            .finish()
    }
}

impl Installation {
    /// The pair Linear handed back after `token`, with the fields a refresh does
    /// not replace carried over.
    ///
    /// A refresh answer may omit the rotated refresh token and the granted
    /// scopes. Keeping the stored ones is the only safe reading of an omission,
    /// since the grant did not change and dropping the refresh token would leave
    /// the installation unusable when the new access token expires.
    #[must_use]
    pub fn renew(&self, token: &TokenResponse, now: Millis) -> Self {
        Self {
            app_user_id: self.app_user_id.clone(),
            access_token: token.access_token.clone(),
            refresh_token: token
                .refresh_token
                .clone()
                .or_else(|| self.refresh_token.clone()),
            expires_at: expires_at(now, token.expires_in),
            scopes: if token.scopes.is_empty() {
                self.scopes.clone()
            } else {
                token.scopes.clone()
            },
            installed_at: self.installed_at,
        }
    }

    /// Whole seconds of usable life left at `now`, floored at zero.
    #[must_use]
    pub fn remaining_seconds(&self, now: Millis) -> u64 {
        let left = self.expires_at.0.saturating_sub(now.0);
        u64::try_from(left).map_or(0, |millis| millis / 1_000)
    }
}

/// Why a refresh did not produce a new pair.
///
/// Split by whether another attempt could work, which is what the caller needs
/// to decide between dropping the credential and keeping it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RefreshError {
    /// Linear withdrew the refresh token, either because it expired unused or
    /// because a rotated one was presented twice. No retry can recover it.
    Revoked,
    /// The exchange did not finish. The old pair may still work, and a later
    /// attempt may succeed.
    Transient(String),
}

impl fmt::Display for RefreshError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Revoked => f.write_str("Linear withdrew the refresh token"),
            Self::Transient(detail) => write!(f, "the token refresh failed: {detail}"),
        }
    }
}

impl std::error::Error for RefreshError {}

/// Runs the whole callback exchange: code to token, then token to app user id.
///
/// # Errors
///
/// Returns whichever [`InstallError`] stopped it; nothing is returned, so a
/// caller that only saves on `Ok` never persists a half installation.
pub async fn complete_install(
    http: &dyn HttpPost,
    client: &OauthClient,
    code: &str,
    now: Millis,
) -> Result<Installation, InstallError> {
    let token = client.exchange(http, code).await?;
    let app_user_id = client.viewer_id(http, &token.access_token).await?;
    Ok(Installation {
        app_user_id,
        access_token: token.access_token,
        refresh_token: token.refresh_token,
        expires_at: expires_at(now, token.expires_in),
        scopes: token.scopes,
        installed_at: now,
    })
}

/// Why an install did not finish.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstallError {
    Http(HttpError),
    TokenStatus(u16),
    Token(TokenError),
    ViewerStatus(u16),
    ViewerNotJson,
    ViewerErrors(String),
    ViewerIdMissing,
}

impl fmt::Display for InstallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Http(error) => write!(f, "{error}"),
            Self::TokenStatus(status) => {
                write!(f, "the token endpoint answered with status {status}")
            }
            Self::Token(error) => write!(f, "{error}"),
            Self::ViewerStatus(status) => {
                write!(f, "the viewer query answered with status {status}")
            }
            Self::ViewerNotJson => {
                f.write_str("the viewer query answered with a body that is not JSON")
            }
            Self::ViewerErrors(detail) => write!(f, "the viewer query failed: {detail}"),
            Self::ViewerIdMissing => f.write_str("the viewer query returned no viewer id"),
        }
    }
}

impl std::error::Error for InstallError {}

/// The state tokens minted by `/linear/install` and not yet redeemed.
///
/// A state lives ten minutes and a restart mid-install is a failed install the
/// operator retries, so this lives in memory rather than in a table.
#[derive(Debug, Default)]
pub struct StateStore {
    states: Mutex<Vec<StateToken>>,
}

impl StateStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds `token`, dropping every entry already expired at `now` so the set
    /// cannot grow with retries nobody completes.
    pub fn insert(&self, token: StateToken, now: Millis) {
        let mut states = self.lock();
        states.retain(|state| state.is_live(now));
        states.push(token);
    }

    /// Redeems `candidate` at most once, reporting whether it was live.
    ///
    /// The removal and the comparison happen under one lock, so two callbacks
    /// presenting the same state cannot both succeed.
    pub fn consume(&self, candidate: &str, now: Millis) -> bool {
        let mut states = self.lock();
        states.retain(|state| state.is_live(now));
        let Some(index) = states.iter().position(|state| state.matches(candidate)) else {
            return false;
        };
        states.remove(index);
        true
    }

    /// Recovering a poisoned lock is right here: a panic elsewhere must not wedge
    /// every future install, and these tokens are worth nothing after ten minutes.
    fn lock(&self) -> MutexGuard<'_, Vec<StateToken>> {
        self.states.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// One scope Linear grants an agent app.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum OauthScope {
    Read,
    Write,
    AppAssignable,
    AppMentionable,
    /// A scope this build does not know, kept verbatim.
    Unknown(String),
}

impl OauthScope {
    fn from_token(token: &str) -> Self {
        match token {
            "read" => Self::Read,
            "write" => Self::Write,
            "app:assignable" => Self::AppAssignable,
            "app:mentionable" => Self::AppMentionable,
            other => Self::Unknown(other.to_owned()),
        }
    }

    fn token(&self) -> &str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::AppAssignable => "app:assignable",
            Self::AppMentionable => "app:mentionable",
            Self::Unknown(raw) => raw,
        }
    }
}

impl fmt::Display for OauthScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.token())
    }
}

/// A whole scope grant. Linear sends it as one space separated string on newer
/// apps and as an array on older ones, and the authorize URL wants it comma
/// separated.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Scopes(Vec<OauthScope>);

impl Scopes {
    /// Accepts either separator, because Linear documents one and sends the
    /// other depending on the app's age.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        Self(
            text.split([' ', ','])
                .filter(|token| !token.is_empty())
                .map(OauthScope::from_token)
                .collect(),
        )
    }

    /// Reports whether no scope was granted, which is how an answer that omits
    /// the field reads.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The granted scopes in the order Linear sent them, each as the token it
    /// was received as.
    pub fn tokens(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(OauthScope::token)
    }
}

impl fmt::Display for Scopes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, scope) in self.0.iter().enumerate() {
            if index > 0 {
                f.write_str(",")?;
            }
            write!(f, "{scope}")?;
        }
        Ok(())
    }
}

/// The URL the operator's browser opens to start an install.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthorizeRequest {
    pub client_id: String,
    pub redirect_uri: String,
    pub state: StateToken,
    pub scopes: Scopes,
}

impl AuthorizeRequest {
    /// Renders the authorize URL. `base` is Linear's endpoint, which carries no
    /// query.
    #[must_use]
    pub fn url(&self, base: &str) -> String {
        let mut query = Serializer::new(String::new());
        query.append_pair("client_id", &self.client_id);
        query.append_pair("redirect_uri", &self.redirect_uri);
        query.append_pair("response_type", "code");
        query.append_pair("scope", &self.scopes.to_string());
        query.append_pair("state", self.state.as_str());
        query.append_pair("actor", "app");
        format!("{base}?{}", query.finish())
    }
}

/// A single-use value that ties the callback to the install that started it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StateToken {
    value: String,
    expires_at: Millis,
}

impl StateToken {
    /// Draws the token from `rng` so a test can pin the bytes.
    #[must_use]
    pub fn generate(rng: &mut impl RngCore, now: Millis, ttl: Millis) -> Self {
        let mut bytes = [0_u8; STATE_BYTES];
        rng.fill_bytes(&mut bytes);
        Self {
            value: URL_SAFE_NO_PAD.encode(bytes),
            expires_at: Millis(now.0.saturating_add(ttl.0)),
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.value
    }

    #[must_use]
    pub const fn expires_at(&self) -> Millis {
        self.expires_at
    }

    /// Reports whether `candidate` is this token.
    ///
    /// The comparison runs in constant time so the callback's timing does not
    /// reveal the token a byte at a time.
    #[must_use]
    pub fn matches(&self, candidate: &str) -> bool {
        self.value.as_bytes().ct_eq(candidate.as_bytes()).into()
    }

    /// Reports whether the token may still be redeemed at `now`.
    #[must_use]
    pub const fn is_live(&self, now: Millis) -> bool {
        now.0 < self.expires_at.0
    }
}

/// What Linear's token endpoint hands back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenResponse {
    pub access_token: String,
    pub refresh_token: Option<String>,
    /// Lifetime in seconds, as Linear sends it.
    pub expires_in: u64,
    pub scopes: Scopes,
}

impl TokenResponse {
    /// Parses a token response body.
    ///
    /// # Errors
    ///
    /// Returns [`TokenError::NotJson`] when the body is not JSON,
    /// [`TokenError::Rejected`] for an OAuth error body, and
    /// [`TokenError::MissingField`] when a required field is absent or not the
    /// type Linear documents.
    pub fn parse(raw: &[u8]) -> Result<Self, TokenError> {
        let body: Value = serde_json::from_slice(raw).map_err(|_| TokenError::NotJson)?;
        if let Some(error) = body.get("error").and_then(Value::as_str) {
            return Err(TokenError::Rejected {
                error: error.to_owned(),
                description: body
                    .get("error_description")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            });
        }
        let access_token = body
            .get("access_token")
            .and_then(Value::as_str)
            .ok_or(TokenError::MissingField("access_token"))?;
        let expires_in = body
            .get("expires_in")
            .and_then(Value::as_u64)
            .ok_or(TokenError::MissingField("expires_in"))?;
        Ok(Self {
            access_token: access_token.to_owned(),
            refresh_token: body
                .get("refresh_token")
                .and_then(Value::as_str)
                .map(str::to_owned),
            expires_in,
            scopes: parse_scope_field(body.get("scope")),
        })
    }
}

/// Older Linear apps send `scope` as an array, newer ones as one string.
fn parse_scope_field(field: Option<&Value>) -> Scopes {
    match field {
        Some(Value::String(text)) => Scopes::parse(text),
        Some(Value::Array(items)) => Scopes(
            items
                .iter()
                .filter_map(Value::as_str)
                .flat_map(|text| Scopes::parse(text).0)
                .collect(),
        ),
        _ => Scopes::default(),
    }
}

/// Why a token response did not become a [`TokenResponse`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TokenError {
    NotJson,
    Rejected {
        error: String,
        description: Option<String>,
    },
    MissingField(&'static str),
}

impl fmt::Display for TokenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotJson => {
                f.write_str("the token endpoint answered with a body that is not JSON")
            }
            Self::Rejected { error, description } => {
                write!(f, "Linear rejected the token request: {error}")?;
                description
                    .as_ref()
                    .map_or(Ok(()), |description| write!(f, " ({description})"))
            }
            Self::MissingField(field) => write!(f, "the token response has no {field}"),
        }
    }
}

impl std::error::Error for TokenError {}

/// The instant a token granted now with `expires_in` seconds stops working.
#[must_use]
pub fn expires_at(now: Millis, expires_in: u64) -> Millis {
    let lifetime = expires_in.saturating_mul(1_000);
    let Ok(lifetime) = i64::try_from(lifetime) else {
        return Millis(i64::MAX);
    };
    Millis(now.0.saturating_add(lifetime))
}

/// Reports whether a token expiring at `expires_at` should be refreshed at
/// `now`, given a `skew` of head room.
#[must_use]
pub const fn needs_refresh(expires_at: Millis, now: Millis, skew: Millis) -> bool {
    now.0 >= expires_at.0.saturating_sub(skew.0)
}
