#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::VecDeque;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use kuroko::linear::{TokenFailure, TokenProvider};
use kuroko::router::{AppState, Installations, Oauth, router};
use kuroko_core::{Book, Clock, Dispatch, Done, Journal, JournalError, Limits, Millis};
use kuroko_engine::{BoxFuture, Transport, spawn};
use kuroko_linear::oauth::{
    BoxFuture as HttpFuture, HttpError, HttpPost, HttpResponse, Installation, OauthClient,
    PostRequest, REFRESH_SKEW, Scopes, Secret, StateStore, Token,
};
use kuroko_linear::{Gate, PromptTemplate, TriggerConfig, WebhookSecret};
use serde_json::Value;
use tokio::sync::Semaphore;
use tokio::time::timeout;
use tower::ServiceExt;

const NOW: Millis = Millis(1_700_000_000_000);
const CLIENT_ID: &str = "client-abc";
const CLIENT_SECRET: &str = "client-secret-xyz";
const REDIRECT: &str = "https://kuroko.example/linear/oauth/callback";
const ACCESS: &str = "access-1";
const REFRESHED_ACCESS: &str = "access-2";
const REFRESH_TOKEN: &str = "refresh-1";
const REFRESHED: &str = r#"{"access_token":"access-2","refresh_token":"refresh-2","expires_in":86399,"token_type":"Bearer","scope":"read write app:assignable"}"#;
const INVALID_GRANT: &str = r#"{"error":"invalid_grant","error_description":"already used"}"#;

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

/// Records each refresh request, counts them, and can hold one open so a test
/// can pile callers up behind it.
struct StubHttp {
    seen: Mutex<Vec<String>>,
    answers: Mutex<VecDeque<Result<HttpResponse, HttpError>>>,
    /// Released when a request arrives, so a test knows a refresh is in flight.
    arrived: Arc<Semaphore>,
    /// Acquired before answering, so a test decides when a refresh finishes.
    held: Arc<Semaphore>,
    holding: bool,
}

impl StubHttp {
    fn answering(answers: Vec<Result<HttpResponse, HttpError>>) -> Arc<Self> {
        Arc::new(Self {
            seen: Mutex::new(Vec::new()),
            answers: Mutex::new(answers.into()),
            arrived: Arc::new(Semaphore::new(0)),
            held: Arc::new(Semaphore::new(0)),
            holding: false,
        })
    }

    /// Holds the first request until the returned permit is released.
    fn holding() -> Arc<Self> {
        Arc::new(Self {
            seen: Mutex::new(Vec::new()),
            answers: Mutex::new(VecDeque::new()),
            arrived: Arc::new(Semaphore::new(0)),
            held: Arc::new(Semaphore::new(0)),
            holding: true,
        })
    }

    fn calls(&self) -> usize {
        self.seen.lock().unwrap().len()
    }

    fn bodies(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }

    /// Answers the request currently being held, releasing `permits` of them.
    ///
    /// More than one permit lets a broken coalescer finish instead of hanging, so
    /// the call count speaks for itself.
    fn answer_held(&self, answer: &Result<HttpResponse, HttpError>, permits: usize) {
        for _ in 0..permits {
            self.answers.lock().unwrap().push_back(answer.clone());
        }
        self.held.add_permits(permits);
    }
}

impl HttpPost for StubHttp {
    fn post<'a>(
        &'a self,
        request: PostRequest<'a>,
    ) -> HttpFuture<'a, Result<HttpResponse, HttpError>> {
        self.seen
            .lock()
            .unwrap()
            .push(String::from_utf8_lossy(request.body).into_owned());
        Box::pin(async move {
            if self.holding {
                self.arrived.add_permits(1);
                let permit = self
                    .held
                    .acquire()
                    .await
                    .map_err(|_| HttpError::Other(String::from("the test closed the gate")))?;
                permit.forget();
            }
            self.answers
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Err(HttpError::Other(String::from("no scripted answer"))))
        })
    }
}

/// Counts writes and keeps the last pair, so a test can tell "persisted" from
/// "returned only".
#[derive(Default)]
struct CountingStore {
    saves: AtomicUsize,
    saved: Mutex<Option<Installation>>,
    fail: bool,
}

impl CountingStore {
    fn saves(&self) -> usize {
        self.saves.load(Ordering::SeqCst)
    }

    fn saved(&self) -> Option<Installation> {
        self.saved.lock().unwrap().clone()
    }

    fn failing() -> Arc<Self> {
        Arc::new(Self {
            fail: true,
            ..Self::default()
        })
    }
}

impl Installations for CountingStore {
    fn save(&self, installation: &Installation) -> Result<(), String> {
        if self.fail {
            return Err(String::from("disk full"));
        }
        self.saves.fetch_add(1, Ordering::SeqCst);
        *self.saved.lock().unwrap() = Some(installation.clone());
        Ok(())
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
        access_token: Token::new(ACCESS),
        refresh_token: Some(Token::new(REFRESH_TOKEN)),
        expires_at: Millis(NOW.0 + 86_399_000),
        scopes: Scopes::parse("read write app:assignable"),
        installed_at: NOW,
    }
}

/// A pair whose access token is inside the refresh skew.
fn expiring() -> Installation {
    let mut pair = installation();
    pair.expires_at = Millis(NOW.0 + REFRESH_SKEW.0);
    pair
}

struct Provider {
    tokens: Arc<TokenProvider>,
    clock: Arc<MovingClock>,
    http: Arc<StubHttp>,
    store: Arc<CountingStore>,
}

impl Provider {
    fn with(held: Installation, http: Arc<StubHttp>, store: Arc<CountingStore>) -> Self {
        let clock = MovingClock::at(NOW);
        let tokens = TokenProvider::new(
            OauthClient {
                client_id: String::from(CLIENT_ID),
                client_secret: Secret::new(CLIENT_SECRET),
                redirect_uri: String::from(REDIRECT),
            },
            Arc::clone(&http) as Arc<dyn HttpPost>,
            Arc::clone(&store) as Arc<dyn Installations>,
            Arc::clone(&clock) as Arc<dyn Clock>,
            Some(held),
        );
        Self {
            tokens: Arc::new(tokens),
            clock,
            http,
            store,
        }
    }

    fn answering(held: Installation, answers: Vec<Result<HttpResponse, HttpError>>) -> Self {
        Self::with(
            held,
            StubHttp::answering(answers),
            Arc::new(CountingStore::default()),
        )
    }

    async fn token(&self) -> Result<Installation, TokenFailure> {
        self.tokens.access_token().await
    }
}

#[tokio::test]
async fn a_token_far_from_expiry_is_returned_without_a_refresh() {
    let fixture = Provider::answering(installation(), vec![]);

    let pair = fixture.token().await.unwrap();

    assert_eq!(pair.access_token.expose(), ACCESS);
    assert_eq!(fixture.http.calls(), 0);
    assert_eq!(fixture.store.saves(), 0);
}

#[tokio::test]
async fn a_token_inside_the_skew_refreshes_once_and_is_reused_after() {
    let fixture = Provider::answering(expiring(), vec![Ok(answered(200, REFRESHED))]);

    let first = fixture.token().await.unwrap();
    let second = fixture.token().await.unwrap();

    assert_eq!(first.access_token.expose(), REFRESHED_ACCESS);
    assert_eq!(second.access_token.expose(), REFRESHED_ACCESS);
    assert_eq!(fixture.http.calls(), 1);
}

#[tokio::test]
async fn a_refresh_sends_the_rotated_form_and_persists_before_returning() {
    let fixture = Provider::answering(expiring(), vec![Ok(answered(200, REFRESHED))]);

    fixture.token().await.unwrap();

    assert_eq!(
        fixture.http.bodies(),
        vec![String::from(
            "grant_type=refresh_token&refresh_token=refresh-1&client_id=client-abc&client_secret=client-secret-xyz"
        )]
    );
    assert_eq!(fixture.store.saves(), 1);
    let persisted = fixture.store.saved().unwrap();
    assert_eq!(persisted.access_token.expose(), REFRESHED_ACCESS);
    assert_eq!(
        persisted
            .refresh_token
            .map(|token| token.expose().to_owned())
            .as_deref(),
        Some("refresh-2")
    );
    assert_eq!(persisted.expires_at, Millis(NOW.0 + 86_399_000));
}

#[tokio::test]
async fn ten_callers_inside_the_skew_share_one_refresh() {
    let http = StubHttp::holding();
    let fixture = Provider::with(
        expiring(),
        Arc::clone(&http),
        Arc::new(CountingStore::default()),
    );
    // The first task owns the exchange and blocks inside it; the rest arrive
    // while it is in flight, which is the case that must not start a second.
    let owner = tokio::spawn({
        let provider = Arc::clone(&fixture.tokens);
        async move { provider.access_token().await }
    });
    http.arrived.acquire().await.unwrap().forget();

    let mut waiters = Vec::new();
    for _ in 0..9 {
        let provider = Arc::clone(&fixture.tokens);
        waiters.push(tokio::spawn(async move { provider.access_token().await }));
    }
    // Lets every waiter reach its wait before the exchange settles.
    tokio::task::yield_now().await;
    assert_eq!(http.calls(), 1, "only the owner has reached the endpoint");

    http.answer_held(&Ok(answered(200, REFRESHED)), 10);

    // Bounded, so a coalescer that strands its waiters fails on a deadline
    // instead of stalling the suite.
    let settle = |task: tokio::task::JoinHandle<Result<Installation, TokenFailure>>| async move {
        timeout(std::time::Duration::from_secs(5), task)
            .await
            .expect("every caller settles after the exchange")
            .unwrap()
            .unwrap()
            .access_token
            .expose()
            .to_owned()
    };
    assert_eq!(settle(owner).await, REFRESHED_ACCESS);
    for waiter in waiters {
        assert_eq!(settle(waiter).await, REFRESHED_ACCESS);
    }
    assert_eq!(http.calls(), 1, "the ten callers made one request");
    assert_eq!(fixture.store.saves(), 1);
}

#[tokio::test]
async fn a_revoked_refresh_token_is_terminal() {
    let fixture = Provider::answering(expiring(), vec![Ok(answered(400, INVALID_GRANT))]);

    let failure = fixture.token().await.unwrap_err();
    assert_eq!(failure, TokenFailure::Withdrawn, "{failure:?}");
    assert_eq!(fixture.store.saves(), 0);

    let second = fixture.token().await.unwrap_err();
    assert_eq!(second, TokenFailure::NotInstalled);
    assert_eq!(
        fixture.http.calls(),
        1,
        "a credential that cannot work is not tried again"
    );
}

#[tokio::test]
async fn a_network_failure_leaves_the_old_pair_for_a_later_retry() {
    let fixture = Provider::answering(
        expiring(),
        vec![Err(HttpError::Timeout), Ok(answered(200, REFRESHED))],
    );

    let failed = fixture.token().await.unwrap_err();
    assert!(matches!(failed, TokenFailure::Transient(_)));
    assert_eq!(fixture.store.saves(), 0);

    let retried = fixture.token().await.unwrap();
    assert_eq!(retried.access_token.expose(), REFRESHED_ACCESS);
    assert_eq!(fixture.http.calls(), 2);
}

#[tokio::test]
async fn a_failed_save_keeps_the_old_pair_and_reports_why() {
    let http = StubHttp::answering(vec![
        Ok(answered(200, REFRESHED)),
        Ok(answered(200, REFRESHED)),
    ]);
    let fixture = Provider::with(expiring(), http, CountingStore::failing());

    let failure = fixture.token().await.unwrap_err();

    assert!(matches!(failure, TokenFailure::NotSaved(_)));
    assert_eq!(fixture.store.saves(), 0);
    // The old pair is still what the daemon holds, so the next call tries again
    // rather than reporting a withdrawal it never saw.
    assert!(matches!(
        fixture.token().await.unwrap_err(),
        TokenFailure::NotSaved(_)
    ));
    assert_eq!(fixture.http.calls(), 2);
}

#[tokio::test]
async fn a_crash_right_after_a_refresh_restarts_with_the_new_pair() {
    let fixture = Provider::answering(expiring(), vec![Ok(answered(200, REFRESHED))]);
    fixture.token().await.unwrap();

    // A restart reads the store and nothing else, which is the state a crash
    // between the exchange and the response leaves behind.
    let restarted = Provider::with(
        fixture.store.saved().unwrap(),
        StubHttp::answering(vec![]),
        Arc::new(CountingStore::default()),
    );
    let pair = restarted.token().await.unwrap();

    assert_eq!(pair.access_token.expose(), REFRESHED_ACCESS);
    assert_eq!(
        pair.refresh_token
            .map(|token| token.expose().to_owned())
            .as_deref(),
        Some("refresh-2")
    );
    assert_eq!(restarted.http.calls(), 0);
}

#[tokio::test]
async fn an_expired_token_refreshes_rather_than_failing() {
    let mut expired = installation();
    expired.expires_at = Millis(NOW.0 - 3_600_000);
    let fixture = Provider::answering(expired, vec![Ok(answered(200, REFRESHED))]);

    let pair = fixture.token().await.unwrap();

    assert_eq!(pair.access_token.expose(), REFRESHED_ACCESS);
    assert_eq!(fixture.http.calls(), 1);
}

#[tokio::test]
async fn a_clock_jump_past_the_expiry_refreshes_on_the_next_call() {
    let fixture = Provider::answering(installation(), vec![Ok(answered(200, REFRESHED))]);
    assert_eq!(fixture.token().await.unwrap().access_token.expose(), ACCESS);

    fixture.clock.advance(Millis(86_399_000));

    assert_eq!(
        fixture.token().await.unwrap().access_token.expose(),
        REFRESHED_ACCESS
    );
    assert_eq!(fixture.http.calls(), 1);
}

#[tokio::test]
async fn a_cancelled_refresh_leaves_the_next_caller_free_to_try() {
    let http = StubHttp::holding();
    let fixture = Provider::with(
        expiring(),
        Arc::clone(&http),
        Arc::new(CountingStore::default()),
    );
    let owner = tokio::spawn({
        let provider = Arc::clone(&fixture.tokens);
        async move { provider.access_token().await }
    });
    http.arrived.acquire().await.unwrap().forget();

    // Dropping the future mid-exchange is what a shutdown or a panicking caller
    // looks like: the slot must not stay claimed with nobody to finish it.
    owner.abort();
    assert!(owner.await.is_err());

    http.answer_held(&Ok(answered(200, REFRESHED)), 1);
    let pair = timeout(std::time::Duration::from_secs(5), fixture.token())
        .await
        .expect("a cancelled refresh must not wedge the provider")
        .unwrap();

    assert_eq!(pair.access_token.expose(), REFRESHED_ACCESS);
}

#[tokio::test]
async fn a_provider_with_nothing_installed_has_no_token() {
    let clock = MovingClock::at(NOW);
    let http = StubHttp::answering(vec![]);
    let store = Arc::new(CountingStore::default());
    let provider = TokenProvider::new(
        OauthClient {
            client_id: String::from(CLIENT_ID),
            client_secret: Secret::new(CLIENT_SECRET),
            redirect_uri: String::from(REDIRECT),
        },
        Arc::clone(&http) as Arc<dyn HttpPost>,
        Arc::clone(&store) as Arc<dyn Installations>,
        Arc::clone(&clock) as Arc<dyn Clock>,
        None,
    );

    let failure = provider.access_token().await.unwrap_err();

    assert_eq!(failure, TokenFailure::NotInstalled);
    assert_eq!(http.calls(), 0);
}

#[tokio::test]
async fn a_pair_without_a_refresh_token_cannot_be_renewed() {
    let mut bare = expiring();
    bare.refresh_token = None;
    let fixture = Provider::answering(bare, vec![]);

    let failure = fixture.token().await.unwrap_err();

    assert_eq!(failure, TokenFailure::NoRefreshToken);
    assert_eq!(fixture.http.calls(), 0);
}

/// The routes on a daemon that has an OAuth app, a store, and the installations
/// in it.
struct Routes {
    app: Router,
    http: Arc<StubHttp>,
    clock: Arc<MovingClock>,
}

impl Routes {
    fn with(
        installed: Option<Installation>,
        answers: Vec<Result<HttpResponse, HttpError>>,
    ) -> Self {
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
        // The engine task outlives the router; leaking the handle keeps its
        // command channel open for the whole test.
        std::mem::forget(task);
        let http = StubHttp::answering(answers);
        let store = Arc::new(CountingStore::default());
        let client = OauthClient {
            client_id: String::from(CLIENT_ID),
            client_secret: Secret::new(CLIENT_SECRET),
            redirect_uri: String::from(REDIRECT),
        };
        let tokens = TokenProvider::new(
            client.clone(),
            Arc::clone(&http) as Arc<dyn HttpPost>,
            Arc::clone(&store) as Arc<dyn Installations>,
            Arc::clone(&as_clock),
            installed,
        );
        let gate = Gate::new(
            WebhookSecret::new("whsec_refresh_tests"),
            Millis(60_000),
            TriggerConfig::default(),
            PromptTemplate::new("work"),
        );
        let state = AppState::new(gate, Arc::new(engine), as_clock)
            .with_oauth(Oauth {
                client,
                install_secret: Secret::new("install-secret-1"),
                http: Arc::clone(&http) as Arc<dyn HttpPost>,
                states: StateStore::new(),
            })
            .with_store(store as Arc<dyn Installations>)
            .with_tokens(Arc::new(tokens));
        Self {
            app: router(state),
            http,
            clock,
        }
    }

    async fn get(&self, uri: &str) -> (StatusCode, String) {
        let request = Request::builder().uri(uri).body(Body::empty()).unwrap();
        let response = self.app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    async fn installation(&self) -> (StatusCode, Value) {
        let (status, body) = self.get("/linear/installation").await;
        (status, serde_json::from_str(&body).unwrap_or(Value::Null))
    }

    /// Starts an install with the right secret and returns the minted state.
    async fn start_install(&self) -> String {
        let request = Request::builder()
            .uri("/linear/install?install_secret=install-secret-1")
            .body(Body::empty())
            .unwrap();
        let response = self.app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::FOUND);
        let location = response
            .headers()
            .get(axum::http::header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .expect("the authorize redirect carries a location")
            .to_owned();
        location
            .split(['?', '&'])
            .find_map(|pair| pair.strip_prefix("state="))
            .expect("the authorize URL carries a state")
            .to_owned()
    }
}

struct NoopJournal;

impl Journal for NoopJournal {
    fn persist(&self, _book: &Book) -> Result<(), JournalError> {
        Ok(())
    }
}

/// Holds every dispatch forever, so the router answers without racing a timer.
struct Holding;

impl Transport for Holding {
    fn run<'a>(&'a self, _job: &'a Dispatch) -> BoxFuture<'a, Done> {
        Box::pin(std::future::pending::<Done>())
    }
}

#[tokio::test]
async fn the_installation_route_reports_the_workspace_and_the_lifetime() {
    let routes = Routes::with(Some(installation()), vec![]);

    let (status, body) = routes.installation().await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["app_user_id"], "app-user-1");
    assert_eq!(
        body["scopes"],
        serde_json::json!(["read", "write", "app:assignable"])
    );
    assert_eq!(body["expires_in"], 86_399);
    assert_eq!(
        routes.http.calls(),
        0,
        "a live token is not refreshed early"
    );
}

#[tokio::test]
async fn the_lifetime_counts_down_as_the_clock_moves() {
    let routes = Routes::with(Some(installation()), vec![]);
    assert_eq!(routes.installation().await.1["expires_in"], 86_399);

    routes.clock.advance(Millis(60_000));

    assert_eq!(routes.installation().await.1["expires_in"], 86_339);
}

#[tokio::test]
async fn the_installation_route_carries_no_token() {
    let routes = Routes::with(Some(installation()), vec![]);

    let (_, body) = routes.get("/linear/installation").await;

    assert!(!body.contains(ACCESS), "{body}");
    assert!(!body.contains(REFRESH_TOKEN), "{body}");
    let parsed: Value = serde_json::from_str(&body).unwrap();
    let fields = parsed.as_object().unwrap();
    assert_eq!(
        fields.keys().collect::<Vec<_>>(),
        vec!["app_user_id", "expires_in", "scopes"],
        "the view is a closed set, so a token field cannot appear by accident"
    );
}

#[tokio::test]
async fn the_installation_route_carries_no_token_after_a_refresh() {
    let routes = Routes::with(Some(expiring()), vec![Ok(answered(200, REFRESHED))]);

    let (_, body) = routes.get("/linear/installation").await;

    assert_eq!(routes.http.calls(), 1);
    for token in [ACCESS, REFRESHED_ACCESS, REFRESH_TOKEN, "refresh-2"] {
        assert!(!body.contains(token), "{token} leaked into {body}");
    }
}

#[tokio::test]
async fn nothing_installed_is_a_conflict() {
    let routes = Routes::with(None, vec![]);

    let (status, body) = routes.get("/linear/installation").await;

    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body.contains("/linear/install"), "{body}");
    assert!(!body.contains(ACCESS), "{body}");
}

#[tokio::test]
async fn a_revoked_grant_is_a_conflict() {
    let routes = Routes::with(Some(expiring()), vec![Ok(answered(400, INVALID_GRANT))]);

    let (status, _) = routes.get("/linear/installation").await;
    let (again, _) = routes.get("/linear/installation").await;

    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(again, StatusCode::CONFLICT);
    assert_eq!(routes.http.calls(), 1);
}

#[tokio::test]
async fn an_expired_token_is_renewed_for_the_route_rather_than_refused() {
    let mut expired = installation();
    expired.expires_at = Millis(NOW.0 - 1);
    let routes = Routes::with(Some(expired), vec![Ok(answered(200, REFRESHED))]);

    let (status, body) = routes.installation().await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["expires_in"], 86_399);
    assert_eq!(routes.http.calls(), 1);
}

#[tokio::test]
async fn a_transient_refresh_failure_is_not_a_missing_installation() {
    let routes = Routes::with(Some(expiring()), vec![Err(HttpError::Timeout)]);

    let (status, body) = routes.get("/linear/installation").await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(body.contains("timed out"), "{body}");
}

#[tokio::test]
async fn the_installation_route_is_not_found_without_an_oauth_config() {
    let clock: Arc<dyn Clock> = MovingClock::at(NOW);
    let limits = Limits {
        in_flight: NonZeroUsize::new(1).unwrap(),
        max_queued: NonZeroUsize::new(8).unwrap(),
    };
    let (engine, task) = spawn(
        Arc::new(Holding),
        Book::new(limits),
        Arc::clone(&clock),
        Arc::new(NoopJournal),
    );
    std::mem::forget(task);
    let gate = Gate::new(
        WebhookSecret::new("whsec_refresh_tests"),
        Millis(60_000),
        TriggerConfig::default(),
        PromptTemplate::new("work"),
    );
    let app = router(AppState::new(gate, Arc::new(engine), clock));

    let response = app
        .oneshot(
            Request::builder()
                .uri("/linear/installation")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_second_install_clears_a_revoked_grant() {
    let mut expired = installation();
    expired.expires_at = Millis(NOW.0 - 1);
    // The first refresh is refused, which moves the provider to `Missing`; the
    // answers after it are the install flow the operator runs next.
    let routes = Routes::with(
        Some(expired),
        vec![
            Ok(answered(400, INVALID_GRANT)),
            Ok(answered(
                200,
                r#"{"access_token":"access-3","refresh_token":"refresh-3","expires_in":86399,"scope":"read write"}"#,
            )),
            Ok(answered(200, r#"{"data":{"viewer":{"id":"app-user-1"}}}"#)),
        ],
    );
    assert_eq!(
        routes.get("/linear/installation").await.0,
        StatusCode::CONFLICT
    );

    let state = routes.start_install().await;
    let (status, _) = routes
        .get(&format!("/linear/oauth/callback?code=code-1&state={state}"))
        .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = routes.installation().await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["app_user_id"], "app-user-1");
    assert_eq!(body["scopes"], serde_json::json!(["read", "write"]));
    assert_eq!(body["expires_in"], 86_399);
    assert_eq!(
        routes.http.calls(),
        3,
        "one refused refresh, then the two install calls"
    );
}
