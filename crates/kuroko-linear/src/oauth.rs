//! Wire types and pure functions for installing the app over OAuth.
//!
//! Linear answers the token endpoint with the scopes it granted, so the install
//! flow keeps whatever it sends: a scope this build does not know is carried
//! through rather than dropped.

use std::fmt;

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use form_urlencoded::Serializer;
use kuroko_core::Millis;
use rand::RngCore;
use serde_json::Value;
use subtle::ConstantTimeEq;

/// 32 bytes of entropy, which base64url without padding renders as 43
/// characters.
const STATE_BYTES: usize = 32;

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
