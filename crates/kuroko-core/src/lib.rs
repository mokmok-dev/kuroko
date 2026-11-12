//! Pure domain: task types, `Book`, `apply`, `Limits`, `Clock`.

mod book;
mod task;

pub use book::{Book, apply};
pub use task::{
    Clock, Cmd, DelegateId, Dispatch, Done, Event, FailureReason, IssueId, LabelName, Limits,
    Millis, Outcome, Source, Step, Submission, Task, TaskKey, TaskRequest, TaskSpec, TaskState,
    TriggerId,
};
