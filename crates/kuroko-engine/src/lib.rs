//! Async runtime: `Transport` trait with boxed futures, `spawn`, `Engine`, shutdown.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use kuroko_core::{
    Book, Clock, Cmd, Dispatch, Done, Event, FailureReason, Journal, JournalError, Outcome,
    Submission, Task, TaskKey, TaskRequest, apply,
};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::{Id, JoinHandle, JoinSet};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub trait Transport: Send + Sync {
    fn run<'a>(&'a self, job: &'a Dispatch) -> BoxFuture<'a, Done>;
}

/// Why an engine call did not complete.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EngineError {
    Down,
    /// The candidate state could not be persisted, so nothing was dispatched.
    Store,
    /// `apply` broke its contract for the event the engine sent it.
    Invariant,
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Down => f.write_str("the engine is down"),
            Self::Store => f.write_str("the task store failed"),
            Self::Invariant => f.write_str("the engine reached an invalid state"),
        }
    }
}

impl std::error::Error for EngineError {}

const COMMAND_CAPACITY: usize = 64;
const EVENT_CAPACITY: usize = 64;

enum Command {
    Submit {
        request: TaskRequest,
        reply: oneshot::Sender<Result<Submission, EngineError>>,
    },
    Withdraw {
        key: TaskKey,
        reply: oneshot::Sender<Result<(), EngineError>>,
    },
    Snapshot {
        reply: oneshot::Sender<Vec<Task>>,
    },
}

/// A handle to the engine loop.
///
/// Every handle is a shutdown trigger: dropping one flips the stop watch, so the
/// daemon holds exactly one. Callers that need to observe [`EngineError`] after a drop
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
    /// Returns [`EngineError::Down`] once shutdown has begun, and
    /// [`EngineError::Store`] when the admission could not be persisted.
    pub async fn submit(&self, request: TaskRequest) -> Result<Submission, EngineError> {
        if *self.stop.borrow() {
            return Err(EngineError::Down);
        }
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::Submit { request, reply })
            .await
            .map_err(|_| EngineError::Down)?;
        response.await.map_err(|_| EngineError::Down)?
    }

    /// Withdraws the queued or recovery-pending task for `key`, if there is one.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::Down`] once shutdown has begun, and
    /// [`EngineError::Store`] when the withdrawal could not be persisted.
    pub async fn withdraw(&self, key: TaskKey) -> Result<(), EngineError> {
        if *self.stop.borrow() {
            return Err(EngineError::Down);
        }
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::Withdraw { key, reply })
            .await
            .map_err(|_| EngineError::Down)?;
        response.await.map_err(|_| EngineError::Down)?
    }

    /// Returns every task in the book.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::Down`] once shutdown has begun.
    pub async fn snapshot(&self) -> Result<Vec<Task>, EngineError> {
        if *self.stop.borrow() {
            return Err(EngineError::Down);
        }
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::Snapshot { reply })
            .await
            .map_err(|_| EngineError::Down)?;
        response.await.map_err(|_| EngineError::Down)
    }
}

/// Starts the engine loop, which owns `book` until its handle is dropped.
pub fn spawn(
    transport: Arc<dyn Transport>,
    book: Book,
    clock: Arc<dyn Clock>,
    journal: Arc<dyn Journal>,
) -> (Engine, JoinHandle<Result<(), EngineError>>) {
    let (commands, inbox) = mpsc::channel(COMMAND_CAPACITY);
    let (stop, stopped) = watch::channel(false);
    let handle = tokio::spawn(run(transport, book, clock, journal, inbox, stopped));
    (Engine { commands, stop }, handle)
}

struct Runtime {
    transport: Arc<dyn Transport>,
    journal: Arc<dyn Journal>,
    events: mpsc::Sender<Event>,
}

struct Running {
    tasks: JoinSet<()>,
    owners: HashMap<Id, TaskKey>,
}

async fn run(
    transport: Arc<dyn Transport>,
    mut book: Book,
    clock: Arc<dyn Clock>,
    journal: Arc<dyn Journal>,
    mut commands: mpsc::Receiver<Command>,
    mut stop: watch::Receiver<bool>,
) -> Result<(), EngineError> {
    let mut running = Running {
        tasks: JoinSet::new(),
        owners: HashMap::new(),
    };
    let (events, mut finished) = mpsc::channel(EVENT_CAPACITY);
    let runtime = Runtime {
        transport,
        journal,
        events,
    };

    apply_step(&mut book, Event::Resume, &mut running, &runtime)
        .map(|_| ())
        .map_err(|error| {
            log::error!("cannot persist the resumed queue: {error}");
            EngineError::Store
        })?;

    let mut accepting = true;
    let mut open = true;

    loop {
        // The book is frozen once accepting stops, so the spawned set is the
        // only count of work that still has to be joined, and a task enqueues its
        // finish before it ends, so a non-empty channel still has results to persist.
        if !accepting && running.tasks.is_empty() && finished.is_empty() {
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
                            handle_command(command, &mut book, &mut running, &runtime);
                        }
                    }
                }
            }
            Some(result) = running.tasks.join_next_with_id() => {
                match result {
                    Ok((id, ())) => {
                        running.owners.remove(&id);
                    }
                    Err(error) => {
                        if let Some(key) = running.owners.remove(&error.id()) {
                            let done = Done {
                                at: clock.now_ms(),
                                outcome: Outcome::Failure(FailureReason::Panicked),
                                output: String::new(),
                                log: String::new(),
                            };
                            settle(&mut book, key, done, accepting, &mut running, &runtime)?;
                        }
                    }
                }
            }
            Some(event) = finished.recv() => {
                if let Event::Finished { key, mut done } = event {
                    done.at = clock.now_ms();
                    settle(&mut book, key, done, accepting, &mut running, &runtime)?;
                }
            }
        }
    }
    Ok(())
}

fn settle(
    book: &mut Book,
    key: TaskKey,
    done: Done,
    accepting: bool,
    running: &mut Running,
    runtime: &Runtime,
) -> Result<(), EngineError> {
    // `Settled` records the result without letting `drain` start queued work that
    // shutdown would then have to wait for.
    let event = if accepting {
        Event::Finished { key, done }
    } else {
        Event::Settled { key, done }
    };
    apply_step(book, event, running, runtime)
        .map(|_| ())
        .map_err(|error| {
            // A completion that cannot be persisted is not retried: the engine exits
            // and the next start reconciles the persisted `Dispatched` row.
            log::error!("cannot persist a finished task: {error}");
            EngineError::Store
        })
}

fn handle_command(command: Command, book: &mut Book, running: &mut Running, runtime: &Runtime) {
    match command {
        Command::Submit { request, reply } => {
            let response = apply_step(book, Event::Admit(request), running, runtime)
                .map_err(|error| {
                    log::error!("cannot persist an admission: {error}");
                    EngineError::Store
                })
                .and_then(|submission| submission.ok_or(EngineError::Invariant));
            let _ = reply.send(response);
        }
        Command::Withdraw { key, reply } => {
            let response = apply_step(book, Event::Withdraw(key), running, runtime)
                .map(|_| ())
                .map_err(|error| {
                    log::error!("cannot persist a withdrawal: {error}");
                    EngineError::Store
                });
            let _ = reply.send(response);
        }
        Command::Snapshot { reply } => {
            let _ = reply.send(book.tasks().cloned().collect());
        }
    }
}

fn apply_step(
    book: &mut Book,
    event: Event,
    running: &mut Running,
    runtime: &Runtime,
) -> Result<Option<Submission>, JournalError> {
    let mut candidate = book.clone();
    let step = apply(&mut candidate, event);
    if candidate != *book {
        runtime.journal.persist(&candidate)?;
        *book = candidate;
    }
    for cmd in step.cmds {
        let Cmd::Dispatch(job) = cmd;
        let owner = job.key.clone();
        let transport = Arc::clone(&runtime.transport);
        let events = runtime.events.clone();
        let handle = running.tasks.spawn(async move {
            let done = transport.run(&job).await;
            let _ = events.send(Event::Finished { key: job.key, done }).await;
        });
        running.owners.insert(handle.id(), owner);
    }
    Ok(step.submission)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::EngineError;

    #[test]
    fn engine_errors_describe_the_failure() {
        assert_eq!(EngineError::Down.to_string(), "the engine is down");
        assert_eq!(EngineError::Store.to_string(), "the task store failed");
        assert_eq!(
            EngineError::Invariant.to_string(),
            "the engine reached an invalid state"
        );
    }
}
