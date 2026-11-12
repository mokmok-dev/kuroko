//! Async runtime: `Transport` trait with boxed futures, `spawn`, `Engine`, shutdown.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use kuroko_core::{
    Book, Clock, Cmd, Dispatch, Done, Event, FailureReason, Limits, Outcome, Submission, Task,
    TaskKey, TaskRequest, apply,
};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::{Id, JoinHandle, JoinSet};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub trait Transport: Send + Sync {
    fn run<'a>(&'a self, job: &'a Dispatch) -> BoxFuture<'a, Done>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EngineDown;

const COMMAND_CAPACITY: usize = 64;
const EVENT_CAPACITY: usize = 64;

enum Command {
    Submit {
        request: TaskRequest,
        reply: oneshot::Sender<Submission>,
    },
    Withdraw {
        key: TaskKey,
        reply: oneshot::Sender<()>,
    },
    Snapshot {
        reply: oneshot::Sender<Vec<Task>>,
    },
}

/// A handle to the engine loop.
///
/// Every handle is a shutdown trigger: dropping one flips the stop watch, so the
/// daemon holds exactly one. Callers that need to observe [`EngineDown`] after a drop
/// keep a second clone.
#[derive(Clone)]
pub struct Engine {
    commands: mpsc::Sender<Command>,
    stop: watch::Sender<bool>,
}

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
    }
}

impl Engine {
    /// Submits `request` and awaits the book's verdict.
    ///
    /// # Errors
    ///
    /// Returns [`EngineDown`] once shutdown has begun.
    pub async fn submit(&self, request: TaskRequest) -> Result<Submission, EngineDown> {
        if *self.stop.borrow() {
            return Err(EngineDown);
        }
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::Submit { request, reply })
            .await
            .map_err(|_| EngineDown)?;
        response.await.map_err(|_| EngineDown)
    }

    /// Withdraws the queued task for `key`, if there is one.
    ///
    /// # Errors
    ///
    /// Returns [`EngineDown`] once shutdown has begun.
    pub async fn withdraw(&self, key: TaskKey) -> Result<(), EngineDown> {
        if *self.stop.borrow() {
            return Err(EngineDown);
        }
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::Withdraw { key, reply })
            .await
            .map_err(|_| EngineDown)?;
        response.await.map_err(|_| EngineDown)
    }

    /// Returns every task in the book.
    ///
    /// # Errors
    ///
    /// Returns [`EngineDown`] once shutdown has begun.
    pub async fn snapshot(&self) -> Result<Vec<Task>, EngineDown> {
        if *self.stop.borrow() {
            return Err(EngineDown);
        }
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::Snapshot { reply })
            .await
            .map_err(|_| EngineDown)?;
        response.await.map_err(|_| EngineDown)
    }
}

/// Starts the engine loop, which owns the [`Book`] until its handle is dropped.
pub fn spawn(
    transport: Arc<dyn Transport>,
    limits: Limits,
    clock: Arc<dyn Clock>,
) -> (Engine, JoinHandle<()>) {
    let (commands, inbox) = mpsc::channel(COMMAND_CAPACITY);
    let (stop, stopped) = watch::channel(false);
    let handle = tokio::spawn(run(transport, limits, clock, inbox, stopped));
    (Engine { commands, stop }, handle)
}

async fn run(
    transport: Arc<dyn Transport>,
    limits: Limits,
    clock: Arc<dyn Clock>,
    mut commands: mpsc::Receiver<Command>,
    mut stop: watch::Receiver<bool>,
) {
    let mut book = Book::new(limits);
    let mut dispatches = JoinSet::new();
    let mut owners: HashMap<Id, TaskKey> = HashMap::new();
    let (events, mut finished) = mpsc::channel(EVENT_CAPACITY);
    let mut accepting = true;
    let mut open = true;

    loop {
        if !accepting && book.in_flight() == 0 {
            break;
        }
        tokio::select! {
            biased;
            changed = stop.changed(), if accepting => {
                accepting = changed.is_ok() && !*stop.borrow();
            }
            command = commands.recv(), if open => {
                match command {
                    None => open = false,
                    Some(command) => {
                        if accepting {
                            handle_command(command, &mut book, &transport, &events, &mut dispatches, &mut owners);
                        }
                    }
                }
            }
            Some(result) = dispatches.join_next_with_id() => {
                match result {
                    Ok((id, ())) => {
                        owners.remove(&id);
                    }
                    Err(error) => {
                        if let Some(key) = owners.remove(&error.id()) {
                            let done = Done {
                                at: clock.now_ms(),
                                outcome: Outcome::Failure(FailureReason::Panicked),
                                output: String::new(),
                                log: String::new(),
                            };
                            let _ = apply_step(
                                &mut book,
                                Event::Finished { key, done },
                                &transport,
                                &events,
                                &mut dispatches,
                                &mut owners,
                            );
                        }
                    }
                }
            }
            Some(event) = finished.recv() => {
                if let Event::Finished { key, mut done } = event {
                    done.at = clock.now_ms();
                    let _ = apply_step(
                        &mut book,
                        Event::Finished { key, done },
                        &transport,
                        &events,
                        &mut dispatches,
                        &mut owners,
                    );
                }
            }
        }
    }
}

fn handle_command(
    command: Command,
    book: &mut Book,
    transport: &Arc<dyn Transport>,
    events: &mpsc::Sender<Event>,
    dispatches: &mut JoinSet<()>,
    owners: &mut HashMap<Id, TaskKey>,
) {
    match command {
        Command::Submit { request, reply } => {
            let submission = apply_step(
                book,
                Event::Admit(request),
                transport,
                events,
                dispatches,
                owners,
            );
            if let Some(submission) = submission {
                let _ = reply.send(submission);
            }
        }
        Command::Withdraw { key, reply } => {
            let _ = apply_step(
                book,
                Event::Withdraw(key),
                transport,
                events,
                dispatches,
                owners,
            );
            let _ = reply.send(());
        }
        Command::Snapshot { reply } => {
            let _ = reply.send(book.tasks().cloned().collect());
        }
    }
}

fn apply_step(
    book: &mut Book,
    event: Event,
    transport: &Arc<dyn Transport>,
    events: &mpsc::Sender<Event>,
    dispatches: &mut JoinSet<()>,
    owners: &mut HashMap<Id, TaskKey>,
) -> Option<Submission> {
    let step = apply(book, event);
    for cmd in step.cmds {
        let Cmd::Dispatch(job) = cmd;
        let owner = job.key.clone();
        let transport = Arc::clone(transport);
        let events = events.clone();
        let handle = dispatches.spawn(async move {
            let done = transport.run(&job).await;
            let _ = events.send(Event::Finished { key: job.key, done }).await;
        });
        owners.insert(handle.id(), owner);
    }
    step.submission
}
