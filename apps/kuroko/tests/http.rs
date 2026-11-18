#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use kuroko::http::Http;
use kuroko_linear::oauth::{HttpError, HttpPost, PostRequest};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const TIMEOUT: Duration = Duration::from_millis(200);

/// A listener that accepts a connection and then never answers, which is what a
/// hung Linear looks like to the client.
async fn silent_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        tokio::time::sleep(Duration::from_secs(30)).await;
        drop(stream);
    });
    url
}

#[tokio::test]
async fn a_silent_peer_times_out_rather_than_hanging() {
    let url = silent_server().await;
    let http = Http::new(TIMEOUT).unwrap();

    let error = http
        .post(PostRequest {
            url: &url,
            content_type: "application/x-www-form-urlencoded",
            authorization: None,
            body: b"code=1",
        })
        .await
        .unwrap_err();

    assert_eq!(error, HttpError::Timeout);
}

#[tokio::test]
async fn a_refused_connection_is_not_reported_as_a_timeout() {
    // Bind then drop, so the port has no listener behind it.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let url = format!("http://{addr}");
    let http = Http::new(TIMEOUT).unwrap();

    let error = http
        .post(PostRequest {
            url: &url,
            content_type: "application/json",
            authorization: Some("Bearer access-1"),
            body: b"{}",
        })
        .await
        .unwrap_err();

    assert!(
        matches!(error, HttpError::Other(_)),
        "expected a transport failure, got {error:?}"
    );
}

#[tokio::test]
async fn a_real_answer_carries_its_status_and_bytes() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buffer = [0_u8; 1024];
        let _ = stream.read(&mut buffer).await;
        stream
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 11\r\n\r\nhello world")
            .await
            .unwrap();
        stream.shutdown().await.unwrap();
    });
    let http = Http::new(TIMEOUT).unwrap();

    let response = http
        .post(PostRequest {
            url: &url,
            content_type: "application/json",
            authorization: None,
            body: b"{}",
        })
        .await
        .unwrap();

    assert_eq!(response.status, 200);
    assert_eq!(response.body, b"hello world");
}
