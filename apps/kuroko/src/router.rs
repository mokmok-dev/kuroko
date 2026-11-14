//! HTTP surface: the Linear webhook and the task snapshot.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use kuroko_core::{Clock, Submission, Task, TaskKey, TaskRequest};
use kuroko_engine::Engine;
use kuroko_linear::{Gate, Intake, Rejection};
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
}

impl AppState {
    #[must_use]
    pub const fn new(gate: Gate, engine: Arc<Engine>, clock: Arc<dyn Clock>) -> Self {
        Self {
            gate,
            engine,
            clock,
        }
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/webhooks/linear", post(webhook))
        .route("/tasks", get(tasks))
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
