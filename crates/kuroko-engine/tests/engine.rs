#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use kuroko_core::{
    Clock, Dispatch, Done, FailureReason, IssueId, LabelName, Limits, Millis, Outcome, Source,
    Submission, TaskKey, TaskRequest, TaskSpec, TaskState, TriggerId,
};
use kuroko_engine::{BoxFuture, EngineDown, Transport, spawn};
use tokio::sync::{mpsc, watch};
use tokio::time::timeout;

struct FixedClock(Millis);

impl Clock for FixedClock {
    fn now_ms(&self) -> Millis {
        self.0
    }
}

async fn wait_for(gate: &mut watch::Receiver<bool>) {
    while !*gate.borrow_and_update() {
        if gate.changed().await.is_err() {
            break;
        }
    }
}

struct Gated {
    starts: mpsc::UnboundedSender<TaskKey>,
    gate: watch::Receiver<bool>,
    calls: AtomicUsize,
    completed: AtomicBool,
}

impl Gated {
    fn new(gate: watch::Receiver<bool>) -> (Arc<Self>, mpsc::UnboundedReceiver<TaskKey>) {
        let (starts, started) = mpsc::unbounded_channel();
        (
            Arc::new(Self {
                starts,
                gate,
                calls: AtomicUsize::new(0),
                completed: AtomicBool::new(false),
            }),
            started,
        )
    }
}

impl Transport for Gated {
    fn run<'a>(&'a self, job: &'a Dispatch) -> BoxFuture<'a, Done> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let _ = self.starts.send(job.key.clone());
            let mut gate = self.gate.clone();
            wait_for(&mut gate).await;
            self.completed.store(true, Ordering::SeqCst);
            Done {
                at: Millis(1),
                outcome: Outcome::Success,
                output: format!("done {}", job.issue),
                log: String::new(),
            }
        })
    }
}

struct Exploding {
    starts: mpsc::UnboundedSender<TaskKey>,
    gate: watch::Receiver<bool>,
    calls: AtomicUsize,
}

impl Exploding {
    fn new(gate: watch::Receiver<bool>) -> (Arc<Self>, mpsc::UnboundedReceiver<TaskKey>) {
        let (starts, started) = mpsc::unbounded_channel();
        (
            Arc::new(Self {
                starts,
                gate,
                calls: AtomicUsize::new(0),
            }),
            started,
        )
    }
}

impl Transport for Exploding {
    fn run<'a>(&'a self, job: &'a Dispatch) -> BoxFuture<'a, Done> {
        Box::pin(async move {
            assert!(
                self.calls.fetch_add(1, Ordering::SeqCst) != 0,
                "transport exploded"
            );
            let _ = self.starts.send(job.key.clone());
            let mut gate = self.gate.clone();
            wait_for(&mut gate).await;
            Done {
                at: Millis(1),
                outcome: Outcome::Success,
                output: String::new(),
                log: String::new(),
            }
        })
    }
}

const fn limits(in_flight: usize, max_queued: usize) -> Limits {
    Limits {
        in_flight: NonZeroUsize::new(in_flight).unwrap(),
        max_queued: NonZeroUsize::new(max_queued).unwrap(),
    }
}

fn key(issue: &str, label: &str) -> TaskKey {
    TaskKey {
        source: Source::Linear,
        issue: IssueId::new(issue),
        trigger: TriggerId::Label(LabelName::new(label)),
    }
}

fn request(key: TaskKey, identifier: &str, prompt: &str) -> TaskRequest {
    TaskRequest {
        key,
        spec: TaskSpec {
            identifier: identifier.to_owned(),
            title: "title".to_owned(),
            description: None,
            url: None,
        },
        prompt: prompt.to_owned(),
    }
}

async fn next_start(starts: &mut mpsc::UnboundedReceiver<TaskKey>) -> TaskKey {
    timeout(Duration::from_secs(1), starts.recv())
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test(start_paused = true)]
async fn duplicate_delivery_runs_the_transport_once() {
    let (gate, gate_rx) = watch::channel(false);
    let (transport, mut starts) = Gated::new(gate_rx);
    let (engine, handle) = spawn(transport, limits(1, 4), Arc::new(FixedClock(Millis(10))));

    let admitted = request(key("issue-a", "kuroko"), "MOK-1", "prompt");
    assert_eq!(
        engine.submit(admitted.clone()).await,
        Ok(Submission::Accepted)
    );
    assert_eq!(engine.submit(admitted).await, Ok(Submission::Duplicate));

    assert_eq!(next_start(&mut starts).await, key("issue-a", "kuroko"));
    tokio::task::yield_now().await;
    assert!(starts.try_recv().is_err());

    gate.send(true).unwrap();
    drop(engine);
    timeout(Duration::from_secs(1), handle)
        .await
        .unwrap()
        .unwrap();
    assert!(starts.try_recv().is_err());
}

#[tokio::test(start_paused = true)]
async fn a_second_dispatch_waits_for_the_first_to_finish() {
    let (gate, gate_rx) = watch::channel(false);
    let (transport, mut starts) = Gated::new(gate_rx);
    let (engine, handle) = spawn(
        transport.clone(),
        limits(1, 4),
        Arc::new(FixedClock(Millis(10))),
    );

    assert_eq!(
        engine
            .submit(request(key("issue-a", "kuroko"), "MOK-1", "a"))
            .await,
        Ok(Submission::Accepted)
    );
    assert_eq!(next_start(&mut starts).await, key("issue-a", "kuroko"));
    assert_eq!(
        engine
            .submit(request(key("issue-b", "kuroko"), "MOK-2", "b"))
            .await,
        Ok(Submission::Accepted)
    );

    assert!(starts.try_recv().is_err());
    assert_eq!(transport.calls.load(Ordering::SeqCst), 1);

    gate.send(true).unwrap();
    assert_eq!(next_start(&mut starts).await, key("issue-b", "kuroko"));
    assert_eq!(transport.calls.load(Ordering::SeqCst), 2);

    drop(engine);
    timeout(Duration::from_secs(1), handle)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_panicking_transport_settles_as_panicked_and_frees_the_slot() {
    let (gate, gate_rx) = watch::channel(false);
    let (transport, mut starts) = Exploding::new(gate_rx);
    let (engine, handle) = spawn(transport, limits(1, 4), Arc::new(FixedClock(Millis(5))));

    assert_eq!(
        engine
            .submit(request(key("issue-a", "kuroko"), "MOK-1", "a"))
            .await,
        Ok(Submission::Accepted)
    );
    assert_eq!(
        engine
            .submit(request(key("issue-b", "kuroko"), "MOK-2", "b"))
            .await,
        Ok(Submission::Accepted)
    );

    assert_eq!(next_start(&mut starts).await, key("issue-b", "kuroko"));

    let tasks = engine.snapshot().await.unwrap();
    let failed = tasks
        .iter()
        .find(|task| task.key == key("issue-a", "kuroko"))
        .unwrap();
    match &failed.state {
        TaskState::Failed(done) => {
            assert_eq!(done.outcome, Outcome::Failure(FailureReason::Panicked));
            assert_eq!(done.at, Millis(5));
        }
        other => panic!("expected a failure, got {other:?}"),
    }
    let reused = tasks
        .iter()
        .find(|task| task.key == key("issue-b", "kuroko"))
        .unwrap();
    assert!(matches!(reused.state, TaskState::Dispatched));

    gate.send(true).unwrap();
    drop(engine);
    timeout(Duration::from_secs(1), handle)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_full_queue_returns_full() {
    let (gate, gate_rx) = watch::channel(false);
    let (transport, mut starts) = Gated::new(gate_rx);
    let (engine, handle) = spawn(transport, limits(1, 1), Arc::new(FixedClock(Millis(10))));

    assert_eq!(
        engine
            .submit(request(key("issue-a", "kuroko"), "MOK-1", "a"))
            .await,
        Ok(Submission::Accepted)
    );
    assert_eq!(next_start(&mut starts).await, key("issue-a", "kuroko"));
    assert_eq!(
        engine
            .submit(request(key("issue-b", "kuroko"), "MOK-2", "b"))
            .await,
        Ok(Submission::Accepted)
    );
    assert_eq!(
        engine
            .submit(request(key("issue-c", "kuroko"), "MOK-3", "c"))
            .await,
        Ok(Submission::Full)
    );

    gate.send(true).unwrap();
    drop(engine);
    timeout(Duration::from_secs(1), handle)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn snapshot_reflects_the_book() {
    let (gate, gate_rx) = watch::channel(false);
    let (transport, mut starts) = Gated::new(gate_rx);
    let (engine, handle) = spawn(transport, limits(1, 4), Arc::new(FixedClock(Millis(10))));

    let admitted = key("issue-a", "kuroko");
    assert_eq!(
        engine
            .submit(request(admitted.clone(), "MOK-1", "the prompt"))
            .await,
        Ok(Submission::Accepted)
    );
    next_start(&mut starts).await;

    let tasks = engine.snapshot().await.unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].key, admitted);
    assert_eq!(tasks[0].spec.identifier, "MOK-1");
    assert_eq!(tasks[0].prompt, "the prompt");
    assert!(matches!(tasks[0].state, TaskState::Dispatched));

    gate.send(true).unwrap();
    drop(engine);
    timeout(Duration::from_secs(1), handle)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn shutdown_joins_only_after_the_in_flight_dispatch_finishes() {
    let (gate, gate_rx) = watch::channel(false);
    let (transport, mut starts) = Gated::new(gate_rx);
    let (engine, handle) = spawn(
        transport.clone(),
        limits(1, 4),
        Arc::new(FixedClock(Millis(10))),
    );

    assert_eq!(
        engine
            .submit(request(key("issue-a", "kuroko"), "MOK-1", "a"))
            .await,
        Ok(Submission::Accepted)
    );
    next_start(&mut starts).await;

    drop(engine);
    for _ in 0..32 {
        tokio::task::yield_now().await;
    }
    assert!(!handle.is_finished());

    gate.send(true).unwrap();
    timeout(Duration::from_secs(1), handle)
        .await
        .unwrap()
        .unwrap();
    assert!(transport.completed.load(Ordering::SeqCst));
}

#[tokio::test(start_paused = true)]
async fn a_queued_task_stays_unstarted_after_stop() {
    let (gate, gate_rx) = watch::channel(false);
    let (transport, mut starts) = Gated::new(gate_rx);
    let (engine, handle) = spawn(
        transport.clone(),
        limits(1, 4),
        Arc::new(FixedClock(Millis(10))),
    );

    assert_eq!(
        engine
            .submit(request(key("issue-a", "kuroko"), "MOK-1", "a"))
            .await,
        Ok(Submission::Accepted)
    );
    assert_eq!(next_start(&mut starts).await, key("issue-a", "kuroko"));
    assert_eq!(
        engine
            .submit(request(key("issue-b", "kuroko"), "MOK-2", "b"))
            .await,
        Ok(Submission::Accepted)
    );

    drop(engine);
    for _ in 0..32 {
        tokio::task::yield_now().await;
    }
    assert!(!handle.is_finished());

    gate.send(true).unwrap();
    timeout(Duration::from_secs(1), handle)
        .await
        .unwrap()
        .unwrap();

    assert!(transport.completed.load(Ordering::SeqCst));
    assert_eq!(transport.calls.load(Ordering::SeqCst), 1);
    assert!(starts.try_recv().is_err());
}

#[tokio::test(start_paused = true)]
async fn a_submit_after_drop_returns_engine_down() {
    let (gate, gate_rx) = watch::channel(false);
    let (transport, _starts) = Gated::new(gate_rx);
    let (engine, handle) = spawn(transport, limits(1, 4), Arc::new(FixedClock(Millis(10))));

    let probe = engine.clone();
    drop(engine);

    assert_eq!(
        probe
            .submit(request(key("issue-a", "kuroko"), "MOK-1", "a"))
            .await,
        Err(EngineDown)
    );
    assert_eq!(
        probe.withdraw(key("issue-a", "kuroko")).await,
        Err(EngineDown)
    );
    assert_eq!(probe.snapshot().await, Err(EngineDown));

    gate.send(true).unwrap();
    timeout(Duration::from_secs(1), handle)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn the_engine_stamps_done_at_from_its_clock() {
    let (gate, gate_rx) = watch::channel(false);
    let (transport, mut starts) = Gated::new(gate_rx);
    let (engine, handle) = spawn(transport, limits(1, 4), Arc::new(FixedClock(Millis(42))));

    let first = key("issue-a", "kuroko");
    assert_eq!(
        engine.submit(request(first.clone(), "MOK-1", "a")).await,
        Ok(Submission::Accepted)
    );
    next_start(&mut starts).await;
    assert_eq!(
        engine
            .submit(request(key("issue-b", "kuroko"), "MOK-2", "b"))
            .await,
        Ok(Submission::Accepted)
    );

    gate.send(true).unwrap();
    // The second dispatch starts only after the first `Finished` has been applied.
    next_start(&mut starts).await;

    let tasks = engine.snapshot().await.unwrap();
    let finished = tasks.iter().find(|task| task.key == first).unwrap();
    match &finished.state {
        TaskState::Succeeded(done) => assert_eq!(done.at, Millis(42)),
        other => panic!("expected success, got {other:?}"),
    }

    drop(engine);
    timeout(Duration::from_secs(1), handle)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn withdraw_removes_a_queued_task() {
    let (gate, gate_rx) = watch::channel(false);
    let (transport, mut starts) = Gated::new(gate_rx);
    let (engine, handle) = spawn(transport, limits(1, 4), Arc::new(FixedClock(Millis(10))));

    let running = key("issue-a", "kuroko");
    assert_eq!(
        engine.submit(request(running.clone(), "MOK-1", "a")).await,
        Ok(Submission::Accepted)
    );
    next_start(&mut starts).await;
    assert_eq!(
        engine
            .submit(request(key("issue-b", "kuroko"), "MOK-2", "b"))
            .await,
        Ok(Submission::Accepted)
    );

    engine.withdraw(key("issue-b", "kuroko")).await.unwrap();

    let tasks = engine.snapshot().await.unwrap();
    assert!(tasks.iter().any(|task| task.key == running));
    assert!(
        !tasks
            .iter()
            .any(|task| task.key == key("issue-b", "kuroko"))
    );

    gate.send(true).unwrap();
    drop(engine);
    timeout(Duration::from_secs(1), handle)
        .await
        .unwrap()
        .unwrap();
}
