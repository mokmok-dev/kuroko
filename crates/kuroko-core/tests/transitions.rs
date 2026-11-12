#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::num::NonZeroUsize;

use kuroko_core::{
    Book, Cmd, DelegateId, Done, Event, FailureReason, IssueId, LabelName, Limits, Millis, Outcome,
    Source, Step, Submission, TaskKey, TaskRequest, TaskSpec, TaskState, TriggerId, apply,
};

const fn limits(in_flight: usize, max_queued: usize) -> Limits {
    Limits {
        in_flight: NonZeroUsize::new(in_flight).unwrap(),
        max_queued: NonZeroUsize::new(max_queued).unwrap(),
    }
}

fn key(issue: &str, trigger: TriggerId) -> TaskKey {
    TaskKey {
        source: Source::Linear,
        issue: IssueId::new(issue),
        trigger,
    }
}

fn delegate(issue: &str, id: &str) -> TaskKey {
    key(issue, TriggerId::Delegate(DelegateId::new(id)))
}

fn label(issue: &str, name: &str) -> TaskKey {
    key(issue, TriggerId::Label(LabelName::new(name)))
}

fn request(key: &TaskKey, identifier: &str) -> TaskRequest {
    TaskRequest {
        key: key.clone(),
        spec: TaskSpec {
            identifier: identifier.to_owned(),
            title: format!("{identifier} title"),
            description: None,
            url: None,
        },
        prompt: format!("work on {identifier}"),
    }
}

fn admit(book: &mut Book, key: &TaskKey, identifier: &str) -> Step {
    apply(book, Event::Admit(request(key, identifier)))
}

fn done(outcome: Outcome) -> Done {
    Done {
        at: Millis(11),
        outcome,
        output: "output".to_owned(),
        log: "log".to_owned(),
    }
}

fn success() -> Done {
    done(Outcome::Success)
}

fn failure() -> Done {
    done(Outcome::Failure(FailureReason::Exit { code: Some(3) }))
}

fn finish(book: &mut Book, key: &TaskKey, done: Done) -> Step {
    apply(
        book,
        Event::Finished {
            key: key.clone(),
            done,
        },
    )
}

fn dispatched(step: &Step) -> Vec<&TaskKey> {
    step.cmds
        .iter()
        .map(|Cmd::Dispatch(dispatch)| &dispatch.key)
        .collect()
}

fn pending(book: &Book) -> Vec<&TaskKey> {
    book.pending_keys().collect()
}

#[test]
fn admit_on_absent_key_queues_then_dispatches() {
    let mut book = Book::new(limits(1, 3));
    let key = delegate("issue-a", "delegate-1");
    let step = admit(&mut book, &key, "MOK-1");

    assert_eq!(step.submission, Some(Submission::Accepted));
    assert_eq!(step.cmds.len(), 1);
    let Cmd::Dispatch(dispatch) = &step.cmds[0];
    assert_eq!(dispatch.key, key);
    assert_eq!(dispatch.issue, "MOK-1");
    assert_eq!(dispatch.prompt, "work on MOK-1");
    assert_eq!(book.in_flight(), 1);
    assert_eq!(pending(&book), Vec::<&TaskKey>::new());
    assert_eq!(
        book.task(&key).map(|task| &task.state),
        Some(&TaskState::Dispatched)
    );
}

#[test]
fn admit_without_free_slot_leaves_task_queued() {
    let mut book = Book::new(limits(1, 3));
    let running = delegate("issue-a", "delegate-1");
    admit(&mut book, &running, "MOK-1");
    let queued = delegate("issue-b", "delegate-1");
    let step = admit(&mut book, &queued, "MOK-2");

    assert_eq!(step.submission, Some(Submission::Accepted));
    assert_eq!(step.cmds, Vec::new());
    assert_eq!(book.in_flight(), 1);
    assert_eq!(pending(&book), vec![&queued]);
    assert_eq!(
        book.task(&queued).map(|task| &task.state),
        Some(&TaskState::Queued)
    );
}

#[test]
fn admit_duplicate_while_queued_is_duplicate() {
    let mut book = Book::new(limits(1, 3));
    let running = delegate("issue-a", "delegate-1");
    admit(&mut book, &running, "MOK-1");
    let queued = delegate("issue-b", "delegate-1");
    admit(&mut book, &queued, "MOK-2");

    let step = admit(&mut book, &queued, "MOK-9");

    assert_eq!(step.submission, Some(Submission::Duplicate));
    assert_eq!(step.cmds, Vec::new());
    assert_eq!(book.tasks().count(), 2);
    assert_eq!(pending(&book), vec![&queued]);
    assert_eq!(
        book.task(&queued).map(|task| task.spec.identifier.as_str()),
        Some("MOK-2")
    );
}

#[test]
fn admit_duplicate_while_in_flight_is_duplicate() {
    let mut book = Book::new(limits(1, 3));
    let key = delegate("issue-a", "delegate-1");
    admit(&mut book, &key, "MOK-1");

    let step = admit(&mut book, &key, "MOK-1");

    assert_eq!(step.submission, Some(Submission::Duplicate));
    assert_eq!(step.cmds, Vec::new());
    assert_eq!(book.in_flight(), 1);
    assert_eq!(
        book.task(&key).map(|task| &task.state),
        Some(&TaskState::Dispatched)
    );
}

#[test]
fn admit_duplicate_after_terminal_is_duplicate() {
    let mut book = Book::new(limits(1, 3));
    let key = delegate("issue-a", "delegate-1");
    admit(&mut book, &key, "MOK-1");
    finish(&mut book, &key, success());

    let step = admit(&mut book, &key, "MOK-1");

    assert_eq!(step.submission, Some(Submission::Duplicate));
    assert_eq!(step.cmds, Vec::new());
    assert_eq!(book.tasks().count(), 1);
    assert_eq!(
        book.task(&key).map(|task| &task.state),
        Some(&TaskState::Succeeded(success()))
    );
}

#[test]
fn admit_at_max_queued_is_full_without_inserting() {
    let mut book = Book::new(limits(1, 1));
    admit(&mut book, &delegate("issue-a", "delegate-1"), "MOK-1");
    let queued = delegate("issue-b", "delegate-1");
    admit(&mut book, &queued, "MOK-2");
    let rejected = delegate("issue-c", "delegate-1");

    let step = admit(&mut book, &rejected, "MOK-3");

    assert_eq!(step.submission, Some(Submission::Full));
    assert_eq!(step.cmds, Vec::new());
    assert!(book.task(&rejected).is_none());
    assert_eq!(pending(&book), vec![&queued]);
    assert_eq!(book.tasks().count(), 2);
    assert_eq!(book.in_flight(), 1);
}

#[test]
fn admit_below_max_queued_is_accepted() {
    let mut book = Book::new(limits(1, 3));
    admit(&mut book, &delegate("issue-a", "delegate-1"), "MOK-1");
    admit(&mut book, &delegate("issue-b", "delegate-1"), "MOK-2");
    let second = delegate("issue-c", "delegate-1");

    let step = admit(&mut book, &second, "MOK-3");

    assert_eq!(step.submission, Some(Submission::Accepted));
    assert_eq!(
        pending(&book),
        vec![&delegate("issue-b", "delegate-1"), &second]
    );
}

#[test]
fn admit_duplicate_at_max_queued_is_duplicate() {
    let mut book = Book::new(limits(1, 1));
    admit(&mut book, &delegate("issue-a", "delegate-1"), "MOK-1");
    let queued = delegate("issue-b", "delegate-1");
    admit(&mut book, &queued, "MOK-2");

    let step = admit(&mut book, &queued, "MOK-2");

    assert_eq!(step.submission, Some(Submission::Duplicate));
    assert_eq!(book.tasks().count(), 2);
}

#[test]
fn withdraw_queued_removes_task_and_pending_entry() {
    let mut book = Book::new(limits(1, 3));
    admit(&mut book, &delegate("issue-a", "delegate-1"), "MOK-1");
    let first = delegate("issue-b", "delegate-1");
    let second = delegate("issue-c", "delegate-1");
    admit(&mut book, &first, "MOK-2");
    admit(&mut book, &second, "MOK-3");

    let step = apply(&mut book, Event::Withdraw(first.clone()));

    assert_eq!(step.submission, None);
    assert_eq!(step.cmds, Vec::new());
    assert!(book.task(&first).is_none());
    assert_eq!(pending(&book), vec![&second]);
    assert_eq!(book.in_flight(), 1);
}

#[test]
fn withdraw_in_flight_is_noop() {
    let mut book = Book::new(limits(1, 3));
    let key = delegate("issue-a", "delegate-1");
    admit(&mut book, &key, "MOK-1");

    let step = apply(&mut book, Event::Withdraw(key.clone()));

    assert_eq!(step.submission, None);
    assert_eq!(step.cmds, Vec::new());
    assert_eq!(
        book.task(&key).map(|task| &task.state),
        Some(&TaskState::Dispatched)
    );
    assert_eq!(book.in_flight(), 1);
}

#[test]
fn withdraw_absent_is_noop() {
    let mut book = Book::new(limits(1, 3));
    let key = delegate("issue-a", "delegate-1");

    let step = apply(&mut book, Event::Withdraw(key.clone()));

    assert_eq!(step.submission, None);
    assert_eq!(step.cmds, Vec::new());
    assert!(book.task(&key).is_none());
    assert_eq!(book.tasks().count(), 0);
    assert_eq!(pending(&book), Vec::<&TaskKey>::new());
}

#[test]
fn withdraw_terminal_is_noop() {
    let mut book = Book::new(limits(1, 3));
    let key = delegate("issue-a", "delegate-1");
    admit(&mut book, &key, "MOK-1");
    finish(&mut book, &key, success());

    let step = apply(&mut book, Event::Withdraw(key.clone()));

    assert_eq!(step.cmds, Vec::new());
    assert_eq!(
        book.task(&key).map(|task| &task.state),
        Some(&TaskState::Succeeded(success()))
    );
}

#[test]
fn finish_in_flight_success_records_done() {
    let mut book = Book::new(limits(1, 3));
    let key = delegate("issue-a", "delegate-1");
    admit(&mut book, &key, "MOK-1");

    let step = finish(&mut book, &key, success());

    assert_eq!(step.submission, None);
    assert_eq!(step.cmds, Vec::new());
    assert_eq!(book.in_flight(), 0);
    assert_eq!(
        book.task(&key).map(|task| &task.state),
        Some(&TaskState::Succeeded(success()))
    );
}

#[test]
fn finish_in_flight_failure_records_done() {
    let mut book = Book::new(limits(1, 3));
    let key = delegate("issue-a", "delegate-1");
    admit(&mut book, &key, "MOK-1");

    let step = finish(&mut book, &key, failure());

    assert_eq!(step.submission, None);
    assert_eq!(step.cmds, Vec::new());
    assert_eq!(book.in_flight(), 0);
    assert_eq!(
        book.task(&key).map(|task| &task.state),
        Some(&TaskState::Failed(failure()))
    );
}

#[test]
fn finish_queued_is_noop() {
    let mut book = Book::new(limits(1, 3));
    admit(&mut book, &delegate("issue-a", "delegate-1"), "MOK-1");
    let queued = delegate("issue-b", "delegate-1");
    admit(&mut book, &queued, "MOK-2");

    let step = finish(&mut book, &queued, success());

    assert_eq!(step.cmds, Vec::new());
    assert_eq!(
        book.task(&queued).map(|task| &task.state),
        Some(&TaskState::Queued)
    );
    assert_eq!(pending(&book), vec![&queued]);
    assert_eq!(book.in_flight(), 1);
}

#[test]
fn finish_absent_is_noop() {
    let mut book = Book::new(limits(1, 3));
    let key = delegate("issue-a", "delegate-1");

    let step = finish(&mut book, &key, success());

    assert_eq!(step.cmds, Vec::new());
    assert!(book.task(&key).is_none());
    assert_eq!(book.in_flight(), 0);
}

#[test]
fn finish_terminal_is_noop() {
    let mut book = Book::new(limits(1, 3));
    let key = delegate("issue-a", "delegate-1");
    admit(&mut book, &key, "MOK-1");
    finish(&mut book, &key, success());

    let step = finish(&mut book, &key, failure());

    assert_eq!(step.cmds, Vec::new());
    assert_eq!(
        book.task(&key).map(|task| &task.state),
        Some(&TaskState::Succeeded(success()))
    );
    assert_eq!(book.in_flight(), 0);
}

#[test]
fn finish_promotes_pending_keys_in_arrival_order_at_in_flight_one() {
    let mut book = Book::new(limits(1, 3));
    let running = delegate("issue-a", "delegate-1");
    let first = delegate("issue-b", "delegate-1");
    let second = delegate("issue-c", "delegate-1");
    let third = delegate("issue-d", "delegate-1");
    admit(&mut book, &running, "MOK-1");
    admit(&mut book, &first, "MOK-2");
    admit(&mut book, &second, "MOK-3");
    admit(&mut book, &third, "MOK-4");

    let step = finish(&mut book, &running, success());

    assert_eq!(dispatched(&step), vec![&first]);
    assert_eq!(pending(&book), vec![&second, &third]);
    assert_eq!(book.in_flight(), 1);
}

#[test]
fn finish_promotes_pending_keys_in_arrival_order_at_in_flight_three() {
    let mut book = Book::new(limits(3, 5));
    let running = delegate("issue-a", "delegate-1");
    let second = delegate("issue-b", "delegate-1");
    let third = delegate("issue-c", "delegate-1");
    let fourth = delegate("issue-d", "delegate-1");
    let fifth = delegate("issue-e", "delegate-1");
    admit(&mut book, &running, "MOK-1");
    admit(&mut book, &second, "MOK-2");
    admit(&mut book, &third, "MOK-3");
    assert_eq!(book.in_flight(), 3);
    admit(&mut book, &fourth, "MOK-4");
    admit(&mut book, &fifth, "MOK-5");
    assert_eq!(pending(&book), vec![&fourth, &fifth]);

    let step = finish(&mut book, &running, success());

    assert_eq!(dispatched(&step), vec![&fourth]);
    assert_eq!(pending(&book), vec![&fifth]);
    assert_eq!(book.in_flight(), 3);
}

#[test]
fn per_issue_guard_blocks_second_trigger_while_first_is_in_flight() {
    let mut book = Book::new(limits(2, 3));
    let delegated = delegate("issue-a", "delegate-1");
    let labelled = label("issue-a", "kuroko");
    let other = delegate("issue-b", "delegate-1");
    admit(&mut book, &delegated, "MOK-1");

    let blocked = admit(&mut book, &labelled, "MOK-1");

    assert_eq!(blocked.submission, Some(Submission::Accepted));
    assert_eq!(blocked.cmds, Vec::new());
    assert_eq!(
        book.task(&labelled).map(|task| &task.state),
        Some(&TaskState::Queued)
    );
    let proceeded = admit(&mut book, &other, "MOK-2");
    assert_eq!(dispatched(&proceeded), vec![&other]);
    assert_eq!(book.in_flight(), 2);
    assert_eq!(pending(&book), vec![&labelled]);

    let released = finish(&mut book, &delegated, success());

    assert_eq!(dispatched(&released), vec![&labelled]);
    assert_eq!(pending(&book), Vec::<&TaskKey>::new());
    assert_eq!(book.in_flight(), 2);
}

#[test]
fn terminal_states_are_succeeded_and_failed_only() {
    assert!(!TaskState::Queued.is_terminal());
    assert!(!TaskState::Dispatched.is_terminal());
    assert!(TaskState::Succeeded(success()).is_terminal());
    assert!(TaskState::Failed(failure()).is_terminal());
}
