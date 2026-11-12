//! Stdio and websocket transports.

use std::process::Stdio as Pipe;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use kuroko_core::{Dispatch, Done, FailureReason, Millis, Outcome};
use kuroko_engine::{BoxFuture, Transport};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::time::timeout;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

const TAIL: usize = 8 * 1024;

/// A local agent driven over its standard streams.
#[derive(Debug)]
pub struct Stdio {
    pub command: String,
    pub args: Vec<String>,
    pub timeout: Duration,
}

impl Transport for Stdio {
    fn run<'a>(&'a self, job: &'a Dispatch) -> BoxFuture<'a, Done> {
        Box::pin(async move {
            let mut spawned = None;
            let attempt = self.exchange(job, &mut spawned);
            let outcome = timeout(self.timeout, attempt).await;
            if let Ok(done) = outcome {
                done
            } else {
                if let Some(mut child) = spawned {
                    // `kill` waits as well, so a timed-out child is never left a zombie.
                    let _ = child.kill().await;
                }
                timed_out(self.timeout)
            }
        })
    }
}

impl Stdio {
    /// Runs the child, publishing the child handle through `spawned` so that a timeout can
    /// still reach it after this future has been dropped.
    async fn exchange(&self, job: &Dispatch, spawned: &mut Option<Child>) -> Done {
        let mut command = Command::new(&self.command);
        command
            .args(&self.args)
            .stdin(Pipe::piped())
            .stdout(Pipe::piped())
            .stderr(Pipe::piped())
            // A dispatch future dropped without going through the timeout must not orphan
            // the child.
            .kill_on_drop(true);
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => return disconnected(&format!("spawn {}: {error}", self.command)),
        };
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        *spawned = Some(child);

        // The child may answer before it reads stdin, so both pipes drain while the prompt
        // is written; a prompt larger than a pipe buffer would deadlock a write-then-read.
        let feed = async move {
            if let Some(mut stdin) = stdin {
                let _ = stdin.write_all(job.prompt.as_bytes()).await;
            }
        };
        let ((), out, err) = tokio::join!(feed, read_tail(stdout), read_tail(stderr));

        let Some(mut child) = spawned.take() else {
            return disconnected("the child handle was already claimed");
        };
        let status = match child.wait().await {
            Ok(status) => status,
            Err(error) => return disconnected(&format!("wait {}: {error}", self.command)),
        };

        let outcome = if status.success() {
            Outcome::Success
        } else {
            Outcome::Failure(FailureReason::Exit {
                code: status.code(),
            })
        };
        Done {
            at: Millis(0),
            outcome,
            output: tail(&out),
            log: tail(&err),
        }
    }
}

/// A local agent driven over a websocket.
#[derive(Debug)]
pub struct WebSocket {
    pub url: String,
    pub timeout: Duration,
}

#[derive(Serialize)]
struct TaskFrame<'a> {
    kind: &'a str,
    issue: &'a str,
    prompt: &'a str,
}

#[derive(Deserialize)]
struct ResultFrame {
    ok: bool,
    output: Option<String>,
}

impl Transport for WebSocket {
    fn run<'a>(&'a self, job: &'a Dispatch) -> BoxFuture<'a, Done> {
        Box::pin(async move {
            let attempt = self.exchange(job);
            timeout(self.timeout, attempt)
                .await
                .unwrap_or_else(|_| timed_out(self.timeout))
        })
    }
}

impl WebSocket {
    async fn exchange(&self, job: &Dispatch) -> Done {
        let mut socket = match connect_async(self.url.as_str()).await {
            Ok((socket, _response)) => socket,
            Err(error) => return disconnected(&format!("connect {}: {error}", self.url)),
        };

        let frame = TaskFrame {
            kind: "task",
            issue: &job.issue,
            prompt: &job.prompt,
        };
        let text = match serde_json::to_string(&frame) {
            Ok(text) => text,
            Err(error) => return disconnected(&format!("encode task: {error}")),
        };
        match socket.send(Message::text(text)).await {
            Ok(()) => {}
            Err(error) => return disconnected(&format!("send {}: {error}", self.url)),
        }

        match socket.next().await {
            Some(Ok(Message::Text(text))) => answer(text.as_str()),
            Some(Ok(Message::Close(_))) => disconnected(&format!("{} closed the socket", self.url)),
            Some(Ok(_)) => bad_result(&format!("{} answered without a text frame", self.url)),
            Some(Err(error)) => disconnected(&format!("receive {}: {error}", self.url)),
            None => disconnected(&format!("{} ended the stream", self.url)),
        }
    }
}

fn answer(raw: &str) -> Done {
    match serde_json::from_str::<ResultFrame>(raw) {
        Ok(frame) if frame.ok => Done {
            at: Millis(0),
            outcome: Outcome::Success,
            output: tail(frame.output.unwrap_or_default().as_bytes()),
            log: String::new(),
        },
        _ => bad_result(raw),
    }
}

/// Reads `pipe` to its end, keeping only the bytes a [`Done`] can carry.
async fn read_tail(pipe: Option<impl AsyncRead + Unpin>) -> Vec<u8> {
    let Some(mut pipe) = pipe else {
        return Vec::new();
    };
    let mut buffer = vec![0_u8; TAIL];
    let mut tail = Vec::with_capacity(TAIL);
    loop {
        let Ok(read) = pipe.read(&mut buffer).await else {
            return tail;
        };
        if read == 0 {
            return tail;
        }
        tail.extend_from_slice(&buffer[..read]);
        tail.drain(..tail.len().saturating_sub(TAIL));
    }
}

/// The last [`TAIL`] bytes of `bytes`, moved forward to the first character boundary.
fn tail(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut start = text.len().saturating_sub(TAIL);
    while !text.is_char_boundary(start) {
        start = start.saturating_add(1);
    }
    text[start..].to_owned()
}

fn timed_out(limit: Duration) -> Done {
    Done {
        at: Millis(0),
        outcome: Outcome::Failure(FailureReason::Timeout),
        output: String::new(),
        log: format!("timed out after {limit:?}"),
    }
}

fn disconnected(log: &str) -> Done {
    Done {
        at: Millis(0),
        outcome: Outcome::Failure(FailureReason::Disconnected),
        output: String::new(),
        log: tail(log.as_bytes()),
    }
}

fn bad_result(log: &str) -> Done {
    Done {
        at: Millis(0),
        outcome: Outcome::Failure(FailureReason::BadResult),
        output: String::new(),
        log: tail(log.as_bytes()),
    }
}
