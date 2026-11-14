use std::num::NonZeroUsize;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Millis(pub i64);

pub trait Clock: Send + Sync {
    fn now_ms(&self) -> Millis;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub enum Source {
    Linear,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct IssueId(String);

impl IssueId {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct DelegateId(String);

impl DelegateId {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct LabelName(String);

impl LabelName {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub enum TriggerId {
    Delegate(DelegateId),
    Label(LabelName),
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct TaskKey {
    pub source: Source,
    pub issue: IssueId,
    pub trigger: TriggerId,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
pub struct TaskSpec {
    pub identifier: String,
    pub title: String,
    pub description: Option<String>,
    pub url: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TaskRequest {
    pub key: TaskKey,
    pub spec: TaskSpec,
    pub prompt: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FailureReason {
    Exit { code: Option<i32> },
    Timeout,
    Disconnected,
    BadResult,
    Panicked,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Outcome {
    Success,
    Failure(FailureReason),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Done {
    pub at: Millis,
    pub outcome: Outcome,
    pub output: String,
    pub log: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
pub enum TaskState {
    Queued,
    Dispatched,
    /// A dispatch was recorded but the daemon stopped before recording its result.
    /// It is never retried automatically because the agent may still be running.
    RecoveryPending,
    Succeeded(Done),
    Failed(Done),
    /// Tombstone for a withdrawn task. The key stays present so redelivery is a duplicate.
    Withdrawn,
}

impl TaskState {
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::Succeeded(_) | Self::Failed(_) | Self::Withdrawn)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
pub struct Task {
    pub key: TaskKey,
    pub spec: TaskSpec,
    pub prompt: String,
    pub state: TaskState,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Event {
    Admit(TaskRequest),
    Withdraw(TaskKey),
    Finished {
        key: TaskKey,
        done: Done,
    },
    /// Records a result without draining the queue. Used once shutdown has begun, so a
    /// completed dispatch is persisted even though no new work may start.
    Settled {
        key: TaskKey,
        done: Done,
    },
    /// Drains restored queued work after startup. `RecoveryPending` rows are skipped, so
    /// it can never replay a dispatch whose agent may still be running.
    Resume,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Dispatch {
    pub key: TaskKey,
    pub issue: String,
    pub prompt: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Cmd {
    Dispatch(Dispatch),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Submission {
    Accepted,
    Duplicate,
    Full,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Step {
    pub submission: Option<Submission>,
    pub cmds: Vec<Cmd>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Limits {
    pub in_flight: NonZeroUsize,
    pub max_queued: NonZeroUsize,
}
