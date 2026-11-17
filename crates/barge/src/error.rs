//! Barge's one error type: a kind the CLI maps to an exit code and a message
//! that names the thing that refused.

use std::fmt;
use std::path::Path;

/// What kind of failure this is. The CLI maps it to an exit code: `Refused`
/// changes nothing and exits 1, `Usage` exits 2, everything else exits 3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// The caller's request is malformed or incomplete.
    Usage,
    /// A policy refused the request; nothing changed.
    Refused,
    /// A named volume, area, root or pin does not exist.
    NotFound,
    /// Stored state is missing, unreadable or inconsistent. Treat as "keep".
    UnknownState,
    /// An operating-system operation failed.
    Io,
    /// The platform cannot supply the fact that was asked for.
    Unsupported,
}

impl ErrorKind {
    /// The stable snake_case name used in the CLI's JSON error document.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Usage => "usage",
            Self::Refused => "refused",
            Self::NotFound => "not_found",
            Self::UnknownState => "unknown_state",
            Self::Io => "io",
            Self::Unsupported => "unsupported",
        }
    }
}

/// Barge's error: a [`ErrorKind`] and a message that names what refused.
#[derive(Debug, Clone)]
pub struct Error {
    kind: ErrorKind,
    message: String,
}

impl Error {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub fn usage(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Usage, message)
    }

    pub fn refused(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Refused, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::NotFound, message)
    }

    pub fn unknown_state(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::UnknownState, message)
    }

    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Unsupported, message)
    }

    /// An I/O failure with the operation and path that failed.
    pub fn io(context: &str, path: &Path, source: &std::io::Error) -> Self {
        Self::new(
            ErrorKind::Io,
            format!("{context} {}: {source}", path.display()),
        )
    }

    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

/// Barge's result alias.
pub type Result<T> = std::result::Result<T, Error>;
