//! Barge's one error type: a kind the CLI maps to an exit code and a message
//! that names the thing that refused.

use std::fmt;
use std::path::Path;

/// What kind of failure this is. The CLI maps it to an exit code: `Refused` and
/// `NotFound` change nothing and exit 1, `Usage` exits 2, everything else exits
/// 3.
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
///
/// An I/O failure keeps the source's `std::io::ErrorKind` as well as its text, so
/// a caller can tell a missing file from a refused one without parsing a message.
#[derive(Debug, Clone)]
pub struct Error {
    kind: ErrorKind,
    message: String,
    source_kind: Option<std::io::ErrorKind>,
}

impl Error {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            source_kind: None,
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

    /// An I/O failure with the operation and path that failed, and the source's
    /// [`std::io::ErrorKind`].
    pub fn io(context: &str, path: &Path, source: &std::io::Error) -> Self {
        Self {
            kind: ErrorKind::Io,
            message: format!("{context} {}: {source}", path.display()),
            source_kind: Some(source.kind()),
        }
    }

    /// An I/O failure whose message is already written out, with no source to
    /// take a kind from. Prefer [`Error::io`] wherever an `io::Error` exists.
    pub fn io_message(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Io, message)
    }

    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// The `std::io::ErrorKind` behind an [`ErrorKind::Io`] failure, when the
    /// failure came from one.
    pub fn io_kind(&self) -> Option<std::io::ErrorKind> {
        self.source_kind
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_io_failure_keeps_the_source_kind() {
        let missing = std::io::Error::from(std::io::ErrorKind::NotFound);
        let error = Error::io("cannot read", Path::new("/nowhere"), &missing);
        assert_eq!(error.kind(), ErrorKind::Io);
        assert_eq!(error.io_kind(), Some(std::io::ErrorKind::NotFound));
        assert!(error.message().contains("/nowhere"), "{error}");

        let refused = Error::refused("no");
        assert_eq!(refused.io_kind(), None, "only an I/O failure carries one");
        assert_eq!(Error::io_message("gone").io_kind(), None);
    }
}
