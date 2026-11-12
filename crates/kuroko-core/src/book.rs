use std::collections::{BTreeMap, VecDeque};

use crate::task::{
    Cmd, Dispatch, Done, Event, IssueId, Limits, Outcome, Step, Submission, Task, TaskKey,
    TaskRequest, TaskState,
};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Book {
    tasks: BTreeMap<TaskKey, Task>,
    pending: VecDeque<TaskKey>,
    in_flight: usize,
    limits: Limits,
}

impl Book {
    #[must_use]
    pub const fn new(limits: Limits) -> Self {
        Self {
            tasks: BTreeMap::new(),
            pending: VecDeque::new(),
            in_flight: 0,
            limits,
        }
    }

    pub fn tasks(&self) -> impl Iterator<Item = &Task> + '_ {
        self.tasks.values()
    }

    #[must_use]
    pub fn task(&self, key: &TaskKey) -> Option<&Task> {
        self.tasks.get(key)
    }

    pub fn pending_keys(&self) -> impl Iterator<Item = &TaskKey> + '_ {
        self.pending.iter()
    }

    #[must_use]
    pub const fn in_flight(&self) -> usize {
        self.in_flight
    }
}

/// The only mutator of `Book` internals. Pure, deterministic, IO free.
/// The engine loop and the stateright model are its only two callers.
#[must_use]
pub fn apply(book: &mut Book, event: Event) -> Step {
    match event {
        Event::Admit(request) => admit(book, request),
        Event::Withdraw(key) => {
            withdraw(book, &key);
            Step {
                submission: None,
                cmds: Vec::new(),
            }
        }
        Event::Finished { key, done } => finish(book, &key, done),
    }
}

fn admit(book: &mut Book, request: TaskRequest) -> Step {
    if book.tasks.contains_key(&request.key) {
        return Step {
            submission: Some(Submission::Duplicate),
            cmds: Vec::new(),
        };
    }
    if book.pending.len() >= book.limits.max_queued.get() {
        return Step {
            submission: Some(Submission::Full),
            cmds: Vec::new(),
        };
    }
    let key = request.key.clone();
    book.pending.push_back(request.key.clone());
    book.tasks.insert(
        request.key,
        Task {
            key,
            spec: request.spec,
            prompt: request.prompt,
            state: TaskState::Queued,
        },
    );
    let mut cmds = Vec::new();
    drain(book, &mut cmds);
    Step {
        submission: Some(Submission::Accepted),
        cmds,
    }
}

fn withdraw(book: &mut Book, key: &TaskKey) {
    let queued = book
        .tasks
        .get(key)
        .is_some_and(|task| task.state == TaskState::Queued);
    if !queued {
        return;
    }
    let _ = book.tasks.remove(key);
    book.pending.retain(|pending| pending != key);
}

fn finish(book: &mut Book, key: &TaskKey, done: Done) -> Step {
    let dispatched = book
        .tasks
        .get(key)
        .is_some_and(|task| matches!(task.state, TaskState::Dispatched));
    if !dispatched {
        return Step {
            submission: None,
            cmds: Vec::new(),
        };
    }
    if let Some(task) = book.tasks.get_mut(key) {
        task.state = match done.outcome {
            Outcome::Success => TaskState::Succeeded(done),
            Outcome::Failure(_) => TaskState::Failed(done),
        };
    }
    book.in_flight -= 1;
    let mut cmds = Vec::new();
    drain(book, &mut cmds);
    Step {
        submission: None,
        cmds,
    }
}

fn drain(book: &mut Book, cmds: &mut Vec<Cmd>) {
    let capacity = book.limits.in_flight.get().saturating_sub(book.in_flight);
    let mut started = 0;
    let mut deferred = VecDeque::new();
    while started < capacity {
        let Some(key) = book.pending.pop_front() else {
            break;
        };
        if issue_busy(book, &key.issue) {
            deferred.push_back(key);
            continue;
        }
        if let Some(task) = book.tasks.get_mut(&key) {
            task.state = TaskState::Dispatched;
            cmds.push(Cmd::Dispatch(Dispatch {
                key,
                issue: task.spec.identifier.clone(),
                prompt: task.prompt.clone(),
            }));
            book.in_flight += 1;
            started += 1;
        }
    }
    deferred.append(&mut book.pending);
    book.pending = deferred;
}

fn issue_busy(book: &Book, issue: &IssueId) -> bool {
    book.tasks
        .values()
        .any(|task| &task.key.issue == issue && matches!(task.state, TaskState::Dispatched))
}
