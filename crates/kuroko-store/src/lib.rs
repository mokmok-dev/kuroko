//! SQLite persistence for the task book: schema, migrations, and the journal port.

use std::fmt;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use kuroko_core::{
    Book, DelegateId, Done, IssueId, Journal, JournalError, LabelName, Source, Task, TaskKey,
    TaskSpec, TaskState, TriggerId,
};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

const SCHEMA_VERSION: i64 = 1;
// Must stay below the webhook submit timeout, so a busy lock fails the request
// instead of committing after the caller has already received 503.
const BUSY_TIMEOUT: Duration = Duration::from_millis(1_000);

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

/// The rows a restart hands to [`kuroko_core::Book::reconcile`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Loaded {
    pub tasks: Vec<Task>,
    pub pending: Vec<TaskKey>,
}

/// A SQLite-backed journal.
#[derive(Debug)]
pub struct Store {
    connection: Mutex<Connection>,
}

impl Store {
    /// Opens or creates the database at `path`.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the file cannot be opened, a pragma fails, or
    /// the schema is newer than this binary understands.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        let mut connection = Connection::open(path)?;
        prepare(&mut connection)?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    /// Opens a private in-memory database.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when a pragma fails or migration cannot run.
    pub fn open_in_memory() -> Result<Self, StoreError> {
        let mut connection = Connection::open_in_memory()?;
        prepare(&mut connection)?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    /// Loads every task and the pending queue order.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the query fails or a row does not decode.
    pub fn load(&self) -> Result<Loaded, StoreError> {
        let mut connection = self.lock()?;
        // Both reads must come from one snapshot: a writer between them could leave a
        // `Queued` row out of `pending`, and `reconcile` would drop it from the queue
        // for good.
        let transaction = connection.transaction()?;
        let tasks = {
            let mut statement = transaction.prepare(
                "SELECT source, issue, trigger, identifier, title, description, url, prompt, state, done \
                 FROM tasks ORDER BY source, issue, trigger",
            )?;
            let rows = statement.query_map([], |row| {
                Ok(RowData {
                    source: row.get(0)?,
                    issue: row.get(1)?,
                    trigger: row.get(2)?,
                    identifier: row.get(3)?,
                    title: row.get(4)?,
                    description: row.get(5)?,
                    url: row.get(6)?,
                    prompt: row.get(7)?,
                    state: row.get(8)?,
                    done: row.get(9)?,
                })
            })?;
            let mut tasks = Vec::new();
            for row in rows {
                tasks.push(row?.decode()?);
            }
            tasks
        };
        let pending = {
            let mut statement = transaction
                .prepare("SELECT source, issue, trigger FROM pending ORDER BY position")?;
            let rows = statement.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?;
            let mut pending = Vec::new();
            for row in rows {
                let (source, issue, trigger) = row?;
                pending.push(decode_key(&source, &issue, &trigger)?);
            }
            pending
        };
        transaction.commit()?;
        drop(connection);
        Ok(Loaded { tasks, pending })
    }

    /// Replaces the persisted book with `book` in one transaction.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the transaction fails; the previous rows stay
    /// visible because SQLite rolls the whole transaction back.
    pub fn save(&self, book: &Book) -> Result<(), StoreError> {
        let mut connection = self.lock()?;
        // DEFERRED upgrades a read to a writer after the fact, which can surface
        // SQLITE_BUSY without the busy handler retrying the upgrade.
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute("DELETE FROM pending", [])?;
        transaction.execute("DELETE FROM tasks", [])?;

        for task in book.tasks() {
            let (source, issue, trigger) = encode_key(&task.key);
            transaction.execute(
                "INSERT INTO tasks \
                 (source, issue, trigger, identifier, title, description, url, prompt, state, done) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    source,
                    issue,
                    trigger,
                    task.spec.identifier,
                    task.spec.title,
                    task.spec.description,
                    task.spec.url,
                    task.prompt,
                    state_tag(&task.state),
                    done_json(&task.state)?,
                ],
            )?;
        }

        for (position, key) in book.pending_keys().enumerate() {
            let (source, issue, trigger) = encode_key(key);
            let position = i64::try_from(position)
                .map_err(|_| StoreError::Corrupt("pending position overflow".to_owned()))?;
            transaction.execute(
                "INSERT INTO pending (source, issue, trigger, position) VALUES (?1, ?2, ?3, ?4)",
                params![source, issue, trigger, position],
            )?;
        }

        transaction.commit()?;
        drop(connection);
        Ok(())
    }

    fn lock(&self) -> Result<MutexGuard<'_, Connection>, StoreError> {
        self.connection.lock().map_err(|_| StoreError::Poisoned)
    }
}

impl Journal for Store {
    fn persist(&self, book: &Book) -> Result<(), JournalError> {
        self.save(book)
            .map_err(|error| JournalError::new(error.to_string()))
    }
}

/// One row of `tasks` as SQLite returns it.
struct RowData {
    source: String,
    issue: String,
    trigger: String,
    identifier: String,
    title: String,
    description: Option<String>,
    url: Option<String>,
    prompt: String,
    state: String,
    done: Option<String>,
}

impl RowData {
    fn decode(self) -> Result<Task, StoreError> {
        let key = decode_key(&self.source, &self.issue, &self.trigger)?;
        let state =
            decode_state(&self.state, self.done.as_deref()).map_err(|error| match error {
                StoreError::Corrupt(message) => StoreError::Corrupt(format!(
                    "task ({}, {}, {}): {message}",
                    self.source, self.issue, self.trigger
                )),
                other => other,
            })?;
        Ok(Task {
            key,
            spec: TaskSpec {
                identifier: self.identifier,
                title: self.title,
                description: self.description,
                url: self.url,
            },
            prompt: self.prompt,
            state,
        })
    }
}

fn prepare(connection: &mut Connection) -> Result<(), StoreError> {
    connection.busy_timeout(BUSY_TIMEOUT)?;
    connection.pragma_update(None, "journal_mode", "WAL")?;
    let mode: String = connection.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
    if !journal_mode_is_usable(&mode) {
        return Err(StoreError::Refused(format!(
            "journal_mode is `{mode}`, not `wal`; WAL is required for the durability guarantee"
        )));
    }
    connection.pragma_update(None, "synchronous", "FULL")?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    migrate(connection, SCHEMA_VERSION)
}

fn journal_mode_is_usable(mode: &str) -> bool {
    matches!(mode, "wal" | "memory")
}

fn migrate(connection: &mut Connection, target: i64) -> Result<(), StoreError> {
    connection
        .execute_batch("CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL)")?;
    let version: Option<i64> = connection
        .query_row("SELECT version FROM schema_version LIMIT 1", [], |row| {
            row.get(0)
        })
        .optional()?;
    let mut version = version.unwrap_or(0);
    if version > target {
        return Err(StoreError::NewerSchema {
            found: version,
            supported: target,
        });
    }
    if version < 1 {
        apply_migration(connection, 1, SCHEMA_V1)?;
        version = 1;
    }
    if version != target {
        return Err(StoreError::Refused(format!(
            "no migration from schema version {version} to {target}"
        )));
    }
    Ok(())
}

fn apply_migration(connection: &mut Connection, version: i64, sql: &str) -> Result<(), StoreError> {
    let transaction = connection.transaction()?;
    transaction.execute_batch(sql)?;
    transaction.execute("DELETE FROM schema_version", [])?;
    transaction.execute(
        "INSERT INTO schema_version (version) VALUES (?1)",
        [version],
    )?;
    transaction.commit()?;
    Ok(())
}

fn encode_key(key: &TaskKey) -> (String, String, String) {
    let source = match key.source {
        Source::Linear => "linear",
    };
    let trigger = match &key.trigger {
        TriggerId::Delegate(id) => format!("delegate:{}", id.as_str()),
        TriggerId::Label(name) => format!("label:{}", name.as_str()),
    };
    (source.to_owned(), key.issue.as_str().to_owned(), trigger)
}

fn decode_key(source: &str, issue: &str, trigger: &str) -> Result<TaskKey, StoreError> {
    let context = || format!("task ({source}, {issue}, {trigger})");
    let source = match source {
        "linear" => Source::Linear,
        other => {
            return Err(StoreError::Corrupt(format!(
                "{}: unknown source `{other}`",
                context()
            )));
        }
    };
    let (kind, value) = trigger.split_once(':').ok_or_else(|| {
        StoreError::Corrupt(format!("{}: unknown trigger `{trigger}`", context()))
    })?;
    if value.is_empty() {
        return Err(StoreError::Corrupt(format!(
            "{}: empty trigger `{trigger}`",
            context()
        )));
    }
    let trigger = match kind {
        "delegate" => TriggerId::Delegate(DelegateId::new(value)),
        "label" => TriggerId::Label(LabelName::new(value)),
        other => {
            return Err(StoreError::Corrupt(format!(
                "{}: unknown trigger `{other}`",
                context()
            )));
        }
    };
    Ok(TaskKey {
        source,
        issue: IssueId::new(issue),
        trigger,
    })
}

const fn state_tag(state: &TaskState) -> &'static str {
    match state {
        TaskState::Queued => "queued",
        TaskState::Dispatched => "dispatched",
        TaskState::RecoveryPending => "recovery_pending",
        TaskState::Succeeded(_) => "succeeded",
        TaskState::Failed(_) => "failed",
        TaskState::Withdrawn => "withdrawn",
    }
}

fn done_json(state: &TaskState) -> Result<Option<String>, StoreError> {
    let (TaskState::Succeeded(done) | TaskState::Failed(done)) = state else {
        return Ok(None);
    };
    let text = serde_json::to_string(done)
        .map_err(|error| StoreError::Refused(format!("cannot encode a result: {error}")))?;
    Ok(Some(text))
}

fn decode_state(tag: &str, done: Option<&str>) -> Result<TaskState, StoreError> {
    match tag {
        "queued" => Ok(TaskState::Queued),
        "dispatched" => Ok(TaskState::Dispatched),
        "recovery_pending" => Ok(TaskState::RecoveryPending),
        "succeeded" => Ok(TaskState::Succeeded(decode_done(tag, done)?)),
        "failed" => Ok(TaskState::Failed(decode_done(tag, done)?)),
        "withdrawn" => Ok(TaskState::Withdrawn),
        other => Err(StoreError::Corrupt(format!("unknown task state `{other}`"))),
    }
}

fn decode_done(tag: &str, done: Option<&str>) -> Result<Done, StoreError> {
    let text = done.ok_or_else(|| StoreError::Corrupt(format!("state `{tag}` has no result")))?;
    serde_json::from_str(text)
        .map_err(|error| StoreError::Corrupt(format!("cannot decode result: {error}")))
}

/// Why the store refused the operation.
#[derive(Debug)]
pub enum StoreError {
    Sqlite(rusqlite::Error),
    Poisoned,
    NewerSchema { found: i64, supported: i64 },
    Refused(String),
    Corrupt(String),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(error) => write!(f, "sqlite: {error}"),
            Self::Poisoned => f.write_str("the store lock is poisoned"),
            Self::NewerSchema { found, supported } => write!(
                f,
                "the database schema is version {found}, this binary supports {supported}"
            ),
            Self::Refused(message) => write!(f, "the store refused the operation: {message}"),
            Self::Corrupt(message) => write!(f, "cannot decode the store: {message}"),
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sqlite(error) => Some(error),
            Self::Poisoned | Self::NewerSchema { .. } | Self::Refused(_) | Self::Corrupt(_) => None,
        }
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::{SCHEMA_VERSION, Store, StoreError, journal_mode_is_usable, migrate};

    #[test]
    fn durability_pragmas_are_explicit() {
        let store = Store::open_in_memory().unwrap();
        let connection = store.connection.lock().unwrap();
        let synchronous: i64 = connection
            .query_row("PRAGMA synchronous", [], |row| row.get(0))
            .unwrap();
        let busy_timeout: i64 = connection
            .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
            .unwrap();
        let foreign_keys: i64 = connection
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .unwrap();
        assert_eq!(synchronous, 2);
        assert_eq!(busy_timeout, 1000);
        assert_eq!(foreign_keys, 1);
        drop(connection);
    }

    #[test]
    fn journal_modes_are_wal_or_in_memory() {
        assert!(journal_mode_is_usable("wal"));
        assert!(journal_mode_is_usable("memory"));
        assert!(!journal_mode_is_usable("delete"));
        assert!(!journal_mode_is_usable("truncate"));
    }

    #[test]
    fn the_schema_version_is_recorded() {
        let store = Store::open_in_memory().unwrap();
        let connection = store.connection.lock().unwrap();
        let version: i64 = connection
            .query_row("SELECT version FROM schema_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        drop(connection);
    }

    #[test]
    fn a_missing_migration_step_is_refused() {
        let store = Store::open_in_memory().unwrap();
        let mut connection = store.connection.lock().unwrap();
        let error = migrate(&mut connection, SCHEMA_VERSION + 1).unwrap_err();
        assert!(matches!(error, StoreError::Refused(_)));
        assert!(error.to_string().contains("no migration"));
        drop(connection);
    }

    #[test]
    fn a_sqlite_error_keeps_its_source() {
        use std::error::Error;

        let error = StoreError::Sqlite(rusqlite::Error::InvalidQuery);
        assert!(error.source().is_some());
        assert!(StoreError::Poisoned.source().is_none());
    }
}
