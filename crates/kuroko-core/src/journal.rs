//! The persistence port the engine calls before acting on a state change.

use std::fmt;

use crate::Book;

/// Persists a candidate [`Book`] before it becomes the live state.
///
/// The engine only dispatches after `persist` returns `Ok`, so a failed write
/// never leaves an agent running for a task the ledger does not know about.
pub trait Journal: Send + Sync {
    /// Persists `book` or reports why it could not be written.
    ///
    /// # Errors
    ///
    /// Returns [`JournalError`] when the write fails; the caller must then leave
    /// the live book and its dispatch queue untouched.
    fn persist(&self, book: &Book) -> Result<(), JournalError>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JournalError(String);

impl JournalError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for JournalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for JournalError {}

#[cfg(test)]
mod tests {
    use super::JournalError;

    #[test]
    fn an_error_displays_its_message() {
        assert_eq!(JournalError::new("disk full").to_string(), "disk full");
    }
}
