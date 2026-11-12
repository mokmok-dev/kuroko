#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroUsize;

use kuroko_core::{
    Book, Cmd, DelegateId, Done, Event, FailureReason, IssueId, LabelName, Limits, Millis, Outcome,
    Source, TaskKey, TaskRequest, TaskSpec, TaskState, TriggerId, apply,
};
use stateright::{Checker, Model, Property};

const KEY_COUNT: usize = 4;
const IDENTIFIERS: [&str; KEY_COUNT] = ["MOK-1", "MOK-1", "MOK-2", "MOK-3"];

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct SysState {
    book: Book,
    tickets: BTreeMap<TaskKey, u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Action {
    Admit(usize),
    Withdraw(usize),
    Finish(usize, bool),
}

#[derive(Clone, Debug)]
struct Kuroko {
    keys: [TaskKey; KEY_COUNT],
    limits: Limits,
}

fn task_key(issue: &str, trigger: TriggerId) -> TaskKey {
    TaskKey {
        source: Source::Linear,
        issue: IssueId::new(issue),
        trigger,
    }
}

fn model() -> Kuroko {
    Kuroko {
        keys: [
            task_key(
                "issue-a",
                TriggerId::Delegate(DelegateId::new("delegate-1")),
            ),
            task_key("issue-a", TriggerId::Label(LabelName::new("kuroko"))),
            task_key(
                "issue-b",
                TriggerId::Delegate(DelegateId::new("delegate-1")),
            ),
            task_key("issue-c", TriggerId::Label(LabelName::new("kuroko"))),
        ],
        limits: Limits {
            in_flight: NonZeroUsize::new(1).unwrap(),
            max_queued: NonZeroUsize::new(3).unwrap(),
        },
    }
}

impl Kuroko {
    fn event(&self, action: Action) -> Event {
        match action {
            Action::Admit(index) => Event::Admit(TaskRequest {
                key: self.keys[index].clone(),
                spec: TaskSpec {
                    identifier: IDENTIFIERS[index].to_owned(),
                    title: format!("issue {index}"),
                    description: None,
                    url: None,
                },
                prompt: format!("prompt {index}"),
            }),
            Action::Withdraw(index) => Event::Withdraw(self.keys[index].clone()),
            Action::Finish(index, ok) => Event::Finished {
                key: self.keys[index].clone(),
                done: Done {
                    at: Millis(1_000),
                    outcome: if ok {
                        Outcome::Success
                    } else {
                        Outcome::Failure(FailureReason::Timeout)
                    },
                    output: String::new(),
                    log: String::new(),
                },
            },
        }
    }
}

impl Model for Kuroko {
    type State = SysState;
    type Action = Action;

    fn init_states(&self) -> Vec<Self::State> {
        vec![SysState {
            book: Book::new(self.limits),
            tickets: BTreeMap::new(),
        }]
    }

    fn actions(&self, _state: &Self::State, actions: &mut Vec<Self::Action>) {
        for index in 0..KEY_COUNT {
            actions.push(Action::Admit(index));
            actions.push(Action::Withdraw(index));
            actions.push(Action::Finish(index, true));
            actions.push(Action::Finish(index, false));
        }
    }

    fn next_state(&self, state: &Self::State, action: Self::Action) -> Option<Self::State> {
        let mut next = state.clone();
        let step = apply(&mut next.book, self.event(action));
        for cmd in step.cmds {
            let Cmd::Dispatch(dispatch) = cmd;
            *next.tickets.entry(dispatch.key).or_insert(0) += 1;
        }
        // Two `withdraw` mutants let a finished task be removed and re-admitted forever,
        // so the ticket map grows without bound and BFS never terminates. A ticket of 2
        // is still reachable, so an invariant violation still fails fast under this bound.
        if next.tickets.values().any(|&tickets| tickets > 4) {
            return None;
        }
        Some(next)
    }

    fn properties(&self) -> Vec<Property<Self>> {
        vec![
            Property::always("one dispatch per key", |_model: &Self, state: &SysState| {
                state.tickets.values().all(|&count| count <= 1)
            }),
            Property::always(
                "one dispatched task per issue",
                |_model: &Self, state: &SysState| {
                    let mut busy: BTreeSet<&IssueId> = BTreeSet::new();
                    state.book.tasks().all(|task| {
                        !matches!(task.state, TaskState::Dispatched) || busy.insert(&task.key.issue)
                    })
                },
            ),
            Property::always(
                "in flight within the limit",
                |model: &Self, state: &SysState| {
                    state.book.in_flight() <= model.limits.in_flight.get()
                },
            ),
            Property::always(
                "queue is exactly the queued keys",
                |_model: &Self, state: &SysState| {
                    let pending: Vec<&TaskKey> = state.book.pending_keys().collect();
                    let unique: BTreeSet<&TaskKey> = pending.iter().copied().collect();
                    let queued: BTreeSet<&TaskKey> = state
                        .book
                        .tasks()
                        .filter(|task| task.state == TaskState::Queued)
                        .map(|task| &task.key)
                        .collect();
                    let dispatched = state
                        .book
                        .tasks()
                        .filter(|task| matches!(task.state, TaskState::Dispatched))
                        .count();
                    unique.len() == pending.len()
                        && unique == queued
                        && state.book.in_flight() == dispatched
                },
            ),
            Property::always(
                "terminal tasks carry a result",
                |_model: &Self, state: &SysState| {
                    state.book.tasks().all(|task| {
                        !task.state.is_terminal()
                            || matches!(task.state, TaskState::Succeeded(_) | TaskState::Failed(_))
                    })
                },
            ),
            Property::always(
                "in flight matches the dispatched count",
                |_model: &Self, state: &SysState| {
                    state.book.in_flight()
                        == state
                            .book
                            .tasks()
                            .filter(|task| matches!(task.state, TaskState::Dispatched))
                            .count()
                },
            ),
        ]
    }
}

#[test]
fn book_invariants_hold_under_interleavings() {
    let checker = model().checker().threads(4).spawn_bfs().join();
    checker.assert_properties();
    println!("unique states: {}", checker.unique_state_count());
    assert!(checker.unique_state_count() < 200_000);
}
