//! HTTP surface: the Linear webhook, the task snapshot, and the OAuth install.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use kuroko_core::{Clock, Submission, Task, TaskKey, TaskRequest};
use kuroko_engine::Engine;
use kuroko_linear::oauth::{
    HttpPost, INSTALL_STATE_TTL, Installation, OauthClient, Secret, StateStore, StateToken,
    complete_install,
};
use kuroko_linear::{Gate, Intake, Rejection};
use serde::Deserialize;
use tokio::time::timeout;

/// Linear gives up after five seconds, so a wedged engine must not spend that budget.
const SUBMIT_TIMEOUT: Duration = Duration::from_secs(2);

const SIGNATURE_HEADER: &str = "linear-signature";

#[derive(Clone)]
pub struct AppState {
    pub gate: Gate,
    /// Held behind an `Arc` because dropping an [`Engine`] clone is a shutdown trigger, and
    /// axum clones and drops the state while it builds the router.
    pub engine: Arc<Engine>,
    pub clock: Arc<dyn Clock>,
    /// Absent until the operator registers an OAuth app, which leaves webhooks
    /// working and answers the install routes with `404`.
    pub oauth: Option<Arc<Oauth>>,
    /// The installations `AppState` writes through, absent in a build that took
    /// no store.
    pub store: Option<Arc<dyn Installations>>,
}

/// Everything the install routes need, present only when `[linear.oauth]` is.
pub struct Oauth {
    pub client: OauthClient,
    pub install_secret: Secret,
    pub http: Arc<dyn HttpPost>,
    pub states: StateStore,
}

impl std::fmt::Debug for Oauth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Oauth")
            .field("client", &self.client)
            .field("install_secret", &self.install_secret)
            .finish_non_exhaustive()
    }
}

/// Where a completed install is written. The store implements it; a test supplies
/// one that counts writes.
pub trait Installations: Send + Sync {
    /// Persists `installation`, replacing the row for its app user id.
    ///
    /// # Errors
    ///
    /// Returns the message the page shows when the write fails.
    fn save(&self, installation: &Installation) -> Result<(), String>;
}

impl AppState {
    #[must_use]
    pub const fn new(gate: Gate, engine: Arc<Engine>, clock: Arc<dyn Clock>) -> Self {
        Self {
            gate,
            engine,
            clock,
            oauth: None,
            store: None,
        }
    }

    #[must_use]
    pub fn with_oauth(mut self, oauth: Oauth) -> Self {
        self.oauth = Some(Arc::new(oauth));
        self
    }

    #[must_use]
    pub fn with_store(mut self, store: Arc<dyn Installations>) -> Self {
        self.store = Some(store);
        self
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/webhooks/linear", post(webhook))
        .route("/tasks", get(tasks))
        .route("/linear/install", get(install))
        .route("/linear/oauth/callback", get(callback))
        .with_state(state)
}

async fn webhook(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> StatusCode {
    let signature = headers
        .get(SIGNATURE_HEADER)
        .and_then(|value| value.to_str().ok());
    let intake = match state.gate.accept(signature, &body, state.clock.now_ms()) {
        Ok(intake) => intake,
        Err(rejection) => return rejection_status(rejection),
    };
    match intake {
        Intake::Admit(request) => admit(&state, request).await,
        Intake::Withdraw(key) => withdraw(&state, key).await,
        Intake::Drop(reason) => {
            log::info!("delivery dropped: {reason:?}");
            StatusCode::OK
        }
    }
}

async fn withdraw(state: &AppState, key: TaskKey) -> StatusCode {
    match timeout(SUBMIT_TIMEOUT, state.engine.withdraw(key)).await {
        Ok(Ok(())) => StatusCode::OK,
        Ok(Err(error)) => {
            log::warn!("withdraw refused: {error}");
            StatusCode::SERVICE_UNAVAILABLE
        }
        Err(_) => {
            log::warn!("engine did not answer within two seconds, withdraw refused");
            StatusCode::SERVICE_UNAVAILABLE
        }
    }
}

async fn admit(state: &AppState, request: TaskRequest) -> StatusCode {
    let identifier = request.spec.identifier.clone();
    match timeout(SUBMIT_TIMEOUT, state.engine.submit(request)).await {
        Ok(Ok(Submission::Accepted)) => {
            log::info!("admitted {identifier}");
            StatusCode::OK
        }
        Ok(Ok(Submission::Duplicate)) => {
            log::info!("duplicate {identifier}");
            StatusCode::OK
        }
        Ok(Ok(Submission::Full)) => {
            log::warn!("queue full, refused {identifier}");
            StatusCode::SERVICE_UNAVAILABLE
        }
        Ok(Err(error)) => {
            log::warn!("{error}, refused {identifier}");
            StatusCode::SERVICE_UNAVAILABLE
        }
        Err(_) => {
            log::warn!("engine did not answer within two seconds, refused {identifier}");
            StatusCode::SERVICE_UNAVAILABLE
        }
    }
}

async fn tasks(State(state): State<AppState>) -> Result<Json<Vec<Task>>, StatusCode> {
    state
        .engine
        .snapshot()
        .await
        .map(Json)
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)
}

fn rejection_status(rejection: Rejection) -> StatusCode {
    // `Rejection::status` owns the mapping; every value it returns is a valid status.
    StatusCode::from_u16(rejection.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
}

/// The query parameters `/linear/install` accepts.
#[derive(Debug, Deserialize)]
struct InstallQuery {
    install_secret: Option<String>,
}

/// Starts an install: checks the secret, mints a state, redirects to Linear.
async fn install(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<InstallQuery>,
) -> Response {
    let Some(oauth) = state.oauth.as_ref() else {
        return page(
            StatusCode::NOT_FOUND,
            "OAuth is not configured in kuroko.toml.",
        );
    };
    let provided = install_secret(&headers, query.install_secret.as_deref());
    if !provided.is_some_and(|provided| oauth.install_secret.matches(provided)) {
        return page(StatusCode::UNAUTHORIZED, MISSING_INSTALL_SECRET);
    }

    let now = state.clock.now_ms();
    let token = StateToken::generate(&mut rand::rng(), now, INSTALL_STATE_TTL);
    let location = oauth.client.authorize_url(token.clone());
    oauth.states.insert(token, now);
    log::info!("install started, redirecting to Linear");
    // axum's `Redirect` answers 303; Linear's install flow documents 302, so the
    // response is built here rather than through that helper.
    (StatusCode::FOUND, [(header::LOCATION, location)]).into_response()
}

/// The query parameters Linear sends back to `/linear/oauth/callback`.
#[derive(Debug, Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

async fn callback(State(state): State<AppState>, Query(query): Query<CallbackQuery>) -> Response {
    let Some(oauth) = state.oauth.as_ref() else {
        return page(
            StatusCode::NOT_FOUND,
            "OAuth is not configured in kuroko.toml.",
        );
    };
    if let Some(error) = query.error.as_deref() {
        return page(
            StatusCode::BAD_REQUEST,
            &format!("Linear refused the install: {error}"),
        );
    }
    let (Some(code), Some(state_token)) = (query.code.as_deref(), query.state.as_deref()) else {
        return page(
            StatusCode::BAD_REQUEST,
            "The callback is missing its `code` or `state`.",
        );
    };
    let now = state.clock.now_ms();
    // Consumed before the exchange, so a replay cannot reach Linear at all and a
    // state stolen after a failure cannot be retried.
    if !oauth.states.consume(state_token, now) {
        return page(
            StatusCode::UNAUTHORIZED,
            "That install link is unknown or expired. Start again at `/linear/install`.",
        );
    }
    let Some(store) = state.store.as_ref() else {
        return page(
            StatusCode::SERVICE_UNAVAILABLE,
            "kuroko has no store to save the installation in.",
        );
    };

    let installation = match complete_install(oauth.http.as_ref(), &oauth.client, code, now).await {
        Ok(installation) => installation,
        Err(error) => {
            // The message is already redacted, so it is safe to log and to show.
            log::warn!("install failed: {error}");
            return page(
                StatusCode::BAD_GATEWAY,
                &format!("The install with Linear failed: {error}"),
            );
        }
    };
    if let Err(error) = store.save(&installation) {
        log::warn!("cannot save the installation: {error}");
        return page(
            StatusCode::INTERNAL_SERVER_ERROR,
            "kuroko could not save the installation.",
        );
    }
    log::info!("installed as app user {}", installation.app_user_id);
    page(
        StatusCode::OK,
        &format!(
            "kuroko is installed in Linear.\nApp user ID: {}\nYou can close this tab.\n",
            installation.app_user_id
        ),
    )
}

const MISSING_INSTALL_SECRET: &str =
    "Send the install secret as `Authorization: Bearer <secret>` or `?install_secret=<secret>`.";

/// Reads the install secret from the bearer header, falling back to the query.
fn install_secret<'a>(headers: &'a HeaderMap, query: Option<&'a str>) -> Option<&'a str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .or(query)
}

/// A plain text page, which is what a browser shows after a redirect.
fn page(status: StatusCode, body: &str) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        body.to_owned(),
    )
        .into_response()
}
