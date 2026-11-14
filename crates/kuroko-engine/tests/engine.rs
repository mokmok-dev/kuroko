#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use kuroko_core::{
    Book, Clock, Dispatch, Done, Event, FailureReason, IssueId, Journal, JournalError, LabelName,
    Limits, Millis, Outcome, Source, Submission, TaskKey, TaskRequest, TaskSpec, TaskState,
    TriggerId, apply,
};
use kuroko_engine::{BoxFuture, EngineError, Transport, spawn};
use tokio::sync::{mpsc, watch};
use tokio::time::timeout;

struct FixedClock(Millis);

impl Clock for FixedClock {
    fn now_ms(&self) -> Millis {
        self.0
    }
}

struct NoopJournal;

impl Journal for NoopJournal {
    fn persist(&self, _book: &Book) -> Result<(), JournalError> {
        Ok(())
    }
}

struct FailingJournal;

impl Journal for FailingJournal {
    fn persist(&self, _book: &Book) -> Result<(), JournalError> {
        Err(JournalError::new("disk full"))
    }
}

struct FlakyJournal {
    failing: AtomicBool,
}

impl FlakyJournal {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            failing: AtomicBool::new(false),
        })
    }
}

impl Journal for FlakyJournal {
    fn persist(&self, _book: &Book) -> Result<(), JournalError> {
        if self.failing.load(Ordering::SeqCst) {
            return Err(JournalError::new("disk full"));
        }
        Ok(())
    }
}

/// Fails the persist after `allowed` successful writes.
struct FailAfter {
    allowed: usize,
    attempts: AtomicUsize,
}

impl FailAfter {
    fn new(allowed: usize) -> Arc<Self> {
        Arc::new(Self {
            allowed,
            attempts: AtomicUsize::new(0),
        })
    }
}

impl Journal for FailAfter {
    fn persist(&self, _book: &Book) -> Result<(), JournalError> {
        let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
        if attempt >= self.allowed {
            return Err(JournalError::new("disk full"));
        }
        Ok(())
    }
}

#[derive(Default)]
struct RecordingJournal {
    saved: std::sync::Mutex<Vec<Book>>,
}

impl Journal for RecordingJournal {
    fn persist(&self, book: &Book) -> Result<(), JournalError> {
        self.saved
            .lock()
            .map_err(|_| JournalError::new("poisoned"))?
            .push(book.clone());
        Ok(())
    }
}

impl RecordingJournal {
    fn last(&self) -> Option<Book> {
        self.saved.lock().ok()?.last().cloned()
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
    let (engine, handle) = spawn(
        transport,
        Book::new(limits(1, 4)),
        Arc::new(FixedClock(Millis(10))),
        Arc::new(NoopJournal),
    );

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
        Book::new(limits(1, 4)),
        Arc::new(FixedClock(Millis(10))),
        Arc::new(NoopJournal),
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
        .unwrap()
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_panicking_transport_settles_as_panicked_and_frees_the_slot() {
    let (gate, gate_rx) = watch::channel(false);
    let (transport, mut starts) = Exploding::new(gate_rx);
    let (engine, handle) = spawn(
        transport,
        Book::new(limits(1, 4)),
        Arc::new(FixedClock(Millis(5))),
        Arc::new(NoopJournal),
    );

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
        .unwrap()
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_full_queue_returns_full() {
    let (gate, gate_rx) = watch::channel(false);
    let (transport, mut starts) = Gated::new(gate_rx);
    let (engine, handle) = spawn(
        transport,
        Book::new(limits(1, 1)),
        Arc::new(FixedClock(Millis(10))),
        Arc::new(NoopJournal),
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
        .unwrap()
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn snapshot_reflects_the_book() {
    let (gate, gate_rx) = watch::channel(false);
    let (transport, mut starts) = Gated::new(gate_rx);
    let (engine, handle) = spawn(
        transport,
        Book::new(limits(1, 4)),
        Arc::new(FixedClock(Millis(10))),
        Arc::new(NoopJournal),
    );

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
        .unwrap()
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn shutdown_joins_only_after_the_in_flight_dispatch_finishes() {
    let (gate, gate_rx) = watch::channel(false);
    let (transport, mut starts) = Gated::new(gate_rx);
    let (engine, handle) = spawn(
        transport.clone(),
        Book::new(limits(1, 4)),
        Arc::new(FixedClock(Millis(10))),
        Arc::new(NoopJournal),
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
        Book::new(limits(1, 4)),
        Arc::new(FixedClock(Millis(10))),
        Arc::new(NoopJournal),
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
    let (engine, handle) = spawn(
        transport,
        Book::new(limits(1, 4)),
        Arc::new(FixedClock(Millis(10))),
        Arc::new(NoopJournal),
    );

    let probe = engine.clone();
    drop(engine);

    assert_eq!(
        probe
            .submit(request(key("issue-a", "kuroko"), "MOK-1", "a"))
            .await,
        Err(EngineError::Down)
    );
    assert_eq!(
        probe.withdraw(key("issue-a", "kuroko")).await,
        Err(EngineError::Down)
    );
    assert_eq!(probe.snapshot().await, Err(EngineError::Down));

    gate.send(true).unwrap();
    timeout(Duration::from_secs(1), handle)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn the_engine_stamps_done_at_from_its_clock() {
    let (gate, gate_rx) = watch::channel(false);
    let (transport, mut starts) = Gated::new(gate_rx);
    let (engine, handle) = spawn(
        transport,
        Book::new(limits(1, 4)),
        Arc::new(FixedClock(Millis(42))),
        Arc::new(NoopJournal),
    );

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
        .unwrap()
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn withdraw_leaves_a_tombstone() {
    let (gate, gate_rx) = watch::channel(false);
    let (transport, mut starts) = Gated::new(gate_rx);
    let (engine, handle) = spawn(
        transport,
        Book::new(limits(1, 4)),
        Arc::new(FixedClock(Millis(10))),
        Arc::new(NoopJournal),
    );

    let running = key("issue-a", "kuroko");
    assert_eq!(
        engine.submit(request(running.clone(), "MOK-1", "a")).await,
        Ok(Submission::Accepted)
    );
    next_start(&mut starts).await;
    let queued = key("issue-b", "kuroko");
    assert_eq!(
        engine.submit(request(queued.clone(), "MOK-2", "b")).await,
        Ok(Submission::Accepted)
    );

    engine.withdraw(queued.clone()).await.unwrap();

    let tasks = engine.snapshot().await.unwrap();
    assert!(tasks.iter().any(|task| task.key == running));
    assert_eq!(
        tasks
            .iter()
            .find(|task| task.key == queued)
            .map(|task| &task.state),
        Some(&TaskState::Withdrawn)
    );
    assert_eq!(
        engine.submit(request(queued, "MOK-2", "b")).await,
        Ok(Submission::Duplicate)
    );

    gate.send(true).unwrap();
    drop(engine);
    timeout(Duration::from_secs(1), handle)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_store_failure_refuses_admission_without_dispatching() {
    let (_gate, gate_rx) = watch::channel(false);
    let (transport, mut starts) = Gated::new(gate_rx);
    let (engine, handle) = spawn(
        transport.clone(),
        Book::new(limits(1, 4)),
        Arc::new(FixedClock(Millis(10))),
        Arc::new(FailingJournal),
    );

    assert_eq!(
        engine
            .submit(request(key("issue-a", "kuroko"), "MOK-1", "a"))
            .await,
        Err(EngineError::Store)
    );
    assert!(starts.try_recv().is_err());
    assert_eq!(transport.calls.load(Ordering::SeqCst), 0);
    assert_eq!(engine.snapshot().await.unwrap(), Vec::new());

    drop(engine);
    timeout(Duration::from_secs(1), handle)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_store_failure_on_withdraw_keeps_the_task() {
    let (gate, gate_rx) = watch::channel(false);
    let (transport, mut starts) = Gated::new(gate_rx);
    let journal = FlakyJournal::new();
    let (engine, handle) = spawn(
        transport,
        Book::new(limits(1, 4)),
        Arc::new(FixedClock(Millis(10))),
        journal.clone(),
    );

    assert_eq!(
        engine
            .submit(request(key("issue-a", "kuroko"), "MOK-1", "a"))
            .await,
        Ok(Submission::Accepted)
    );
    next_start(&mut starts).await;
    let queued = key("issue-b", "kuroko");
    assert_eq!(
        engine.submit(request(queued.clone(), "MOK-2", "b")).await,
        Ok(Submission::Accepted)
    );

    journal.failing.store(true, Ordering::SeqCst);
    assert_eq!(
        engine.withdraw(queued.clone()).await,
        Err(EngineError::Store)
    );
    let tasks = engine.snapshot().await.unwrap();
    assert_eq!(
        tasks
            .iter()
            .find(|task| task.key == queued)
            .map(|task| &task.state),
        Some(&TaskState::Queued)
    );

    journal.failing.store(false, Ordering::SeqCst);
    gate.send(true).unwrap();
    drop(engine);
    timeout(Duration::from_secs(1), handle)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn recovery_pending_blocks_the_issue_without_running_it() {
    let mut crashed = Book::new(limits(1, 4));
    let interrupted = key("issue-a", "kuroko");
    let same_issue = key("issue-a", "label");
    let other_issue = key("issue-b", "kuroko");
    let _ = apply(
        &mut crashed,
        Event::Admit(request(interrupted.clone(), "MOK-1", "a")),
    );
    let _ = apply(
        &mut crashed,
        Event::Admit(request(same_issue.clone(), "MOK-1", "b")),
    );
    let _ = apply(
        &mut crashed,
        Event::Admit(request(other_issue.clone(), "MOK-2", "c")),
    );
    let recovered = Book::reconcile(
        limits(1, 4),
        crashed.tasks().cloned().collect(),
        crashed.pending_keys().cloned().collect(),
    );

    let (gate, gate_rx) = watch::channel(false);
    let (transport, mut starts) = Gated::new(gate_rx);
    let (engine, handle) = spawn(
        transport.clone(),
        recovered,
        Arc::new(FixedClock(Millis(10))),
        Arc::new(NoopJournal),
    );

    assert_eq!(transport.calls.load(Ordering::SeqCst), 0);
    let tasks = engine.snapshot().await.unwrap();
    assert_eq!(
        tasks
            .iter()
            .find(|task| task.key == interrupted)
            .map(|task| &task.state),
        Some(&TaskState::RecoveryPending)
    );

    assert_eq!(
        engine
            .submit(request(key("issue-c", "kuroko"), "MOK-3", "d"))
            .await,
        Ok(Submission::Accepted)
    );
    assert_eq!(next_start(&mut starts).await, other_issue);
    assert!(starts.try_recv().is_err());
    assert_eq!(transport.calls.load(Ordering::SeqCst), 1);

    gate.send(true).unwrap();
    drop(engine);
    timeout(Duration::from_secs(1), handle)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn startup_resumes_the_restored_queue() {
    let mut crashed = Book::new(limits(1, 4));
    let interrupted = key("issue-a", "kuroko");
    let queued = key("issue-b", "kuroko");
    let _ = apply(
        &mut crashed,
        Event::Admit(request(interrupted.clone(), "MOK-1", "a")),
    );
    let _ = apply(
        &mut crashed,
        Event::Admit(request(queued.clone(), "MOK-2", "b")),
    );
    let recovered = Book::reconcile(
        limits(1, 4),
        crashed.tasks().cloned().collect(),
        crashed.pending_keys().cloned().collect(),
    );

    let (gate, gate_rx) = watch::channel(false);
    let (transport, mut starts) = Gated::new(gate_rx);
    let (engine, handle) = spawn(
        transport.clone(),
        recovered,
        Arc::new(FixedClock(Millis(10))),
        Arc::new(NoopJournal),
    );

    assert_eq!(next_start(&mut starts).await, queued);
    assert_eq!(transport.calls.load(Ordering::SeqCst), 1);
    let tasks = engine.snapshot().await.unwrap();
    assert_eq!(
        tasks
            .iter()
            .find(|task| task.key == interrupted)
            .map(|task| &task.state),
        Some(&TaskState::RecoveryPending)
    );

    gate.send(true).unwrap();
    drop(engine);
    timeout(Duration::from_secs(1), handle)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_failed_resume_persist_stops_the_engine_before_dispatching() {
    let mut crashed = Book::new(limits(1, 4));
    let _ = apply(
        &mut crashed,
        Event::Admit(request(key("issue-a", "kuroko"), "MOK-1", "a")),
    );
    let _ = apply(
        &mut crashed,
        Event::Admit(request(key("issue-b", "kuroko"), "MOK-2", "b")),
    );
    let recovered = Book::reconcile(
        limits(1, 4),
        crashed.tasks().cloned().collect(),
        crashed.pending_keys().cloned().collect(),
    );

    let (_gate, gate_rx) = watch::channel(false);
    let (transport, mut starts) = Gated::new(gate_rx);
    let (engine, handle) = spawn(
        transport.clone(),
        recovered,
        Arc::new(FixedClock(Millis(10))),
        Arc::new(FailingJournal),
    );

    let outcome = timeout(Duration::from_secs(1), handle)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(outcome, Err(EngineError::Store));
    assert!(starts.try_recv().is_err());
    assert_eq!(transport.calls.load(Ordering::SeqCst), 0);
    drop(engine);
}

#[tokio::test(start_paused = true)]
async fn a_failed_completion_persist_stops_the_engine() {
    let (gate, gate_rx) = watch::channel(false);
    let (transport, mut starts) = Gated::new(gate_rx);
    let journal = FailAfter::new(1);
    let (engine, handle) = spawn(
        transport,
        Book::new(limits(1, 4)),
        Arc::new(FixedClock(Millis(10))),
        journal,
    );

    assert_eq!(
        engine
            .submit(request(key("issue-a", "kuroko"), "MOK-1", "a"))
            .await,
        Ok(Submission::Accepted)
    );
    next_start(&mut starts).await;
    gate.send(true).unwrap();

    let outcome = timeout(Duration::from_secs(1), handle)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(outcome, Err(EngineError::Store));
    assert_eq!(engine.snapshot().await, Err(EngineError::Down));
}

#[tokio::test(start_paused = true)]
async fn shutdown_persists_the_result_without_starting_the_queue() {
    let (gate, gate_rx) = watch::channel(false);
    let (transport, mut starts) = Gated::new(gate_rx);
    let journal = Arc::new(RecordingJournal::default());
    let (engine, handle) = spawn(
        transport,
        Book::new(limits(1, 4)),
        Arc::new(FixedClock(Millis(10))),
        journal.clone(),
    );

    let running = key("issue-a", "kuroko");
    let queued = key("issue-b", "kuroko");
    assert_eq!(
        engine.submit(request(running.clone(), "MOK-1", "a")).await,
        Ok(Submission::Accepted)
    );
    next_start(&mut starts).await;
    assert_eq!(
        engine.submit(request(queued.clone(), "MOK-2", "b")).await,
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
        .unwrap()
        .unwrap();

    let saved = journal.last().expect("a persisted book");
    assert!(matches!(
        saved.task(&running).map(|task| &task.state),
        Some(TaskState::Succeeded(_))
    ));
    assert_eq!(
        saved.task(&queued).map(|task| &task.state),
        Some(&TaskState::Queued)
    );
    assert_eq!(saved.in_flight(), 0);
    assert_eq!(saved.pending_keys().count(), 1);
}
