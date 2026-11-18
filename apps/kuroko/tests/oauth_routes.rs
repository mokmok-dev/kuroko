#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use kuroko::router::{AppState, Installations, Oauth, router};
use kuroko_core::{Book, Clock, Dispatch, Done, Journal, JournalError, Limits, Millis};
use kuroko_engine::{BoxFuture, Transport, spawn};
use kuroko_linear::oauth::{
    BoxFuture as HttpFuture, HttpError, HttpPost, HttpResponse, INSTALL_STATE_TTL, InstallError,
    Installation, OauthClient, PostRequest, Secret, StateStore,
};
use kuroko_linear::{Gate, PromptTemplate, TriggerConfig, WebhookSecret};
use tower::ServiceExt;

const SECRET: &str = "whsec_app_tests";
const INSTALL_SECRET: &str = "install-secret-1";
const CLIENT_ID: &str = "client-abc";
const CLIENT_SECRET: &str = "client-secret-xyz";
const REDIRECT: &str = "https://kuroko.example/linear/oauth/callback";
const NOW: Millis = Millis(1_700_000_000_000);
const WINDOW: Millis = Millis(60_000);
const TOKEN_OK: &str = r#"{"access_token":"access-1","refresh_token":"refresh-1","expires_in":86399,"token_type":"Bearer","scope":"read write app:assignable app:mentionable"}"#;
const VIEWER_OK: &str = r#"{"data":{"viewer":{"id":"app-user-1"}}}"#;

/// A clock a test can move forward, so state expiry is exercised without waiting.
struct MovingClock(AtomicI64);

impl MovingClock {
    fn at(now: Millis) -> Arc<Self> {
        Arc::new(Self(AtomicI64::new(now.0)))
    }

    fn advance(&self, by: Millis) {
        self.0.fetch_add(by.0, Ordering::SeqCst);
    }
}

impl Clock for MovingClock {
    fn now_ms(&self) -> Millis {
        Millis(self.0.load(Ordering::SeqCst))
    }
}

struct NoopJournal;

impl Journal for NoopJournal {
    fn persist(&self, _book: &Book) -> Result<(), JournalError> {
        Ok(())
    }
}

/// Holds every dispatch forever, so `/tasks` answers without racing a timer.
struct Holding;

impl Transport for Holding {
    fn run<'a>(&'a self, _job: &'a Dispatch) -> BoxFuture<'a, Done> {
        Box::pin(std::future::pending::<Done>())
    }
}

/// The stub exchange: records each request and answers from a script.
struct StubHttp {
    seen: Mutex<Vec<Seen>>,
    answers: Mutex<Vec<Result<HttpResponse, HttpError>>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Seen {
    url: String,
    authorization: Option<String>,
    body: String,
}

impl StubHttp {
    fn answering(answers: Vec<Result<HttpResponse, HttpError>>) -> Arc<Self> {
        Arc::new(Self {
            seen: Mutex::new(Vec::new()),
            answers: Mutex::new(answers),
        })
    }

    fn requests(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

impl HttpPost for StubHttp {
    fn post<'a>(
        &'a self,
        request: PostRequest<'a>,
    ) -> HttpFuture<'a, Result<HttpResponse, HttpError>> {
        self.seen.lock().unwrap().push(Seen {
            url: String::from(request.url),
            authorization: request.authorization.map(str::to_owned),
            body: String::from_utf8_lossy(request.body).into_owned(),
        });
        let answer = self.answers.lock().unwrap().remove(0);
        Box::pin(async move { answer })
    }
}

/// Counts writes and keeps the last installation, so a test can tell "one row"
/// from "no row".
#[derive(Default)]
struct CountingStore {
    saves: AtomicUsize,
    saved: Mutex<Option<Installation>>,
}

impl CountingStore {
    fn saves(&self) -> usize {
        self.saves.load(Ordering::SeqCst)
    }

    fn saved(&self) -> Option<Installation> {
        self.saved.lock().unwrap().clone()
    }
}

impl Installations for CountingStore {
    fn save(&self, installation: &Installation) -> Result<(), String> {
        self.saves.fetch_add(1, Ordering::SeqCst);
        *self.saved.lock().unwrap() = Some(installation.clone());
        Ok(())
    }
}

struct Fixture {
    app: Router,
    clock: Arc<MovingClock>,
    http: Arc<StubHttp>,
    store: Arc<CountingStore>,
}

impl Fixture {
    fn with_answers(answers: Vec<Result<HttpResponse, HttpError>>) -> Self {
        let clock = MovingClock::at(NOW);
        let as_clock: Arc<dyn Clock> = Arc::clone(&clock) as Arc<dyn Clock>;
        let limits = Limits {
            in_flight: NonZeroUsize::new(1).unwrap(),
            max_queued: NonZeroUsize::new(64).unwrap(),
        };
        let (engine, task) = spawn(
            Arc::new(Holding),
            Book::new(limits),
            Arc::clone(&as_clock),
            Arc::new(NoopJournal),
        );
        // The engine task outlives the router in these tests; leaking the handle
        // keeps the engine's command channel open for the whole test.
        std::mem::forget(task);
        let gate = Gate::new(
            WebhookSecret::new(SECRET),
            WINDOW,
            TriggerConfig {
                any_delegate: true,
                ..TriggerConfig::default()
            },
            PromptTemplate::new("work"),
        );
        let http = StubHttp::answering(answers);
        let store = Arc::new(CountingStore::default());
        let oauth = Oauth {
            client: OauthClient {
                client_id: String::from(CLIENT_ID),
                client_secret: Secret::new(CLIENT_SECRET),
                redirect_uri: String::from(REDIRECT),
            },
            install_secret: Secret::new(INSTALL_SECRET),
            http: Arc::clone(&http) as Arc<dyn HttpPost>,
            states: StateStore::new(),
        };
        let state = AppState::new(gate, Arc::new(engine), as_clock)
            .with_oauth(oauth)
            .with_store(Arc::clone(&store) as Arc<dyn Installations>);
        Self {
            app: router(state),
            clock,
            http,
            store,
        }
    }

    fn ok_install() -> Self {
        Self::with_answers(vec![Ok(answer(200, TOKEN_OK)), Ok(answer(200, VIEWER_OK))])
    }

    async fn get(&self, uri: &str) -> (StatusCode, Option<String>, String) {
        let request = Request::builder().uri(uri).body(Body::empty()).unwrap();
        self.send(request).await
    }

    async fn get_with(
        &self,
        uri: &str,
        name: &str,
        value: &str,
    ) -> (StatusCode, Option<String>, String) {
        let request = Request::builder()
            .uri(uri)
            .header(name, value)
            .body(Body::empty())
            .unwrap();
        self.send(request).await
    }

    async fn send(&self, request: Request<Body>) -> (StatusCode, Option<String>, String) {
        let response = self.app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let location = response
            .headers()
            .get(header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            location,
            String::from_utf8_lossy(&body).into_owned(),
        )
    }

    /// Starts an install with the right secret and returns the minted state.
    async fn start(&self) -> String {
        let (status, location, _) = self
            .get(&format!("/linear/install?install_secret={INSTALL_SECRET}"))
            .await;
        assert_eq!(status, StatusCode::FOUND);
        state_of(&location.unwrap())
    }

    /// The callback path for `state`.
    fn callback(state: &str) -> String {
        format!("/linear/oauth/callback?code=code-1&state={state}")
    }
}

fn answer(status: u16, body: &str) -> HttpResponse {
    HttpResponse {
        status,
        body: body.as_bytes().to_vec(),
    }
}

/// Reads `state` out of an authorize URL, so a test never hardcodes a random.
fn state_of(location: &str) -> String {
    location
        .split(['?', '&'])
        .find_map(|pair| pair.strip_prefix("state="))
        .expect("the authorize URL carries a state")
        .to_owned()
}

#[tokio::test]
async fn install_without_a_secret_is_unauthorized() {
    let fixture = Fixture::ok_install();
    let (status, location, body) = fixture.get("/linear/install").await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(location, None);
    assert!(body.contains("Authorization: Bearer"), "{body}");
    assert_eq!(fixture.http.requests(), vec![]);
}

#[tokio::test]
async fn install_with_a_wrong_secret_of_the_same_length_is_unauthorized() {
    let fixture = Fixture::ok_install();
    let wrong = "x".repeat(INSTALL_SECRET.len());
    let query = fixture
        .get(&format!("/linear/install?install_secret={wrong}"))
        .await;
    let header = fixture
        .get_with(
            "/linear/install",
            header::AUTHORIZATION.as_str(),
            &format!("Bearer {wrong}"),
        )
        .await;

    assert_eq!(query.0, StatusCode::UNAUTHORIZED);
    assert_eq!(header.0, StatusCode::UNAUTHORIZED);
    assert_eq!(query.1, None);
    assert_eq!(header.1, None);
}

#[tokio::test]
async fn install_with_a_wrong_secret_of_a_different_length_is_unauthorized() {
    let fixture = Fixture::ok_install();
    let (status, location, _) = fixture
        .get(&format!("/linear/install?install_secret={}", "x".repeat(3)))
        .await;
    let (header_status, header_location, _) = fixture
        .get_with(
            "/linear/install",
            header::AUTHORIZATION.as_str(),
            "Bearer x",
        )
        .await;
    let longer = format!("{INSTALL_SECRET}-longer");
    let (long_status, _, _) = fixture
        .get(&format!("/linear/install?install_secret={longer}"))
        .await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(header_status, StatusCode::UNAUTHORIZED);
    assert_eq!(long_status, StatusCode::UNAUTHORIZED);
    assert_eq!(location, None);
    assert_eq!(header_location, None);
}

#[tokio::test]
async fn a_bearer_header_that_is_not_the_scheme_is_unauthorized() {
    let fixture = Fixture::ok_install();
    let (status, _, _) = fixture
        .get_with(
            "/linear/install",
            header::AUTHORIZATION.as_str(),
            INSTALL_SECRET,
        )
        .await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn the_query_parameter_starts_an_install() {
    let fixture = Fixture::ok_install();
    let (status, location, _) = fixture
        .get(&format!("/linear/install?install_secret={INSTALL_SECRET}"))
        .await;

    assert_eq!(status, StatusCode::FOUND);
    assert!(
        location
            .unwrap()
            .starts_with("https://linear.app/oauth/authorize?"),
    );
}

#[tokio::test]
async fn the_authorize_redirect_carries_every_documented_parameter() {
    let fixture = Fixture::ok_install();
    let (_, location, _) = fixture
        .get(&format!("/linear/install?install_secret={INSTALL_SECRET}"))
        .await;
    let location = location.unwrap();

    for expected in [
        "actor=app",
        "response_type=code",
        "scope=read%2Cwrite%2Capp%3Aassignable%2Capp%3Amentionable",
        "client_id=client-abc",
        "redirect_uri=https%3A%2F%2Fkuroko.example%2Flinear%2Foauth%2Fcallback",
    ] {
        assert!(
            location.contains(expected),
            "{expected} missing from {location}"
        );
    }
    assert_eq!(state_of(&location).len(), 43);
}

#[tokio::test]
async fn every_install_mints_a_different_state() {
    let fixture = Fixture::ok_install();
    let (_, first, _) = fixture
        .get(&format!("/linear/install?install_secret={INSTALL_SECRET}"))
        .await;
    let (_, second, _) = fixture
        .get(&format!("/linear/install?install_secret={INSTALL_SECRET}"))
        .await;

    assert_ne!(state_of(&first.unwrap()), state_of(&second.unwrap()));
}

#[tokio::test]
async fn the_install_routes_are_not_found_without_an_oauth_config() {
    let app = without_oauth();
    let mut statuses = Vec::new();
    for uri in [
        "/linear/install",
        &format!("/linear/install?install_secret={INSTALL_SECRET}"),
        "/linear/oauth/callback?code=c&state=s",
    ] {
        let response = app
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        statuses.push(response.status());
    }

    assert_eq!(
        statuses,
        vec![
            StatusCode::NOT_FOUND,
            StatusCode::NOT_FOUND,
            StatusCode::NOT_FOUND
        ]
    );
}

#[tokio::test]
async fn webhooks_still_work_without_an_oauth_config() {
    let app = without_oauth();
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/webhooks/linear")
                .header("content-type", "application/json")
                .body(Body::from(Vec::new()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_callback_with_an_unknown_state_is_unauthorized() {
    let fixture = Fixture::ok_install();
    let (status, _, body) = fixture
        .get("/linear/oauth/callback?code=code-1&state=unknown")
        .await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(body.contains("unknown or expired"), "{body}");
    assert_eq!(fixture.http.requests(), vec![]);
    assert_eq!(fixture.store.saves(), 0);
}

#[tokio::test]
async fn a_callback_with_a_blank_state_is_unauthorized() {
    let fixture = Fixture::ok_install();
    let (status, _, _) = fixture
        .get("/linear/oauth/callback?code=code-1&state=")
        .await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(fixture.store.saves(), 0);
}

#[tokio::test]
async fn a_replayed_state_is_unauthorized_and_writes_no_second_row() {
    let fixture = Fixture::ok_install();
    let state = fixture.start().await;
    let path = Fixture::callback(&state);

    assert_eq!(fixture.get(&path).await.0, StatusCode::OK);
    let replay = fixture.get(&path).await;

    assert_eq!(replay.0, StatusCode::UNAUTHORIZED);
    assert_eq!(fixture.store.saves(), 1);
    assert_eq!(fixture.http.requests().len(), 2);
}

#[tokio::test]
async fn a_replayed_state_leaves_the_first_installation_untouched() {
    let fixture = Fixture::with_answers(vec![
        Ok(answer(200, TOKEN_OK)),
        Ok(answer(200, VIEWER_OK)),
        Ok(answer(
            200,
            r#"{"access_token":"access-2","expires_in":1,"scope":"read"}"#,
        )),
        Ok(answer(200, r#"{"data":{"viewer":{"id":"app-user-2"}}}"#)),
    ]);
    let state = fixture.start().await;
    let path = Fixture::callback(&state);
    assert_eq!(fixture.get(&path).await.0, StatusCode::OK);
    let first = fixture.store.saved().unwrap();

    assert_eq!(fixture.get(&path).await.0, StatusCode::UNAUTHORIZED);

    assert_eq!(fixture.store.saves(), 1);
    assert_eq!(fixture.store.saved().unwrap(), first);
    assert_eq!(first.app_user_id, "app-user-1");
}

#[tokio::test]
async fn a_state_that_expired_before_the_callback_is_unauthorized() {
    let fixture = Fixture::ok_install();
    let state = fixture.start().await;
    fixture.clock.advance(Millis(INSTALL_STATE_TTL.0 + 1));

    let (status, _, body) = fixture.get(&Fixture::callback(&state)).await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(body.contains("unknown or expired"), "{body}");
    assert_eq!(fixture.http.requests(), vec![]);
    assert_eq!(fixture.store.saves(), 0);
}

#[tokio::test]
async fn a_callback_with_an_error_parameter_names_it() {
    let fixture = Fixture::ok_install();
    let state = fixture.start().await;
    let (status, _, body) = fixture
        .get(&format!(
            "/linear/oauth/callback?error=access_denied&state={state}"
        ))
        .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("access_denied"), "{body}");
    assert_eq!(fixture.http.requests(), vec![]);
    assert_eq!(fixture.store.saves(), 0);
}

#[tokio::test]
async fn an_error_parameter_wins_over_a_code_and_leaves_the_state_redeemable() {
    let fixture = Fixture::ok_install();
    let state = fixture.start().await;
    let refused = fixture
        .get(&format!(
            "/linear/oauth/callback?error=access_denied&code=code-1&state={state}"
        ))
        .await;

    assert_eq!(refused.0, StatusCode::BAD_REQUEST);
    assert_eq!(fixture.store.saves(), 0);
    assert_eq!(
        fixture.get(&Fixture::callback(&state)).await.0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn a_callback_without_a_code_or_state_is_a_bad_request() {
    let fixture = Fixture::ok_install();
    let state = fixture.start().await;

    let no_state = fixture.get("/linear/oauth/callback?code=code-1").await;
    let no_code = fixture
        .get(&format!("/linear/oauth/callback?state={state}"))
        .await;

    assert_eq!(no_state.0, StatusCode::BAD_REQUEST);
    assert_eq!(no_code.0, StatusCode::BAD_REQUEST);
    assert_eq!(fixture.store.saves(), 0);
}

#[tokio::test]
async fn a_failed_exchange_writes_no_row() {
    let fixture = Fixture::with_answers(vec![Ok(answer(400, r#"{"error":"invalid_grant"}"#))]);
    let state = fixture.start().await;
    let (status, _, body) = fixture.get(&Fixture::callback(&state)).await;

    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(body.contains("invalid_grant"), "{body}");
    assert_eq!(fixture.store.saves(), 0);
}

#[tokio::test]
async fn an_unreadable_refusal_names_the_status_and_writes_no_row() {
    let fixture = Fixture::with_answers(vec![Ok(answer(502, "<html>bad gateway</html>"))]);
    let state = fixture.start().await;
    let (status, _, body) = fixture.get(&Fixture::callback(&state)).await;

    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(body.contains("status 502"), "{body}");
    assert_eq!(fixture.store.saves(), 0);
}

#[tokio::test]
async fn a_timed_out_exchange_names_the_timeout_and_writes_no_row() {
    let fixture = Fixture::with_answers(vec![Err(HttpError::Timeout)]);
    let state = fixture.start().await;
    let (status, _, body) = fixture.get(&Fixture::callback(&state)).await;

    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(body.contains("timed out"), "{body}");
    assert_eq!(fixture.store.saves(), 0);
}

#[tokio::test]
async fn a_timed_out_viewer_query_names_the_timeout_and_writes_no_row() {
    let fixture = Fixture::with_answers(vec![Ok(answer(200, TOKEN_OK)), Err(HttpError::Timeout)]);
    let state = fixture.start().await;
    let (status, _, body) = fixture.get(&Fixture::callback(&state)).await;

    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(body.contains("timed out"), "{body}");
    assert_eq!(fixture.store.saves(), 0);
}

#[tokio::test]
async fn a_viewer_query_without_an_id_writes_no_row() {
    let fixture = Fixture::with_answers(vec![
        Ok(answer(200, TOKEN_OK)),
        Ok(answer(200, r#"{"data":{"viewer":{}}}"#)),
    ]);
    let state = fixture.start().await;
    let (status, _, body) = fixture.get(&Fixture::callback(&state)).await;

    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(body.contains("no viewer id"), "{body}");
    assert_eq!(fixture.store.saves(), 0);
}

#[tokio::test]
async fn a_graphql_errors_array_writes_no_row() {
    let fixture = Fixture::with_answers(vec![
        Ok(answer(200, TOKEN_OK)),
        Ok(answer(200, r#"{"errors":[{"message":"nope"}]}"#)),
    ]);
    let state = fixture.start().await;
    let (status, _, body) = fixture.get(&Fixture::callback(&state)).await;

    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(body.contains("nope"), "{body}");
    assert_eq!(fixture.store.saves(), 0);
}

#[tokio::test]
async fn a_successful_install_writes_one_row_and_names_the_app_user() {
    let fixture = Fixture::ok_install();
    let state = fixture.start().await;
    let (status, _, body) = fixture.get(&Fixture::callback(&state)).await;

    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("app-user-1"), "{body}");
    assert!(body.contains("installed in Linear"), "{body}");
    assert!(!body.contains("access-1"), "{body}");
    assert!(!body.contains("refresh-1"), "{body}");
    assert_eq!(fixture.store.saves(), 1);

    let installation = fixture.store.saved().unwrap();
    assert_eq!(installation.app_user_id, "app-user-1");
    assert_eq!(installation.access_token, "access-1");
    assert_eq!(installation.refresh_token.as_deref(), Some("refresh-1"));
    assert_eq!(installation.expires_at, Millis(NOW.0 + 86_399_000));
    assert_eq!(installation.installed_at, NOW);
    assert_eq!(
        installation.scopes.to_string(),
        "read,write,app:assignable,app:mentionable"
    );
}

#[tokio::test]
async fn the_exchange_sends_the_code_and_the_registration() {
    let fixture = Fixture::ok_install();
    let state = fixture.start().await;
    fixture.get(&Fixture::callback(&state)).await;

    let requests = fixture.http.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].url, "https://api.linear.app/oauth/token");
    assert_eq!(
        requests[0].body,
        "code=code-1&redirect_uri=https%3A%2F%2Fkuroko.example%2Flinear%2Foauth%2Fcallback&client_id=client-abc&client_secret=client-secret-xyz&grant_type=authorization_code"
    );
    assert_eq!(requests[1].url, "https://api.linear.app/graphql");
    assert_eq!(
        requests[1].authorization.as_deref(),
        Some("Bearer access-1")
    );
}

#[tokio::test]
async fn a_callback_without_a_store_writes_nothing_and_says_so() {
    let app = without_oauth();
    let response = app
        .oneshot(
            Request::builder()
                .uri("/linear/oauth/callback?code=c&state=s")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_daemon_still_answers_tasks_after_a_failed_install() {
    let fixture = Fixture::with_answers(vec![Err(HttpError::Timeout)]);
    let state = fixture.start().await;
    assert_eq!(
        fixture.get(&Fixture::callback(&state)).await.0,
        StatusCode::BAD_GATEWAY
    );

    let (status, _, body) = fixture.get("/tasks").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "[]");
}

#[tokio::test]
async fn the_daemon_still_answers_tasks_after_a_refused_install() {
    let fixture = Fixture::ok_install();
    assert_eq!(
        fixture.get("/linear/install").await.0,
        StatusCode::UNAUTHORIZED
    );

    let (status, _, body) = fixture.get("/tasks").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "[]");
}

#[tokio::test]
async fn a_save_failure_is_an_internal_error() {
    struct Failing;
    impl Installations for Failing {
        fn save(&self, _installation: &Installation) -> Result<(), String> {
            Err(String::from("disk full"))
        }
    }

    let clock = MovingClock::at(NOW);
    let as_clock: Arc<dyn Clock> = Arc::clone(&clock) as Arc<dyn Clock>;
    let limits = Limits {
        in_flight: NonZeroUsize::new(1).unwrap(),
        max_queued: NonZeroUsize::new(64).unwrap(),
    };
    let (engine, task) = spawn(
        Arc::new(Holding),
        Book::new(limits),
        Arc::clone(&as_clock),
        Arc::new(NoopJournal),
    );
    std::mem::forget(task);
    let gate = Gate::new(
        WebhookSecret::new(SECRET),
        WINDOW,
        TriggerConfig::default(),
        PromptTemplate::new("work"),
    );
    let http = StubHttp::answering(vec![Ok(answer(200, TOKEN_OK)), Ok(answer(200, VIEWER_OK))]);
    let oauth = Oauth {
        client: OauthClient {
            client_id: String::from(CLIENT_ID),
            client_secret: Secret::new(CLIENT_SECRET),
            redirect_uri: String::from(REDIRECT),
        },
        install_secret: Secret::new(INSTALL_SECRET),
        http: Arc::clone(&http) as Arc<dyn HttpPost>,
        states: StateStore::new(),
    };
    let state = AppState::new(gate, Arc::new(engine), as_clock)
        .with_oauth(oauth)
        .with_store(Arc::new(Failing));
    let app = router(state);
    let fixture = Fixture {
        app,
        clock,
        http,
        store: Arc::new(CountingStore::default()),
    };
    let state = fixture.start().await;

    let (status, _, body) = fixture.get(&Fixture::callback(&state)).await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(body.contains("could not save"), "{body}");
}

#[tokio::test]
async fn an_install_error_never_reaches_the_page() {
    let fixture = Fixture::with_answers(vec![
        Ok(answer(
            200,
            r#"{"access_token":"super-secret-token","expires_in":1}"#,
        )),
        Ok(answer(500, "boom")),
    ]);
    let state = fixture.start().await;
    let (_, _, body) = fixture.get(&Fixture::callback(&state)).await;

    assert!(!body.contains("super-secret-token"), "{body}");
}

#[tokio::test]
async fn the_install_error_enum_stays_redacted() {
    let error = InstallError::TokenStatus(401);

    assert_eq!(
        error.to_string(),
        "the token endpoint answered with status 401"
    );
}

/// A router with a working engine but no `[linear.oauth]`, which is what a daemon
/// booted without an OAuth app looks like.
fn without_oauth() -> Router {
    let clock: Arc<dyn Clock> = MovingClock::at(NOW);
    let limits = Limits {
        in_flight: NonZeroUsize::new(1).unwrap(),
        max_queued: NonZeroUsize::new(64).unwrap(),
    };
    let (engine, task) = spawn(
        Arc::new(Holding),
        Book::new(limits),
        Arc::clone(&clock),
        Arc::new(NoopJournal),
    );
    std::mem::forget(task);
    let gate = Gate::new(
        WebhookSecret::new(SECRET),
        WINDOW,
        TriggerConfig::default(),
        PromptTemplate::new("work"),
    );
    router(AppState::new(gate, Arc::new(engine), clock))
}
