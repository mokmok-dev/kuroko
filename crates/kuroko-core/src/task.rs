use std::num::NonZeroUsize;

use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
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
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct DelegateId(String);

impl DelegateId {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct LabelName(String);

impl LabelName {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
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

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
pub enum FailureReason {
    Exit { code: Option<i32> },
    Timeout,
    Disconnected,
    BadResult,
    Panicked,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
pub enum Outcome {
    Success,
    Failure(FailureReason),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
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
    Succeeded(Done),
    Failed(Done),
}

impl TaskState {
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::Succeeded(_) | Self::Failed(_))
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
    Finished { key: TaskKey, done: Done },
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
