//! What the errors of this crate share: an I/O failure that keeps the
//! system's message, and the chain formatting the reports print.

use std::error::Error;
use std::fmt;
use std::io;

/// An error and its causes on one line: the error's message, then `": "`
/// and each [`source`](Error::source) in turn, the same text as anyhow's
/// `{:#}`. Formatting it does not allocate, so in-play and between-play
/// reports use it.
#[derive(Clone, Copy, Debug)]
pub struct Chain<'a>(pub &'a (dyn Error + 'static));

impl fmt::Display for Chain<'_> {
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)?;
        let mut cause = self.0.source();
        while let Some(e) = cause {
            write!(f, ": {e}")?;
            cause = e.source();
        }
        Ok(())
    }
}

/// An I/O failure: its kind, and the message the system gave, which the
/// player sees. Unlike [`io::Error`] it is `Clone` and `Eq`, so the errors
/// holding it keep theirs.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct IoFailure {
    /// The kind of failure.
    pub kind: io::ErrorKind,
    message: String,
}

impl IoFailure {
    /// The message the system gave.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl From<io::Error> for IoFailure {
    fn from(e: io::Error) -> Self {
        Self {
            kind: e.kind(),
            message: e.to_string(),
        }
    }
}
