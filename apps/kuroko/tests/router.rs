#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use hmac::{Hmac, KeyInit, Mac};
use kuroko::router::{AppState, router};
use kuroko_core::{Clock, Dispatch, Done, IssueId, Limits, Millis};
use kuroko_engine::{BoxFuture, Transport, spawn};
use kuroko_linear::{Gate, PromptTemplate, TriggerConfig, WebhookSecret};
use serde_json::{Value, json};
use sha2::Sha256;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tower::ServiceExt;

const SECRET: &str = "whsec_app_tests";
const NOW: Millis = Millis(1_700_000_000_000);
const WINDOW: Millis = Millis(60_000);
const AGENT: &str = "agent-1";
const WAIT: Duration = Duration::from_secs(5);
const TEMPLATE: &str = "Issue {identifier}: {title}\n\n{description}\n{url}";

struct FixedClock(Millis);

impl Clock for FixedClock {
    fn now_ms(&self) -> Millis {
        self.0
    }
}

/// Reports each dispatch and then holds it forever, so a test sees the queue and the
/// in-flight count without racing a timer.
struct Holding {
    started: mpsc::UnboundedSender<Dispatch>,
}

impl Transport for Holding {
    fn run<'a>(&'a self, job: &'a Dispatch) -> BoxFuture<'a, Done> {
        Box::pin(async move {
            let _ = self.started.send(job.clone());
            std::future::pending::<Done>().await
        })
    }
}

struct Fixture {
    app: Router,
    started: mpsc::UnboundedReceiver<Dispatch>,
}

impl Fixture {
    fn new(in_flight: usize, max_queued: usize) -> Self {
        let (started, dispatches) = mpsc::unbounded_channel();
        let transport = Arc::new(Holding { started });
        let limits = Limits {
            in_flight: NonZeroUsize::new(in_flight).unwrap(),
            max_queued: NonZeroUsize::new(max_queued).unwrap(),
        };
        let clock: Arc<dyn Clock> = Arc::new(FixedClock(NOW));
        let (engine, _task) = spawn(transport, limits, Arc::clone(&clock));
        let engine = Arc::new(engine);
        let gate = Gate::new(
            WebhookSecret::new(SECRET),
            WINDOW,
            TriggerConfig {
                any_delegate: true,
                ..TriggerConfig::default()
            },
            PromptTemplate::new(TEMPLATE),
        );
        Self {
            app: router(AppState::new(gate, engine, clock)),
            started: dispatches,
        }
    }

    async fn post(&self, body: &[u8], signature: Option<&str>) -> StatusCode {
        let mut request = Request::builder()
            .method("POST")
            .uri("/webhooks/linear")
            .header("content-type", "application/json");
        if let Some(signature) = signature {
            request = request.header("linear-signature", signature);
        }
        let request = request.body(Body::from(body.to_vec())).unwrap();
        self.app.clone().oneshot(request).await.unwrap().status()
    }

    async fn post_signed(&self, body: &[u8]) -> StatusCode {
        self.post(body, Some(&sign(SECRET, body))).await
    }

    async fn next_dispatch(&mut self) -> Dispatch {
        timeout(WAIT, self.started.recv()).await.unwrap().unwrap()
    }

    async fn tasks(&self) -> (StatusCode, Value) {
        let request = Request::builder()
            .uri("/tasks")
            .body(Body::empty())
            .unwrap();
        let response = self.app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&body).unwrap())
    }
}

fn sign(secret: &str, body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(body);
    hex::encode(mac.finalize().into_bytes())
}

fn payload(
    timestamp: i64,
    action: &str,
    issue: &str,
    delegate: Option<&str>,
    labels: &[&str],
) -> Vec<u8> {
    let labels = labels
        .iter()
        .map(|name| json!({ "name": name }))
        .collect::<Vec<_>>();
    let mut data = json!({
        "id": issue,
        "identifier": format!("MOK-{issue}"),
        "title": format!("title {issue}"),
        "description": format!("body {issue}"),
        "url": format!("https://linear.app/mok/issue/{issue}"),
        "labels": labels,
    });
    if let Some(id) = delegate {
        data["delegate"] = json!({ "id": id });
    }
    json!({
        "action": action,
        "type": "Issue",
        "createdAt": "2026-11-12T09:00:00.000Z",
        "data": data,
        "webhookTimestamp": timestamp,
    })
    .to_string()
    .into_bytes()
}

#[tokio::test]
async fn a_signed_delivery_reaches_the_transport() {
    let mut fixture = Fixture::new(1, 64);
    let body = payload(NOW.0, "create", "issue-a", Some(AGENT), &[]);

    assert_eq!(fixture.post_signed(&body).await, StatusCode::OK);

    let job = fixture.next_dispatch().await;
    assert_eq!(job.issue, "MOK-issue-a");
    assert_eq!(job.key.issue, IssueId::new("issue-a"));
    assert_eq!(
        job.prompt,
        "Issue MOK-issue-a: title issue-a\n\nbody issue-a\nhttps://linear.app/mok/issue/issue-a"
    );
}

#[tokio::test]
async fn an_unsigned_delivery_is_unauthorized() {
    let fixture = Fixture::new(1, 64);
    let body = payload(NOW.0, "create", "issue-a", Some(AGENT), &[]);

    assert_eq!(fixture.post(&body, None).await, StatusCode::UNAUTHORIZED);

    let (status, tasks) = fixture.tasks().await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(tasks, json!([]));
}

#[tokio::test]
async fn a_tampered_body_is_unauthorized() {
    let fixture = Fixture::new(1, 64);
    let body = payload(NOW.0, "create", "issue-a", Some(AGENT), &[]);
    let signature = sign(SECRET, &body);
    let mut tampered = body.clone();
    tampered.push(b' ');

    assert_eq!(
        fixture.post(&tampered, Some(&signature)).await,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn a_stale_timestamp_is_rejected() {
    let fixture = Fixture::new(1, 64);
    let body = payload(NOW.0 - 61_000, "create", "issue-a", Some(AGENT), &[]);

    assert_eq!(fixture.post_signed(&body).await, StatusCode::BAD_REQUEST);

    let (status, tasks) = fixture.tasks().await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(tasks, json!([]));
}

#[tokio::test]
async fn a_non_triggering_issue_is_acknowledged_without_a_task() {
    let fixture = Fixture::new(1, 64);
    let body = payload(NOW.0, "create", "issue-a", None, &[]);

    assert_eq!(fixture.post_signed(&body).await, StatusCode::OK);

    let (status, tasks) = fixture.tasks().await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(tasks, json!([]));
}

#[tokio::test]
async fn a_full_queue_is_unavailable() {
    let fixture = Fixture::new(1, 1);
    let first = payload(NOW.0, "create", "issue-a", Some(AGENT), &[]);
    let second = payload(NOW.0, "create", "issue-b", Some(AGENT), &[]);
    let third = payload(NOW.0, "create", "issue-c", Some(AGENT), &[]);

    assert_eq!(fixture.post_signed(&first).await, StatusCode::OK);
    assert_eq!(fixture.post_signed(&second).await, StatusCode::OK);
    assert_eq!(
        fixture.post_signed(&third).await,
        StatusCode::SERVICE_UNAVAILABLE
    );
}

#[tokio::test]
async fn tasks_returns_the_snapshot() {
    let mut fixture = Fixture::new(1, 64);
    let (status, empty) = fixture.tasks().await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(empty, json!([]));

    let body = payload(NOW.0, "create", "issue-a", Some(AGENT), &[]);
    assert_eq!(fixture.post_signed(&body).await, StatusCode::OK);
    let _job = fixture.next_dispatch().await;

    let (status, tasks) = fixture.tasks().await;
    assert_eq!(status, StatusCode::OK);
    let tasks = tasks.as_array().unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0]["key"]["issue"], "issue-a");
    assert_eq!(tasks[0]["spec"]["identifier"], "MOK-issue-a");
    assert_eq!(tasks[0]["state"], "Dispatched");
}
