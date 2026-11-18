#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::num::NonZeroUsize;
use std::path::PathBuf;

use kuroko_core::{
    Book, DelegateId, Done, Event, FailureReason, IssueId, Journal, LabelName, Limits, Millis,
    Outcome, Source, Submission, TaskKey, TaskRequest, TaskSpec, TaskState, TriggerId, apply,
};
use kuroko_store::{Loaded, Store, StoreError, StoredInstallation};
use rusqlite::Connection;
use tempfile::TempDir;

// The version 1 schema as a released binary wrote it, so the migration test
// opens a database this build never created.
const SCHEMA_V1: &str = "
CREATE TABLE tasks (
    source TEXT NOT NULL,
    issue TEXT NOT NULL,
    trigger TEXT NOT NULL,
    identifier TEXT NOT NULL,
    title TEXT NOT NULL,
    description TEXT,
    url TEXT,
    prompt TEXT NOT NULL,
    state TEXT NOT NULL,
    done TEXT,
    PRIMARY KEY (source, issue, trigger)
);

CREATE TABLE pending (
    source TEXT NOT NULL,
    issue TEXT NOT NULL,
    trigger TEXT NOT NULL,
    position INTEGER NOT NULL,
    PRIMARY KEY (source, issue, trigger),
    FOREIGN KEY (source, issue, trigger) REFERENCES tasks (source, issue, trigger) ON DELETE CASCADE
);
";

const fn limits(in_flight: usize, max_queued: usize) -> Limits {
    Limits {
        in_flight: NonZeroUsize::new(in_flight).unwrap(),
        max_queued: NonZeroUsize::new(max_queued).unwrap(),
    }
}

fn label(issue: &str, name: &str) -> TaskKey {
    TaskKey {
        source: Source::Linear,
        issue: IssueId::new(issue),
        trigger: TriggerId::Label(LabelName::new(name)),
    }
}

fn delegate(issue: &str, id: &str) -> TaskKey {
    TaskKey {
        source: Source::Linear,
        issue: IssueId::new(issue),
        trigger: TriggerId::Delegate(DelegateId::new(id)),
    }
}

fn request(key: &TaskKey) -> TaskRequest {
    TaskRequest {
        key: key.clone(),
        spec: TaskSpec {
            identifier: key.issue.as_str().to_uppercase(),
            title: format!("title {}", key.issue.as_str()),
            description: Some(format!("body {}", key.issue.as_str())),
            url: Some(format!(
                "https://linear.app/mok/issue/{}",
                key.issue.as_str()
            )),
        },
        prompt: format!("work on {}", key.issue.as_str()),
    }
}

fn admit(book: &mut Book, key: &TaskKey) {
    let _ = apply(book, Event::Admit(request(key)));
}

fn finish(book: &mut Book, key: &TaskKey, ok: bool) {
    let done = Done {
        at: Millis(11),
        outcome: if ok {
            Outcome::Success
        } else {
            Outcome::Failure(FailureReason::Exit { code: Some(3) })
        },
        output: "output".to_owned(),
        log: "log".to_owned(),
    };
    let _ = apply(
        book,
        Event::Finished {
            key: key.clone(),
            done,
        },
    );
}

fn state<'a>(loaded: &'a Loaded, key: &TaskKey) -> Option<&'a TaskState> {
    loaded
        .tasks
        .iter()
        .find(|task| &task.key == key)
        .map(|task| &task.state)
}

struct Fixture {
    _dir: TempDir,
    path: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tasks.db");
        Self { _dir: dir, path }
    }

    fn store(&self) -> Store {
        Store::open(&self.path).unwrap()
    }

    fn raw(&self) -> Connection {
        Connection::open(&self.path).unwrap()
    }
}

fn installation(app_user_id: &str, access_token: &str, installed_at: i64) -> StoredInstallation {
    StoredInstallation {
        app_user_id: app_user_id.to_owned(),
        access_token: access_token.to_owned(),
        refresh_token: Some(format!("refresh-{access_token}")),
        expires_at: 111,
        scope: "read,write".to_owned(),
        installed_at,
    }
}

#[test]
fn a_saved_book_round_trips_after_reopen() {
    let fixture = Fixture::new();
    let mut book = Book::new(limits(1, 8));
    let running = delegate("issue-a", "agent-1");
    let first = label("issue-b", "kuroko");
    let second = delegate("issue-c", "agent-1");
    let withdrawn = label("issue-d", "kuroko");
    admit(&mut book, &running);
    admit(&mut book, &first);
    admit(&mut book, &second);
    admit(&mut book, &withdrawn);
    let _ = apply(&mut book, Event::Withdraw(withdrawn.clone()));

    let store = fixture.store();
    store.save(&book).unwrap();
    drop(store);

    let loaded = fixture.store().load().unwrap();

    assert_eq!(loaded.tasks.len(), 4);
    assert_eq!(state(&loaded, &running), Some(&TaskState::Dispatched));
    assert_eq!(state(&loaded, &first), Some(&TaskState::Queued));
    assert_eq!(state(&loaded, &second), Some(&TaskState::Queued));
    assert_eq!(state(&loaded, &withdrawn), Some(&TaskState::Withdrawn));
    assert_eq!(loaded.pending, vec![first, second]);
    let task = loaded
        .tasks
        .iter()
        .find(|task| task.key == running)
        .unwrap();
    assert_eq!(task.spec.identifier, "ISSUE-A");
    assert_eq!(task.spec.description.as_deref(), Some("body issue-a"));
    assert_eq!(task.prompt, "work on issue-a");
}

#[test]
fn terminal_results_survive_reopen() {
    let fixture = Fixture::new();
    let mut book = Book::new(limits(1, 8));
    let succeeded = label("issue-a", "kuroko");
    let failed = label("issue-b", "kuroko");
    admit(&mut book, &succeeded);
    finish(&mut book, &succeeded, true);
    admit(&mut book, &failed);
    finish(&mut book, &failed, false);

    let store = fixture.store();
    store.save(&book).unwrap();
    drop(store);

    let loaded = fixture.store().load().unwrap();

    let expected_failure = Done {
        at: Millis(11),
        outcome: Outcome::Failure(FailureReason::Exit { code: Some(3) }),
        output: "output".to_owned(),
        log: "log".to_owned(),
    };
    let expected_success = Done {
        at: Millis(11),
        outcome: Outcome::Success,
        output: "output".to_owned(),
        log: "log".to_owned(),
    };
    assert_eq!(
        state(&loaded, &succeeded),
        Some(&TaskState::Succeeded(expected_success))
    );
    assert_eq!(
        state(&loaded, &failed),
        Some(&TaskState::Failed(expected_failure))
    );
}

#[test]
fn a_loaded_dispatch_reconciles_to_recovery_pending_in_order() {
    let fixture = Fixture::new();
    let mut book = Book::new(limits(1, 8));
    let running = label("issue-a", "kuroko");
    let first = label("issue-b", "kuroko");
    let second = label("issue-c", "kuroko");
    admit(&mut book, &running);
    admit(&mut book, &first);
    admit(&mut book, &second);
    let store = fixture.store();
    store.save(&book).unwrap();
    drop(store);

    let loaded = fixture.store().load().unwrap();
    assert_eq!(state(&loaded, &running), Some(&TaskState::Dispatched));

    let mut restored = Book::reconcile(limits(1, 8), loaded.tasks, loaded.pending);

    assert_eq!(
        restored.task(&running).map(|task| &task.state),
        Some(&TaskState::RecoveryPending)
    );
    assert_eq!(restored.in_flight(), 0);
    assert_eq!(
        restored.pending_keys().cloned().collect::<Vec<_>>(),
        vec![first.clone(), second.clone()]
    );
    assert_eq!(
        apply(&mut restored, Event::Admit(request(&running))).submission,
        Some(Submission::Duplicate)
    );
}

#[test]
fn a_redelivery_after_restart_is_a_duplicate() {
    let fixture = Fixture::new();
    let mut book = Book::new(limits(1, 8));
    let key = label("issue-a", "kuroko");
    admit(&mut book, &key);
    finish(&mut book, &key, true);
    let store = fixture.store();
    store.save(&book).unwrap();
    drop(store);

    let loaded = fixture.store().load().unwrap();
    let mut restored = Book::reconcile(limits(1, 8), loaded.tasks, loaded.pending);

    let step = apply(&mut restored, Event::Admit(request(&key)));

    assert_eq!(step.submission, Some(Submission::Duplicate));
    assert_eq!(step.cmds, Vec::new());
}

#[test]
fn a_withdrawn_tombstone_survives_restart() {
    let fixture = Fixture::new();
    let mut book = Book::new(limits(1, 1));
    admit(&mut book, &label("issue-running", "kuroko"));
    let key = label("issue-a", "kuroko");
    admit(&mut book, &key);
    let _ = apply(&mut book, Event::Withdraw(key.clone()));
    let store = fixture.store();
    store.save(&book).unwrap();
    drop(store);

    let loaded = fixture.store().load().unwrap();
    assert_eq!(state(&loaded, &key), Some(&TaskState::Withdrawn));
    assert_eq!(loaded.pending, Vec::new());

    let mut restored = Book::reconcile(limits(1, 1), loaded.tasks, loaded.pending);
    assert_eq!(
        restored.task(&key).map(|task| &task.state),
        Some(&TaskState::Withdrawn)
    );
    assert_eq!(
        apply(&mut restored, Event::Admit(request(&key))).submission,
        Some(Submission::Duplicate)
    );
}

#[test]
fn a_recovery_pending_row_round_trips() {
    let fixture = Fixture::new();
    let mut book = Book::new(limits(1, 1));
    let key = label("issue-a", "kuroko");
    admit(&mut book, &key);
    let store = fixture.store();
    store.save(&book).unwrap();

    let loaded = store.load().unwrap();
    let recovered = Book::reconcile(limits(1, 1), loaded.tasks, loaded.pending);
    store.save(&recovered).unwrap();
    drop(store);

    let loaded = fixture.store().load().unwrap();
    assert_eq!(state(&loaded, &key), Some(&TaskState::RecoveryPending));
}

#[test]
fn a_newer_schema_is_refused() {
    let fixture = Fixture::new();
    let raw = Connection::open(&fixture.path).unwrap();
    raw.execute_batch(
        "CREATE TABLE schema_version (version INTEGER NOT NULL);
         INSERT INTO schema_version (version) VALUES (999);",
    )
    .unwrap();
    drop(raw);

    let error = Store::open(&fixture.path).unwrap_err();

    assert!(matches!(
        error,
        StoreError::NewerSchema {
            found: 999,
            supported: 2
        }
    ));
}

#[test]
fn a_failed_write_rolls_back() {
    let fixture = Fixture::new();
    let mut book = Book::new(limits(1, 1));
    let running = label("issue-a", "kuroko");
    let queued = label("issue-b", "kuroko");
    admit(&mut book, &running);
    admit(&mut book, &queued);
    let store = fixture.store();
    store.save(&book).unwrap();

    let raw = fixture.raw();
    raw.execute_batch(
        "CREATE TRIGGER fail_insert BEFORE INSERT ON tasks
         BEGIN SELECT RAISE(ABORT, 'disk full'); END;",
    )
    .unwrap();
    drop(raw);

    let mut rejected = Book::new(limits(1, 1));
    admit(&mut rejected, &label("issue-z", "kuroko"));
    assert!(matches!(store.save(&rejected), Err(StoreError::Sqlite(_))));

    let loaded = store.load().unwrap();
    assert_eq!(loaded.tasks.len(), 2);
    assert_eq!(state(&loaded, &running), Some(&TaskState::Dispatched));
    assert_eq!(state(&loaded, &queued), Some(&TaskState::Queued));
    assert!(state(&loaded, &label("issue-z", "kuroko")).is_none());
}

#[test]
fn an_uncommitted_write_is_rolled_back() {
    let fixture = Fixture::new();
    let mut book = Book::new(limits(1, 8));
    let running = label("issue-a", "kuroko");
    let queued = label("issue-b", "kuroko");
    admit(&mut book, &running);
    admit(&mut book, &queued);
    let store = fixture.store();
    store.save(&book).unwrap();
    drop(store);

    let raw = fixture.raw();
    raw.execute_batch("BEGIN IMMEDIATE; DELETE FROM pending; DELETE FROM tasks;")
        .unwrap();
    drop(raw);

    let loaded = fixture.store().load().unwrap();
    assert_eq!(loaded.tasks.len(), 2);
    assert_eq!(loaded.pending, vec![queued]);
}

#[test]
fn a_committed_write_is_visible_to_a_second_connection() {
    let fixture = Fixture::new();
    let mut book = Book::new(limits(1, 1));
    admit(&mut book, &label("issue-running", "kuroko"));
    let queued = label("issue-a", "kuroko");
    admit(&mut book, &queued);
    let writer = fixture.store();
    writer.save(&book).unwrap();

    let reader = fixture.store();
    let loaded = reader.load().unwrap();

    assert_eq!(loaded.tasks.len(), 2);
    assert_eq!(state(&loaded, &queued), Some(&TaskState::Queued));
}

#[test]
fn a_file_database_runs_in_wal_mode() {
    let fixture = Fixture::new();
    let _store = fixture.store();
    let raw = fixture.raw();
    let mode: String = raw
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
}

#[test]
fn corrupt_rows_are_reported() {
    let fixture = Fixture::new();
    let mut original = Book::new(limits(1, 1));
    admit(&mut original, &label("issue-a", "kuroko"));
    let store = fixture.store();
    store.save(&original).unwrap();

    let cases = [
        ("UPDATE tasks SET state = 'bogus'", "unknown task state"),
        ("UPDATE tasks SET source = 'bogus'", "unknown source"),
        ("UPDATE tasks SET trigger = 'bogus'", "unknown trigger"),
        ("UPDATE tasks SET trigger = 'delegate:'", "empty trigger"),
        (
            "UPDATE tasks SET state = 'succeeded', done = NULL",
            "has no result",
        ),
        (
            "UPDATE tasks SET state = 'succeeded', done = 'not json'",
            "cannot decode result",
        ),
    ];
    for (sql, needle) in cases {
        let raw = fixture.raw();
        raw.execute_batch(sql).unwrap();
        drop(raw);
        let error = store.load().unwrap_err().to_string();
        assert!(error.contains(needle), "`{sql}` produced `{error}`");
        assert!(
            error.contains("issue-a"),
            "`{sql}` did not name the row: `{error}`"
        );
        store.save(&original).unwrap();
    }
}

#[test]
fn the_journal_port_writes_through() {
    let store = Store::open_in_memory().unwrap();
    let mut book = Book::new(limits(1, 1));
    let running = label("issue-a", "kuroko");
    let queued = label("issue-b", "kuroko");
    admit(&mut book, &running);
    admit(&mut book, &queued);

    Journal::persist(&store, &book).unwrap();

    let loaded = store.load().unwrap();
    assert_eq!(loaded.tasks.len(), 2);
    assert_eq!(loaded.pending, vec![queued]);
}

#[test]
fn an_in_memory_store_round_trips() {
    let store = Store::open_in_memory().unwrap();
    let mut book = Book::new(limits(1, 1));
    admit(&mut book, &label("issue-running", "kuroko"));
    let queued = label("issue-a", "kuroko");
    admit(&mut book, &queued);
    store.save(&book).unwrap();

    let loaded = store.load().unwrap();

    assert_eq!(loaded.tasks.len(), 2);
    assert_eq!(loaded.pending, vec![queued]);
}

#[test]
fn an_installation_round_trips_after_reopen() {
    let fixture = Fixture::new();
    let saved = installation("user-1", "access-1", 100);
    let store = fixture.store();
    store.save_installation(&saved).unwrap();
    drop(store);

    let loaded = fixture.store().load_installation("user-1").unwrap();

    assert_eq!(loaded, Some(saved));
}

#[test]
fn an_installation_without_a_refresh_token_round_trips_as_none() {
    let store = Store::open_in_memory().unwrap();
    let saved = StoredInstallation {
        refresh_token: None,
        ..installation("user-1", "access-1", 100)
    };
    store.save_installation(&saved).unwrap();

    let loaded = store.load_installation("user-1").unwrap();

    assert_eq!(loaded, Some(saved));
}

#[test]
fn an_unknown_installation_is_none() {
    let store = Store::open_in_memory().unwrap();

    assert_eq!(store.load_installation("nobody").unwrap(), None);
}

#[test]
fn saving_an_installation_again_replaces_the_row() {
    let fixture = Fixture::new();
    let store = fixture.store();
    store
        .save_installation(&installation("user-1", "access-1", 100))
        .unwrap();
    let second = StoredInstallation {
        access_token: "access-2".to_owned(),
        refresh_token: Some("refresh-2".to_owned()),
        expires_at: 222,
        scope: "read".to_owned(),
        installed_at: 200,
        ..installation("user-1", "access-1", 100)
    };
    store.save_installation(&second).unwrap();

    let loaded = store.load_installation("user-1").unwrap();
    let raw = fixture.raw();
    let rows: i64 = raw
        .query_row("SELECT COUNT(*) FROM linear_installation", [], |row| {
            row.get(0)
        })
        .unwrap();

    assert_eq!(loaded, Some(second));
    assert_eq!(rows, 1);
}

#[test]
fn the_default_installation_is_the_newest() {
    let store = Store::open_in_memory().unwrap();
    let older = installation("user-1", "access-1", 100);
    let newer = installation("user-2", "access-2", 200);
    store.save_installation(&newer).unwrap();
    store.save_installation(&older).unwrap();

    assert_eq!(store.default_installation().unwrap(), Some(newer));
}

#[test]
fn no_installation_has_a_default() {
    let store = Store::open_in_memory().unwrap();

    assert_eq!(store.default_installation().unwrap(), None);
}

#[test]
fn a_version_1_database_migrates_with_its_tasks_intact() {
    let fixture = Fixture::new();
    let key = label("issue-a", "kuroko");

    let raw = fixture.raw();
    raw.execute_batch(SCHEMA_V1).unwrap();
    raw.execute_batch(
        "CREATE TABLE schema_version (version INTEGER NOT NULL);
         INSERT INTO schema_version (version) VALUES (1);
         INSERT INTO tasks (source, issue, trigger, identifier, title, description, url, prompt, state, done)
         VALUES ('linear', 'issue-a', 'label:kuroko', 'ISSUE-A', 'title issue-a', NULL, NULL, 'work on issue-a', 'queued', NULL);
         INSERT INTO pending (source, issue, trigger, position)
         VALUES ('linear', 'issue-a', 'label:kuroko', 0);",
    )
    .unwrap();
    drop(raw);

    let store = fixture.store();

    let loaded = store.load().unwrap();
    let version: i64 = fixture
        .raw()
        .query_row("SELECT version FROM schema_version", [], |row| row.get(0))
        .unwrap();
    store
        .save_installation(&installation("user-1", "access-1", 100))
        .unwrap();
    assert_eq!(version, 2);
    assert_eq!(state(&loaded, &key), Some(&TaskState::Queued));
    assert_eq!(loaded.pending, vec![key]);
}

#[test]
fn a_version_3_database_is_refused() {
    let fixture = Fixture::new();
    let raw = fixture.raw();
    raw.execute_batch(
        "CREATE TABLE schema_version (version INTEGER NOT NULL);
         INSERT INTO schema_version (version) VALUES (3);",
    )
    .unwrap();
    drop(raw);

    let error = Store::open(&fixture.path).unwrap_err();

    assert!(matches!(
        error,
        StoreError::NewerSchema {
            found: 3,
            supported: 2
        }
    ));
}

#[test]
fn the_installation_debug_redacts_both_tokens() {
    let saved = installation("user-1", "secret-access", 100);

    let printed = format!("{saved:?}");

    assert!(
        printed.contains("access_token: \"[redacted]\""),
        "`{printed}`"
    );
    assert!(
        printed.contains("refresh_token: Some(\"[redacted]\")"),
        "`{printed}`"
    );
    assert!(!printed.contains("secret-access"), "`{printed}`");
    assert!(!printed.contains("refresh-secret-access"), "`{printed}`");
    assert!(printed.contains("user-1"), "`{printed}`");
}

#[test]
fn an_absent_refresh_token_prints_as_none() {
    let saved = StoredInstallation {
        refresh_token: None,
        ..installation("user-1", "secret-access", 100)
    };

    let printed = format!("{saved:?}");

    assert!(printed.contains("refresh_token: None"), "`{printed}`");
}
