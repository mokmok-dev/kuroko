//! Pure domain: task types, `Book`, `apply`, `Limits`, `Clock`, `Journal`.

mod book;
mod journal;
mod task;

pub use book::{Book, apply};
pub use journal::{Journal, JournalError};
pub use task::{
    Clock, Cmd, DelegateId, Dispatch, Done, Event, FailureReason, IssueId, LabelName, Limits,
    Millis, Outcome, Source, Step, Submission, Task, TaskKey, TaskRequest, TaskSpec, TaskState,
    TriggerId,
};
