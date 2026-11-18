#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use kuroko_core::Millis;
use kuroko_linear::oauth::{
    AuthorizeRequest, OauthScope, Scopes, StateToken, TokenError, TokenResponse, expires_at,
    needs_refresh,
};
use rand::RngCore;

const NOW: Millis = Millis(1_700_000_000_000);
const TTL: Millis = Millis(600_000);
const AUTHORIZE_BASE: &str = "https://linear.app/oauth/authorize";

/// `base64url` of `bytes 0..32` without padding, the golden state token.
const GOLDEN_STATE: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8";

fn golden_state() -> StateToken {
    StateToken::generate(&mut CountingRng(0), NOW, TTL)
}

/// Hands out byte 0, 1, 2, ... so a generation is reproducible.
struct CountingRng(u8);

impl RngCore for CountingRng {
    fn next_u32(&mut self) -> u32 {
        u32::from(self.0)
    }

    fn next_u64(&mut self) -> u64 {
        u64::from(self.0)
    }

    fn fill_bytes(&mut self, dst: &mut [u8]) {
        for slot in dst {
            *slot = self.0;
            self.0 = self.0.wrapping_add(1);
        }
    }
}

#[test]
fn authorize_url_is_golden() {
    let request = AuthorizeRequest {
        client_id: String::from("client-abc"),
        redirect_uri: String::from("https://kuroko.example/linear/callback"),
        state: golden_state(),
        scopes: Scopes::parse("read write app:assignable app:mentionable"),
    };

    assert_eq!(
        request.url(AUTHORIZE_BASE),
        "https://linear.app/oauth/authorize?client_id=client-abc&redirect_uri=https%3A%2F%2Fkuroko.example%2Flinear%2Fcallback&response_type=code&scope=read%2Cwrite%2Capp%3Aassignable%2Capp%3Amentionable&state=AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8&actor=app"
    );
}

#[test]
fn authorize_url_reuses_the_given_base() {
    let request = AuthorizeRequest {
        client_id: String::from("id"),
        redirect_uri: String::from("https://example.test/cb"),
        state: golden_state(),
        scopes: Scopes::parse("read"),
    };

    assert!(
        request
            .url("https://example.test/authorize")
            .starts_with("https://example.test/authorize?")
    );
}

#[test]
fn parse_accepts_a_space_separated_scope_string() {
    assert_eq!(
        Scopes::parse("read write app:assignable app:mentionable").to_string(),
        "read,write,app:assignable,app:mentionable"
    );
}

#[test]
fn parse_accepts_a_comma_separated_scope_string() {
    assert_eq!(
        Scopes::parse("read,write,app:assignable,app:mentionable").to_string(),
        "read,write,app:assignable,app:mentionable"
    );
}

#[test]
fn parse_skips_empty_tokens() {
    assert_eq!(Scopes::parse("read,,write ").to_string(), "read,write");
}

#[test]
fn parse_of_an_empty_string_grants_nothing() {
    assert_eq!(Scopes::parse("").to_string(), "");
}

#[test]
fn unknown_scope_survives_a_round_trip() {
    let granted = Scopes::parse("read app:future-capability write");

    assert_eq!(granted.to_string(), "read,app:future-capability,write");
    assert_eq!(
        Scopes::parse(&granted.to_string()).to_string(),
        "read,app:future-capability,write"
    );
}

#[test]
fn scope_display_names_each_documented_scope() {
    let rendered: Vec<String> = ["read", "write", "app:assignable", "app:mentionable"]
        .iter()
        .map(|token| Scopes::parse(token).to_string())
        .collect();

    assert_eq!(
        rendered,
        ["read", "write", "app:assignable", "app:mentionable"]
    );
}

#[test]
fn an_unknown_scope_renders_verbatim() {
    assert_eq!(
        OauthScope::Unknown(String::from("app:future")).to_string(),
        "app:future"
    );
}

#[test]
fn state_token_is_43_base64url_characters() {
    assert_eq!(golden_state().as_str(), GOLDEN_STATE);
    assert_eq!(golden_state().as_str().len(), 43);
}

#[test]
fn state_token_rejects_a_one_byte_change() {
    let token = golden_state();
    let mut tampered: Vec<u8> = token.as_str().bytes().collect();
    tampered[20] ^= 0x01;
    let tampered = String::from_utf8(tampered).unwrap();

    assert!(token.matches(GOLDEN_STATE));
    assert!(!token.matches(&tampered));
    assert!(!token.matches(""));
}

#[test]
fn two_generations_differ() {
    let mut rng = CountingRng(0);
    let first = StateToken::generate(&mut rng, NOW, TTL);
    let second = StateToken::generate(&mut rng, NOW, TTL);

    assert_eq!(first.as_str().len(), 43);
    assert_eq!(second.as_str().len(), 43);
    assert_ne!(first, second);
    assert!(!first.matches(second.as_str()));
}

#[test]
fn state_token_carries_its_expiry() {
    let token = golden_state();

    assert_eq!(token.expires_at(), Millis(NOW.0 + TTL.0));
    assert!(token.is_live(NOW));
    assert!(token.is_live(Millis(token.expires_at().0 - 1)));
    assert!(!token.is_live(token.expires_at()));
}

#[test]
fn token_json_with_a_refresh_token() {
    let token = TokenResponse::parse(
        br#"{"access_token":"access-1","refresh_token":"refresh-1","expires_in":86399,"token_type":"Bearer","scope":"read write app:assignable app:mentionable"}"#,
    )
    .unwrap();

    assert_eq!(token.access_token.expose(), "access-1");
    assert_eq!(
        token
            .refresh_token
            .map(|token| token.expose().to_owned())
            .as_deref(),
        Some("refresh-1")
    );
    assert_eq!(token.expires_in, 86_399);
    assert_eq!(
        token.scopes.to_string(),
        "read,write,app:assignable,app:mentionable"
    );
}

#[test]
fn token_json_without_a_refresh_token() {
    let token =
        TokenResponse::parse(br#"{"access_token":"access-1","expires_in":3600,"scope":"read"}"#)
            .unwrap();

    assert_eq!(token.refresh_token, None);
    assert_eq!(token.scopes.to_string(), "read");
}

#[test]
fn token_scope_as_an_array() {
    let token = TokenResponse::parse(
        br#"{"access_token":"a","expires_in":1,"scope":["read","write","app:mentionable"]}"#,
    )
    .unwrap();

    assert_eq!(token.scopes.to_string(), "read,write,app:mentionable");
}

#[test]
fn token_scope_array_tolerates_awkward_entries() {
    let token = TokenResponse::parse(
        br#"{"access_token":"a","expires_in":1,"scope":["read write",7,"app:future"]}"#,
    )
    .unwrap();

    assert_eq!(token.scopes.to_string(), "read,write,app:future");
}

#[test]
fn token_scope_absent_grants_nothing() {
    let token = TokenResponse::parse(br#"{"access_token":"a","expires_in":1}"#).unwrap();

    assert_eq!(token.scopes.to_string(), "");
}

#[test]
fn token_missing_access_token() {
    let error = TokenResponse::parse(br#"{"expires_in":3600,"scope":"read"}"#).unwrap_err();

    assert_eq!(error, TokenError::MissingField("access_token"));
    assert_eq!(error.to_string(), "the token response has no access_token");
}

#[test]
fn token_missing_expires_in() {
    let error = TokenResponse::parse(br#"{"access_token":"a","scope":"read"}"#).unwrap_err();

    assert_eq!(error, TokenError::MissingField("expires_in"));
    assert_eq!(error.to_string(), "the token response has no expires_in");
}

#[test]
fn token_body_that_is_not_json() {
    let error = TokenResponse::parse(b"<html>502 Bad Gateway</html>").unwrap_err();

    assert_eq!(error, TokenError::NotJson);
    assert_eq!(
        error.to_string(),
        "the token endpoint answered with a body that is not JSON"
    );
}

#[test]
fn token_error_body() {
    let error = TokenResponse::parse(
        br#"{"error":"invalid_grant","error_description":"The authorization code expired"}"#,
    )
    .unwrap_err();

    assert_eq!(
        error,
        TokenError::Rejected {
            error: String::from("invalid_grant"),
            description: Some(String::from("The authorization code expired")),
        }
    );
    assert_eq!(
        error.to_string(),
        "Linear rejected the token request: invalid_grant (The authorization code expired)"
    );
}

#[test]
fn token_error_body_without_a_description() {
    let error = TokenResponse::parse(br#"{"error":"invalid_client"}"#).unwrap_err();

    assert_eq!(
        error,
        TokenError::Rejected {
            error: String::from("invalid_client"),
            description: None,
        }
    );
    assert_eq!(
        error.to_string(),
        "Linear rejected the token request: invalid_client"
    );
}

#[test]
fn token_error_is_a_std_error() {
    let error: Box<dyn std::error::Error> = Box::new(TokenError::NotJson);

    assert!(error.source().is_none());
}

#[test]
fn expires_at_converts_seconds_to_millis() {
    assert_eq!(expires_at(NOW, 3_600), Millis(NOW.0 + 3_600_000));
    assert_eq!(expires_at(NOW, 0), NOW);
}

#[test]
fn a_hostile_lifetime_saturates_instead_of_overflowing() {
    assert_eq!(expires_at(NOW, u64::MAX), Millis(i64::MAX));
    assert_eq!(expires_at(Millis(i64::MAX - 1), 1), Millis(i64::MAX));
    assert_eq!(
        StateToken::generate(&mut CountingRng(0), Millis(i64::MAX - 1), TTL).expires_at(),
        Millis(i64::MAX)
    );
}

#[test]
fn needs_refresh_at_the_skew_boundary() {
    let expiry = Millis(NOW.0 + 3_600_000);
    let skew = Millis(300_000);
    let boundary = Millis(expiry.0 - skew.0);

    assert!(!needs_refresh(expiry, Millis(boundary.0 - 1), skew));
    assert!(needs_refresh(expiry, boundary, skew));
    assert!(needs_refresh(expiry, Millis(boundary.0 + 1), skew));
    assert!(needs_refresh(expiry, expiry, skew));
    assert!(!needs_refresh(expiry, NOW, skew));
}

#[test]
fn a_skew_longer_than_the_lifetime_always_refreshes() {
    let expiry = Millis(NOW.0 + 1_000);

    assert!(needs_refresh(expiry, NOW, Millis(60_000)));
    assert!(needs_refresh(expiry, NOW, Millis(i64::MAX)));
}
