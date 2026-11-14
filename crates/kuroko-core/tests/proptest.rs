#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroUsize;

use kuroko_core::{
    Book, Cmd, DelegateId, Done, Event, FailureReason, IssueId, LabelName, Limits, Millis, Outcome,
    Source, Submission, TaskKey, TaskRequest, TaskSpec, TaskState, TriggerId, apply,
};
use proptest::prelude::*;

const KEY_COUNT: usize = 3;
const IDENTIFIERS: [&str; KEY_COUNT] = ["MOK-1", "MOK-1", "MOK-2"];

fn keys() -> [TaskKey; KEY_COUNT] {
    [
        task_key(
            "issue-a",
            TriggerId::Delegate(DelegateId::new("delegate-1")),
        ),
        task_key("issue-a", TriggerId::Label(LabelName::new("kuroko"))),
        task_key(
            "issue-b",
            TriggerId::Delegate(DelegateId::new("delegate-1")),
        ),
    ]
}

fn task_key(issue: &str, trigger: TriggerId) -> TaskKey {
    TaskKey {
        source: Source::Linear,
        issue: IssueId::new(issue),
        trigger,
    }
}

#[derive(Clone, Copy, Debug)]
enum Op {
    Admit(usize),
    Withdraw(usize),
    Finish(usize, bool),
    Settle(usize, bool),
    Resume,
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        (0..KEY_COUNT).prop_map(Op::Admit),
        (0..KEY_COUNT).prop_map(Op::Withdraw),
        (0..KEY_COUNT, any::<bool>()).prop_map(|(index, ok)| Op::Finish(index, ok)),
        (0..KEY_COUNT, any::<bool>()).prop_map(|(index, ok)| Op::Settle(index, ok)),
        Just(Op::Resume),
    ]
}

fn limits() -> impl Strategy<Value = Limits> {
    (1usize..=3, 1usize..=3).prop_map(|(in_flight, max_queued)| Limits {
        in_flight: NonZeroUsize::new(in_flight).unwrap(),
        max_queued: NonZeroUsize::new(max_queued).unwrap(),
    })
}

fn event(op: Op, keys: &[TaskKey; KEY_COUNT]) -> Event {
    match op {
        Op::Admit(index) => Event::Admit(TaskRequest {
            key: keys[index].clone(),
            spec: TaskSpec {
                identifier: IDENTIFIERS[index].to_owned(),
                title: format!("issue {index}"),
                description: None,
                url: None,
            },
            prompt: format!("work on {}", IDENTIFIERS[index]),
        }),
        Op::Withdraw(index) => Event::Withdraw(keys[index].clone()),
        Op::Finish(index, ok) => Event::Finished {
            key: keys[index].clone(),
            done: done(ok),
        },
        Op::Settle(index, ok) => Event::Settled {
            key: keys[index].clone(),
            done: done(ok),
        },
        Op::Resume => Event::Resume,
    }
}

const fn done(ok: bool) -> Done {
    Done {
        at: Millis(50),
        outcome: if ok {
            Outcome::Success
        } else {
            Outcome::Failure(FailureReason::Timeout)
        },
        output: String::new(),
        log: String::new(),
    }
}

const fn expected_submission(present: bool, pending_len: usize, limits: Limits) -> Submission {
    if present {
        Submission::Duplicate
    } else if pending_len >= limits.max_queued.get() {
        Submission::Full
    } else {
        Submission::Accepted
    }
}

fn assert_invariants(book: &Book, limits: Limits, arrival: &BTreeMap<TaskKey, u64>) {
    let pending: Vec<TaskKey> = book.pending_keys().cloned().collect();
    let pending_set: BTreeSet<TaskKey> = pending.iter().cloned().collect();
    assert_eq!(pending_set.len(), pending.len());
    for key in &pending {
        assert_eq!(
            book.task(key).map(|task| &task.state),
            Some(&TaskState::Queued)
        );
    }
    assert!(
        pending
            .windows(2)
            .all(|pair| arrival[&pair[0]] < arrival[&pair[1]])
    );
    let queued: BTreeSet<TaskKey> = book
        .tasks()
        .filter(|task| task.state == TaskState::Queued)
        .map(|task| task.key.clone())
        .collect();
    assert_eq!(queued, pending_set);
    assert!(pending.len() <= limits.max_queued.get());

    let dispatched = book
        .tasks()
        .filter(|task| matches!(task.state, TaskState::Dispatched))
        .count();
    assert_eq!(book.in_flight(), dispatched);
    assert!(book.in_flight() <= limits.in_flight.get());
    let mut busy: BTreeSet<&IssueId> = BTreeSet::new();
    for task in book.tasks() {
        if matches!(task.state, TaskState::Dispatched) {
            assert!(busy.insert(&task.key.issue));
        }
        if task.state.is_terminal() {
            assert!(matches!(
                task.state,
                TaskState::Succeeded(_) | TaskState::Failed(_) | TaskState::Withdrawn
            ));
        }
    }
    assert!(
        !pending_set
            .iter()
            .any(|key| book.task(key).is_some_and(|task| task.state.is_terminal()))
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn invariants_hold_after_every_event(
        limits in limits(),
        ops in prop::collection::vec(op(), 0..=24),
    ) {
        let keys = keys();
        let identifiers: BTreeMap<TaskKey, &str> =
            keys.iter().cloned().zip(IDENTIFIERS).collect();
        let mut book = Book::new(limits);
        let mut arrival: BTreeMap<TaskKey, u64> = BTreeMap::new();
        let mut dispatched: BTreeSet<TaskKey> = BTreeSet::new();
        let mut admissions: u64 = 0;

        for op in ops {
            let index = match op {
                Op::Admit(index)
                | Op::Withdraw(index)
                | Op::Finish(index, _)
                | Op::Settle(index, _) => Some(index),
                Op::Resume => None,
            };
            let present = index.is_some_and(|index| book.task(&keys[index]).is_some());
            let pending_len = book.pending_keys().count();
            let step = apply(&mut book, event(op, &keys));

            if matches!(op, Op::Admit(_)) {
                prop_assert_eq!(
                    step.submission,
                    Some(expected_submission(present, pending_len, limits))
                );
                if step.submission == Some(Submission::Accepted) {
                    arrival.insert(keys[index.unwrap()].clone(), admissions);
                    admissions += 1;
                }
            } else {
                prop_assert_eq!(step.submission, None);
            }

            for cmd in &step.cmds {
                let Cmd::Dispatch(dispatch) = cmd;
                prop_assert_eq!(dispatch.issue.as_str(), identifiers[&dispatch.key]);
                prop_assert_eq!(
                    &dispatch.prompt,
                    &format!("work on {}", identifiers[&dispatch.key])
                );
                prop_assert_eq!(
                    book.task(&dispatch.key).map(|task| &task.state),
                    Some(&TaskState::Dispatched)
                );
                prop_assert!(dispatched.insert(dispatch.key.clone()));
            }

            assert_invariants(&book, limits, &arrival);
        }
    }
}
