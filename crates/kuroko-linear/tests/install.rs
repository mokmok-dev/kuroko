#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Mutex;

use kuroko_core::Millis;
use kuroko_linear::oauth::{
    HttpError, HttpPost, HttpResponse, INSTALL_STATE_TTL, InstallError, Installation, OauthClient,
    PostRequest, Scopes, Secret, StateStore, StateToken, TokenError, TokenResponse,
    complete_install,
};
use rand::RngCore;

const NOW: Millis = Millis(1_700_000_000_000);
const CLIENT_ID: &str = "client-abc";
const CLIENT_SECRET: &str = "secret-xyz";
const REDIRECT: &str = "https://kuroko.example/linear/oauth/callback";
const CODE: &str = "code-1";
const TOKEN_OK: &str = r#"{"access_token":"access-1","refresh_token":"refresh-1","expires_in":86399,"token_type":"Bearer","scope":"read app:assignable"}"#;
const VIEWER_OK: &str = r#"{"data":{"viewer":{"id":"app-user-1"}}}"#;

fn client() -> OauthClient {
    OauthClient {
        client_id: String::from(CLIENT_ID),
        client_secret: Secret::new(CLIENT_SECRET),
        redirect_uri: String::from(REDIRECT),
    }
}

fn state() -> StateToken {
    StateToken::generate(&mut CountingRng(0), NOW, INSTALL_STATE_TTL)
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

/// Records every request and answers from a queue of scripted results.
struct Stub {
    seen: Mutex<Vec<Seen>>,
    answers: Mutex<Vec<Result<HttpResponse, HttpError>>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Seen {
    url: String,
    content_type: String,
    authorization: Option<String>,
    body: String,
}

impl Stub {
    const fn answering(answers: Vec<Result<HttpResponse, HttpError>>) -> Self {
        Self {
            seen: Mutex::new(Vec::new()),
            answers: Mutex::new(answers),
        }
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

impl HttpPost for Stub {
    fn post<'a>(
        &'a self,
        request: PostRequest<'a>,
    ) -> kuroko_linear::oauth::BoxFuture<'a, Result<HttpResponse, HttpError>> {
        self.seen.lock().unwrap().push(Seen {
            url: String::from(request.url),
            content_type: String::from(request.content_type),
            authorization: request.authorization.map(str::to_owned),
            body: String::from_utf8_lossy(request.body).into_owned(),
        });
        let answer = self.answers.lock().unwrap().remove(0);
        Box::pin(async move { answer })
    }
}

fn http_answer(body: &str) -> HttpResponse {
    HttpResponse {
        status: 200,
        body: body.as_bytes().to_vec(),
    }
}

fn answered(status: u16, body: &[u8]) -> HttpResponse {
    HttpResponse {
        status,
        body: body.to_vec(),
    }
}

#[test]
fn a_secret_matches_only_itself() {
    let secret = Secret::new(CLIENT_SECRET);

    assert!(secret.matches(CLIENT_SECRET));
    assert!(!secret.matches("secret-xyZ"));
    assert!(!secret.matches(""));
    assert!(!secret.matches("secret-xyz-longer"));
    assert!(!secret.matches("secret-xy"));
}

#[test]
fn a_secret_never_prints_its_value() {
    let rendered = format!("{:?}", Secret::new(CLIENT_SECRET));

    assert_eq!(rendered, "[redacted]");
    assert!(!rendered.contains(CLIENT_SECRET));
}

#[test]
fn the_authorize_url_carries_the_agent_install_parameters() {
    let url = client().authorize_url(state());

    assert!(
        url.starts_with("https://linear.app/oauth/authorize?"),
        "{url}"
    );
    for expected in [
        "client_id=client-abc",
        "redirect_uri=https%3A%2F%2Fkuroko.example%2Flinear%2Foauth%2Fcallback",
        "response_type=code",
        "scope=read%2Cwrite%2Capp%3Aassignable%2Capp%3Amentionable",
        "state=AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8",
        "actor=app",
    ] {
        assert!(url.contains(expected), "{expected} missing from {url}");
    }
}

#[test]
fn the_token_body_is_the_documented_form() {
    assert_eq!(
        client().token_body(CODE),
        "code=code-1&redirect_uri=https%3A%2F%2Fkuroko.example%2Flinear%2Foauth%2Fcallback&client_id=client-abc&client_secret=secret-xyz&grant_type=authorization_code"
    );
}

#[test]
fn the_token_body_percent_encodes_the_code() {
    let body = client().token_body("a b&c=d");

    assert!(body.starts_with("code=a+b%26c%3Dd&"), "{body}");
}

#[tokio::test]
async fn an_exchange_posts_a_form_and_parses_the_grant() {
    let http = Stub::answering(vec![Ok(http_answer(TOKEN_OK))]);
    let token = client().exchange(&http, CODE).await.unwrap();

    assert_eq!(token.access_token, "access-1");
    assert_eq!(token.refresh_token.as_deref(), Some("refresh-1"));
    assert_eq!(token.scopes.to_string(), "read,app:assignable");

    let seen = http.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].url, "https://api.linear.app/oauth/token");
    assert_eq!(seen[0].content_type, "application/x-www-form-urlencoded");
    assert_eq!(seen[0].authorization, None);
    assert_eq!(seen[0].body, client().token_body(CODE));
}

#[tokio::test]
async fn a_rejected_exchange_names_the_reason_linear_gives() {
    let http = Stub::answering(vec![Ok(answered(
        400,
        br#"{"error":"invalid_grant","error_description":"The code expired"}"#,
    ))]);
    let error = client().exchange(&http, CODE).await.unwrap_err();

    assert_eq!(
        error,
        InstallError::Token(TokenError::Rejected {
            error: String::from("invalid_grant"),
            description: Some(String::from("The code expired")),
        })
    );
    assert_eq!(
        error.to_string(),
        "Linear rejected the token request: invalid_grant (The code expired)"
    );
}

#[tokio::test]
async fn a_refusal_without_a_readable_body_names_the_status() {
    let http = Stub::answering(vec![Ok(answered(502, b"<html>bad gateway</html>"))]);
    let error = client().exchange(&http, CODE).await.unwrap_err();

    assert_eq!(error, InstallError::TokenStatus(502));
    assert_eq!(
        error.to_string(),
        "the token endpoint answered with status 502"
    );
}

#[tokio::test]
async fn a_2xx_exchange_with_a_bad_body_is_a_token_error() {
    let http = Stub::answering(vec![Ok(http_answer("<html>hello</html>"))]);
    let error = client().exchange(&http, CODE).await.unwrap_err();

    assert_eq!(error, InstallError::Token(TokenError::NotJson));
}

#[tokio::test]
async fn a_timed_out_exchange_is_a_timeout() {
    let http = Stub::answering(vec![Err(HttpError::Timeout)]);
    let error = client().exchange(&http, CODE).await.unwrap_err();

    assert_eq!(error, InstallError::Http(HttpError::Timeout));
    assert_eq!(error.to_string(), "the request timed out");
}

#[tokio::test]
async fn a_viewer_lookup_posts_the_query_with_a_bearer_token() {
    let http = Stub::answering(vec![Ok(http_answer(VIEWER_OK))]);
    let id = client().viewer_id(&http, "access-1").await.unwrap();

    assert_eq!(id, "app-user-1");
    let seen = http.seen();
    assert_eq!(seen[0].url, "https://api.linear.app/graphql");
    assert_eq!(seen[0].content_type, "application/json");
    assert_eq!(seen[0].authorization.as_deref(), Some("Bearer access-1"));
    assert_eq!(seen[0].body, r#"{"query":"query Me { viewer { id } }"}"#);
}

#[tokio::test]
async fn a_viewer_status_names_the_status() {
    let http = Stub::answering(vec![Ok(answered(502, b""))]);
    let error = client().viewer_id(&http, "access-1").await.unwrap_err();

    assert_eq!(error, InstallError::ViewerStatus(502));
    assert_eq!(
        error.to_string(),
        "the viewer query answered with status 502"
    );
}

#[tokio::test]
async fn a_viewer_body_that_is_not_json_is_named() {
    let http = Stub::answering(vec![Ok(http_answer("nope"))]);
    let error = client().viewer_id(&http, "access-1").await.unwrap_err();

    assert_eq!(error, InstallError::ViewerNotJson);
    assert_eq!(
        error.to_string(),
        "the viewer query answered with a body that is not JSON"
    );
}

#[tokio::test]
async fn a_viewer_errors_array_fails_the_install() {
    let http = Stub::answering(vec![Ok(http_answer(
        r#"{"errors":[{"message":"not authorized"},{"message":"again"}]}"#,
    ))]);
    let error = client().viewer_id(&http, "access-1").await.unwrap_err();

    assert_eq!(
        error,
        InstallError::ViewerErrors(String::from("not authorized; again"))
    );
    assert_eq!(
        error.to_string(),
        "the viewer query failed: not authorized; again"
    );
}

#[tokio::test]
async fn a_viewer_errors_array_without_messages_still_fails() {
    let http = Stub::answering(vec![Ok(http_answer(r#"{"errors":[{"path":["viewer"]}]}"#))]);
    let error = client().viewer_id(&http, "access-1").await.unwrap_err();

    assert_eq!(
        error,
        InstallError::ViewerErrors(String::from("the viewer query answered with errors"))
    );
}

#[tokio::test]
async fn a_viewer_without_an_id_fails_the_install() {
    let http = Stub::answering(vec![Ok(http_answer(r#"{"data":{"viewer":{"id":""}}}"#))]);
    let error = client().viewer_id(&http, "access-1").await.unwrap_err();

    assert_eq!(error, InstallError::ViewerIdMissing);
    assert_eq!(error.to_string(), "the viewer query returned no viewer id");
}

#[tokio::test]
async fn a_viewer_without_a_data_envelope_fails_the_install() {
    let http = Stub::answering(vec![Ok(http_answer(r#"{"data":{}}"#))]);
    let error = client().viewer_id(&http, "access-1").await.unwrap_err();

    assert_eq!(error, InstallError::ViewerIdMissing);
}

#[tokio::test]
async fn a_complete_install_carries_both_tokens_and_the_expiry() {
    let http = Stub::answering(vec![Ok(http_answer(TOKEN_OK)), Ok(http_answer(VIEWER_OK))]);
    let installation = complete_install(&http, &client(), CODE, NOW).await.unwrap();

    assert_eq!(
        installation,
        Installation {
            app_user_id: String::from("app-user-1"),
            access_token: String::from("access-1"),
            refresh_token: Some(String::from("refresh-1")),
            expires_at: Millis(NOW.0 + 86_399_000),
            scopes: Scopes::parse("read app:assignable"),
            installed_at: NOW,
        }
    );
    assert_eq!(http.seen().len(), 2);
}

#[tokio::test]
async fn a_complete_install_stops_before_the_viewer_when_the_token_fails() {
    let http = Stub::answering(vec![Err(HttpError::Timeout)]);
    let error = complete_install(&http, &client(), CODE, NOW)
        .await
        .unwrap_err();

    assert_eq!(error, InstallError::Http(HttpError::Timeout));
    assert_eq!(http.seen().len(), 1);
}

#[test]
fn an_installation_never_prints_its_tokens() {
    let installation = Installation {
        app_user_id: String::from("app-user-1"),
        access_token: String::from("access-1"),
        refresh_token: Some(String::from("refresh-1")),
        expires_at: NOW,
        scopes: Scopes::default(),
        installed_at: NOW,
    };
    let rendered = format!("{installation:?}");

    assert!(rendered.contains("app-user-1"), "{rendered}");
    assert!(!rendered.contains("access-1"), "{rendered}");
    assert!(!rendered.contains("refresh-1"), "{rendered}");
    assert!(rendered.contains("[redacted]"), "{rendered}");
}

/// The token endpoint's own answer is the one holder of a token that a derived
/// `Debug` still prints. It is not logged today, and this pins it so a later log
/// line cannot spill a credential.
#[test]
fn a_token_response_never_prints_its_tokens() {
    let raw = br#"{"access_token":"access-1","refresh_token":"refresh-1","expires_in":86399,"scope":"read write"}"#;
    let response = TokenResponse::parse(raw).unwrap();
    let rendered = format!("{response:?}");

    assert!(!rendered.contains("access-1"), "{rendered}");
    assert!(!rendered.contains("refresh-1"), "{rendered}");
    assert!(rendered.contains("86399"), "{rendered}");
}

#[test]
fn an_installation_without_a_refresh_token_redacts_as_absent() {
    let installation = Installation {
        app_user_id: String::from("app-user-1"),
        access_token: String::from("access-1"),
        refresh_token: None,
        expires_at: NOW,
        scopes: Scopes::default(),
        installed_at: NOW,
    };

    assert!(
        format!("{installation:?}").contains("refresh_token: None"),
        "{installation:?}"
    );
}

#[test]
fn a_http_error_displays_its_detail() {
    let error = HttpError::Other(String::from("connection reset"));

    assert_eq!(error, HttpError::Other(String::from("connection reset")));
    assert_eq!(error.to_string(), "the request failed: connection reset");
}

#[test]
fn an_install_error_is_a_std_error() {
    use std::error::Error;

    let error: Box<dyn Error> = Box::new(InstallError::ViewerIdMissing);

    assert!(error.source().is_none());
}

#[test]
fn a_state_redeems_exactly_once() {
    let store = StateStore::new();
    let token = state();
    store.insert(token.clone(), NOW);

    assert!(store.consume(token.as_str(), NOW));
    assert!(!store.consume(token.as_str(), NOW));
}

#[test]
fn an_unknown_state_is_refused() {
    let store = StateStore::new();
    store.insert(state(), NOW);

    assert!(!store.consume("someone-elses-state", NOW));
    assert!(!store.consume("", NOW));
}

#[test]
fn an_expired_state_is_refused_and_dropped() {
    let store = StateStore::new();
    let token = state();
    store.insert(token.clone(), NOW);

    assert!(!store.consume(token.as_str(), token.expires_at()));
    assert!(!store.consume(token.as_str(), NOW));
}

#[test]
fn inserting_prunes_expired_states() {
    let store = StateStore::new();
    let old = state();
    store.insert(old.clone(), NOW);

    let later = Millis(NOW.0 + INSTALL_STATE_TTL.0);
    let fresh = StateToken::generate(&mut CountingRng(200), later, INSTALL_STATE_TTL);
    store.insert(fresh.clone(), later);

    assert!(!store.consume(old.as_str(), later));
    assert!(store.consume(fresh.as_str(), later));
}

#[test]
fn a_live_state_survives_a_later_insert() {
    let store = StateStore::new();
    let first = state();
    store.insert(first.clone(), NOW);

    let later = Millis(NOW.0 + 60_000);
    let second = StateToken::generate(&mut CountingRng(200), later, INSTALL_STATE_TTL);
    store.insert(second.clone(), later);

    assert!(store.consume(first.as_str(), later));
    assert!(store.consume(second.as_str(), later));
}
