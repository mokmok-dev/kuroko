#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Mutex;

use kuroko_core::Millis;
use kuroko_linear::oauth::{
    HttpError, HttpPost, HttpResponse, Installation, OauthClient, PostRequest, REFRESH_SKEW,
    RefreshError, Scopes, Secret, TokenResponse,
};
use kuroko_linear::token::{Slot, Step};

const NOW: Millis = Millis(1_700_000_000_000);
const CLIENT_ID: &str = "client-abc";
const CLIENT_SECRET: &str = "secret-xyz";
const REDIRECT: &str = "https://kuroko.example/linear/oauth/callback";
const REFRESH_TOKEN: &str = "refresh-1";
const LIFETIME: u64 = 86_399;
const REFRESHED: &str = r#"{"access_token":"access-2","refresh_token":"refresh-2","expires_in":86399,"token_type":"Bearer","scope":"read write"}"#;

fn client() -> OauthClient {
    OauthClient {
        client_id: String::from(CLIENT_ID),
        client_secret: Secret::new(CLIENT_SECRET),
        redirect_uri: String::from(REDIRECT),
    }
}

/// Answers from a script and records what it was asked, so a test can assert on
/// the request Linear would have seen.
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

fn answered(status: u16, body: &str) -> HttpResponse {
    HttpResponse {
        status,
        body: body.as_bytes().to_vec(),
    }
}

fn installation() -> Installation {
    Installation {
        app_user_id: String::from("app-user-1"),
        access_token: String::from("access-1"),
        refresh_token: Some(String::from(REFRESH_TOKEN)),
        expires_at: Millis(NOW.0 + 86_399_000),
        scopes: Scopes::parse("read write app:assignable"),
        installed_at: NOW,
    }
}

/// A pair expiring inside the skew, which is what forces an exchange.
fn expiring() -> Installation {
    let mut pair = installation();
    pair.expires_at = Millis(NOW.0 + REFRESH_SKEW.0 - 1);
    pair
}

#[test]
fn the_refresh_body_is_the_documented_form() {
    assert_eq!(
        client().refresh_body(REFRESH_TOKEN),
        "grant_type=refresh_token&refresh_token=refresh-1&client_id=client-abc&client_secret=secret-xyz"
    );
}

#[test]
fn the_refresh_body_percent_encodes_the_refresh_token() {
    let body = client().refresh_body("a b&c=d");

    assert!(
        body.starts_with("grant_type=refresh_token&refresh_token=a+b%26c%3Dd&"),
        "{body}"
    );
}

#[tokio::test]
async fn a_refresh_posts_the_form_and_parses_the_new_pair() {
    let http = Stub::answering(vec![Ok(answered(200, REFRESHED))]);
    let token = client().refresh(&http, REFRESH_TOKEN).await.unwrap();

    assert_eq!(token.access_token, "access-2");
    assert_eq!(token.refresh_token.as_deref(), Some("refresh-2"));
    assert_eq!(token.expires_in, LIFETIME);
    assert_eq!(token.scopes.to_string(), "read,write");

    let seen = http.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].url, "https://api.linear.app/oauth/token");
    assert_eq!(seen[0].content_type, "application/x-www-form-urlencoded");
    assert_eq!(seen[0].authorization, None);
    assert_eq!(seen[0].body, client().refresh_body(REFRESH_TOKEN));
}

#[tokio::test]
async fn a_revoked_refresh_token_is_revoked_whatever_the_status() {
    for status in [400, 401, 403] {
        let http = Stub::answering(vec![Ok(answered(
            status,
            r#"{"error":"invalid_grant","error_description":"refresh token expired"}"#,
        ))]);
        let error = client().refresh(&http, REFRESH_TOKEN).await.unwrap_err();

        assert_eq!(error, RefreshError::Revoked, "status {status}");
    }
}

#[tokio::test]
async fn a_transport_failure_is_transient() {
    let http = Stub::answering(vec![Err(HttpError::Timeout)]);
    let error = client().refresh(&http, REFRESH_TOKEN).await.unwrap_err();

    assert_eq!(
        error,
        RefreshError::Transient(String::from("the request timed out"))
    );
}

#[tokio::test]
async fn a_server_error_is_transient_and_names_the_status() {
    let http = Stub::answering(vec![Ok(answered(503, "<html>down</html>"))]);
    let error = client().refresh(&http, REFRESH_TOKEN).await.unwrap_err();

    assert!(matches!(error, RefreshError::Transient(_)), "{error:?}");
    assert!(error.to_string().contains("503"), "{error}");
}

#[tokio::test]
async fn a_refusal_for_another_reason_is_transient() {
    let http = Stub::answering(vec![Ok(answered(400, r#"{"error":"invalid_request"}"#))]);
    let error = client().refresh(&http, REFRESH_TOKEN).await.unwrap_err();

    assert!(matches!(error, RefreshError::Transient(_)), "{error:?}");
}

#[tokio::test]
async fn a_2xx_body_without_a_token_is_transient() {
    let http = Stub::answering(vec![Ok(answered(200, r#"{"expires_in":10}"#))]);
    let error = client().refresh(&http, REFRESH_TOKEN).await.unwrap_err();

    assert!(matches!(error, RefreshError::Transient(_)), "{error:?}");
}

#[tokio::test]
async fn a_refused_status_wins_over_a_body_that_looks_like_a_grant() {
    let http = Stub::answering(vec![Ok(answered(500, REFRESHED))]);
    let error = client().refresh(&http, REFRESH_TOKEN).await.unwrap_err();

    assert!(matches!(error, RefreshError::Transient(_)), "{error:?}");
}

#[test]
fn a_renewed_pair_takes_the_new_tokens_and_the_new_expiry() {
    let token = TokenResponse::parse(REFRESHED.as_bytes()).unwrap();
    let later = Millis(NOW.0 + 1_000);

    let renewed = expiring().renew(&token, later);

    assert_eq!(renewed.app_user_id, "app-user-1");
    assert_eq!(renewed.access_token, "access-2");
    assert_eq!(renewed.refresh_token.as_deref(), Some("refresh-2"));
    assert_eq!(renewed.expires_at, Millis(later.0 + 86_399_000));
    assert_eq!(renewed.scopes.to_string(), "read,write");
    assert_eq!(renewed.installed_at, NOW);
}

#[test]
fn a_renewal_that_omits_the_refresh_token_keeps_the_stored_one() {
    let token = TokenResponse::parse(br#"{"access_token":"access-2","expires_in":10}"#).unwrap();

    let renewed = installation().renew(&token, NOW);

    assert_eq!(renewed.refresh_token.as_deref(), Some(REFRESH_TOKEN));
}

#[test]
fn a_renewal_that_omits_the_scopes_keeps_the_stored_grant() {
    let token = TokenResponse::parse(br#"{"access_token":"access-2","expires_in":10}"#).unwrap();

    let renewed = installation().renew(&token, NOW);

    assert_eq!(renewed.scopes.to_string(), "read,write,app:assignable");
}

#[test]
fn the_remaining_lifetime_floors_and_never_goes_negative() {
    let pair = installation();

    assert_eq!(
        pair.remaining_seconds(NOW),
        LIFETIME,
        "a whole second of slack, not the fractional remainder"
    );
    assert_eq!(pair.remaining_seconds(Millis(NOW.0 + 999)), LIFETIME - 1);
    assert_eq!(pair.remaining_seconds(pair.expires_at), 0);
    assert_eq!(
        pair.remaining_seconds(Millis(pair.expires_at.0 + 60_000)),
        0
    );
}

#[test]
fn a_pair_far_from_expiry_is_used_as_it_is() {
    let mut slot = Slot::Fresh(installation());

    assert_eq!(slot.step(NOW), Step::Use(installation()));
    assert_eq!(slot, Slot::Fresh(installation()));
}

#[test]
fn a_pair_inside_the_skew_asks_its_first_caller_to_refresh() {
    let mut slot = Slot::Fresh(expiring());

    assert_eq!(slot.step(NOW), Step::Refresh(expiring()));
    assert_eq!(slot, Slot::Refreshing(expiring()));
}

#[test]
fn a_pair_at_the_skew_boundary_is_not_refreshed_early() {
    let mut pair = installation();
    pair.expires_at = Millis(NOW.0 + REFRESH_SKEW.0 + 1);
    let mut slot = Slot::Fresh(pair.clone());

    assert_eq!(slot.step(NOW), Step::Use(pair));

    let mut at_boundary = installation();
    at_boundary.expires_at = Millis(NOW.0 + REFRESH_SKEW.0);
    assert_eq!(
        Slot::Fresh(at_boundary.clone()).step(NOW),
        Step::Refresh(at_boundary)
    );
}

#[test]
fn an_expired_pair_refreshes_rather_than_failing() {
    let mut pair = installation();
    pair.expires_at = Millis(NOW.0 - 3_600_000);
    let mut slot = Slot::Fresh(pair.clone());

    assert_eq!(slot.step(NOW), Step::Refresh(pair));
}

#[test]
fn a_clock_that_jumps_past_the_expiry_between_calls_refreshes() {
    let mut slot = Slot::Fresh(installation());
    assert!(matches!(slot.step(NOW), Step::Use(_)));

    let jumped = Millis(NOW.0 + 86_399_000);
    assert_eq!(slot.step(jumped), Step::Refresh(installation()));
}

#[test]
fn a_second_caller_while_refreshing_is_told_to_wait() {
    let mut slot = Slot::Fresh(expiring());
    assert!(matches!(slot.step(NOW), Step::Refresh(_)));

    assert_eq!(slot.step(NOW), Step::Wait);
    assert_eq!(slot.step(Millis(NOW.0 + 60_000)), Step::Wait);
}

#[test]
fn a_refreshed_pair_serves_the_next_caller() {
    let mut slot = Slot::Fresh(expiring());
    assert!(matches!(slot.step(NOW), Step::Refresh(_)));

    slot.store(installation());

    assert_eq!(slot.step(NOW), Step::Use(installation()));
}

#[test]
fn a_revoked_slot_stays_absent() {
    let mut slot = Slot::Fresh(expiring());
    slot.revoked();

    assert_eq!(slot, Slot::Missing);
    assert_eq!(slot.step(NOW), Step::Absent);
    assert_eq!(slot.step(Millis(NOW.0 + 86_400_000)), Step::Absent);
}

#[test]
fn a_missing_slot_never_asks_for_a_refresh() {
    let mut slot = Slot::Missing;

    assert_eq!(slot.step(NOW), Step::Absent);
    assert_eq!(slot, Slot::Missing);
}

#[test]
fn a_stored_installation_is_a_fresh_slot() {
    assert_eq!(
        Slot::from(Some(installation())),
        Slot::Fresh(installation())
    );
    assert_eq!(Slot::from(None), Slot::Missing);
}

#[test]
fn a_refresh_error_says_whether_a_retry_could_work() {
    assert_eq!(
        RefreshError::Revoked.to_string(),
        "Linear withdrew the refresh token"
    );
    assert_eq!(
        RefreshError::Transient(String::from("the request timed out")).to_string(),
        "the token refresh failed: the request timed out"
    );
}

#[test]
fn scopes_report_emptiness_and_keep_their_order() {
    assert!(Scopes::parse("").is_empty());
    assert!(!Scopes::parse("read").is_empty());
    assert_eq!(
        Scopes::parse("write read app:unknown")
            .tokens()
            .collect::<Vec<_>>(),
        vec!["write", "read", "app:unknown"]
    );
}
