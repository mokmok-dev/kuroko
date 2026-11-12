#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use kuroko_core::{
    Dispatch, FailureReason, IssueId, LabelName, Millis, Outcome, Source, TaskKey, TriggerId,
};
use kuroko_engine::Transport;
use kuroko_transport::WebSocket;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

const FRAME: &str = r#"{"kind":"task","issue":"MOK-7","prompt":"hello"}"#;

enum Reply {
    Text(&'static str),
    Drop,
    Silent,
}

fn dispatch(prompt: &str) -> Dispatch {
    Dispatch {
        key: TaskKey {
            source: Source::Linear,
            issue: IssueId::new("issue-a"),
            trigger: TriggerId::Label(LabelName::new("kuroko")),
        },
        issue: "MOK-7".to_owned(),
        prompt: prompt.to_owned(),
    }
}

const fn transport(url: String, timeout: Duration) -> WebSocket {
    WebSocket { url, timeout }
}

async fn serve(reply: Reply) -> (String, JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = accept_async(stream).await.unwrap();
        let received = match socket.next().await {
            Some(Ok(Message::Text(text))) => text.as_str().to_owned(),
            other => panic!("expected a text frame, got {other:?}"),
        };
        match reply {
            Reply::Text(answer) => {
                socket.send(Message::text(answer)).await.unwrap();
                // Hold the socket open until the client has read the answer and gone.
                let _ = socket.next().await;
            }
            Reply::Drop => {}
            Reply::Silent => tokio::time::sleep(Duration::from_secs(30)).await,
        }
        received
    });
    (url, server)
}

#[tokio::test]
async fn an_ok_answer_is_success_and_the_frame_is_pinned() {
    let (url, server) = serve(Reply::Text(r#"{"ok":true,"output":"the answer"}"#)).await;
    let done = transport(url, Duration::from_secs(5))
        .run(&dispatch("hello"))
        .await;

    assert_eq!(server.await.unwrap(), FRAME);
    assert_eq!(done.outcome, Outcome::Success);
    assert_eq!(done.output, "the answer");
    assert_eq!(done.log, "");
    assert_eq!(done.at, Millis(0));
}

#[tokio::test]
async fn a_negative_answer_is_a_bad_result() {
    let (url, server) = serve(Reply::Text(r#"{"ok":false}"#)).await;
    let done = transport(url, Duration::from_secs(5))
        .run(&dispatch("hello"))
        .await;

    assert_eq!(server.await.unwrap(), FRAME);
    assert_eq!(done.outcome, Outcome::Failure(FailureReason::BadResult));
    assert_eq!(done.log, r#"{"ok":false}"#);
    assert_eq!(done.output, "");
}

#[tokio::test]
async fn garbage_is_a_bad_result() {
    let (url, server) = serve(Reply::Text("not json")).await;
    let done = transport(url, Duration::from_secs(5))
        .run(&dispatch("hello"))
        .await;

    assert_eq!(server.await.unwrap(), FRAME);
    assert_eq!(done.outcome, Outcome::Failure(FailureReason::BadResult));
    assert_eq!(done.log, "not json");
}

#[tokio::test]
async fn a_dropped_connection_is_disconnected() {
    let (url, server) = serve(Reply::Drop).await;
    let done = transport(url, Duration::from_secs(5))
        .run(&dispatch("hello"))
        .await;

    assert_eq!(server.await.unwrap(), FRAME);
    assert_eq!(done.outcome, Outcome::Failure(FailureReason::Disconnected));
    assert_ne!(done.log, "");
}

#[tokio::test]
async fn a_refused_connection_is_disconnected() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    drop(listener);

    let done = transport(url, Duration::from_secs(5))
        .run(&dispatch("hello"))
        .await;

    assert_eq!(done.outcome, Outcome::Failure(FailureReason::Disconnected));
    assert_ne!(done.log, "");
}

#[tokio::test]
async fn a_silent_server_times_out() {
    let (url, _server) = serve(Reply::Silent).await;
    let started = Instant::now();
    let done = transport(url, Duration::from_secs(1))
        .run(&dispatch("hello"))
        .await;
    let elapsed = started.elapsed();

    assert_eq!(done.outcome, Outcome::Failure(FailureReason::Timeout));
    assert!(elapsed < Duration::from_secs(10), "took {elapsed:?}");
}
