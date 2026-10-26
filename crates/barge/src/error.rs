//! The closed failure kinds and the `barge.error/1` document.
//!
//! A failure always carries one kind from the closed set the contract defines
//! (stage 1 design, "Versioned JSON and error contract"), so a consumer can
//! branch on the kind instead of parsing a message. The kind also decides the
//! process exit code, which is why both live here.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::contract::ERROR_SCHEMA;

/// The result type every fallible library call returns.
pub type Result<T> = std::result::Result<T, Error>;

/// The closed set of failure kinds. A new kind is a contract change, not an
/// addition: a consumer written against this set must never meet an unknown
/// one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// The command line or a library argument was not usable.
    Usage,
    /// The request would cross a boundary Barge does not cross.
    RefusedBoundary,
    /// A path escaped the folder or volume it had to stay inside.
    RefusedContainment,
    /// The volume that holds the path is not attached.
    VolumeAbsent,
    /// A different volume is at the expected place.
    VolumeMismatch,
    /// A fact could not be read or did not validate; unknown is never a default.
    UnknownState,
    /// The data belongs to someone else.
    ForeignOwner,
    /// Custody could not be proven, so the effect is refused.
    CustodyNotProven,
    /// A declared bound was reached before the work was finished.
    ResourceBound,
    /// A lock is held by another live owner.
    Locked,
    /// A read or write failed for a reason the other kinds do not name.
    Io,
}

impl ErrorKind {
    /// The kind as it appears in `barge.error/1`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Usage => "usage",
            Self::RefusedBoundary => "refused_boundary",
            Self::RefusedContainment => "refused_containment",
            Self::VolumeAbsent => "volume_absent",
            Self::VolumeMismatch => "volume_mismatch",
            Self::UnknownState => "unknown_state",
            Self::ForeignOwner => "foreign_owner",
            Self::CustodyNotProven => "custody_not_proven",
            Self::ResourceBound => "resource_bound",
            Self::Locked => "locked",
            Self::Io => "io",
        }
    }

    /// The exit code this kind forces: `2` when the command refused before any
    /// effect, `3` when the state is unknown or unsettled and needs repair or an
    /// operator decision. An unreadable fact or a failed read leaves the state
    /// unknown, so `unknown_state` and `io` are both `3`.
    pub const fn exit_code(self) -> u8 {
        match self {
            Self::Usage
            | Self::RefusedBoundary
            | Self::RefusedContainment
            | Self::VolumeAbsent
            | Self::VolumeMismatch
            | Self::ForeignOwner
            | Self::CustodyNotProven
            | Self::ResourceBound
            | Self::Locked => 2,
            Self::UnknownState | Self::Io => 3,
        }
    }
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A classified failure: a kind from the closed set, a message for a human and
/// bounded detail for a machine. There is no constructor without a kind, so no
/// failure reaches a document unclassified.
#[derive(Debug, Clone, PartialEq)]
pub struct Error {
    kind: ErrorKind,
    message: String,
    detail: BTreeMap<String, Value>,
}

impl Error {
    /// A failure of `kind`, described by `message`.
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            detail: BTreeMap::new(),
        }
    }

    /// Attach one detail entry. Callers pass only facts they actually read, so
    /// detail never carries a guess.
    #[must_use]
    pub fn with_detail(mut self, key: &str, value: Value) -> Self {
        self.detail.insert(key.to_owned(), value);
        self
    }

    /// The closed kind this failure is.
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// The human-readable message.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// The machine-readable detail, sorted by key.
    pub fn detail(&self) -> &BTreeMap<String, Value> {
        &self.detail
    }

    /// The `barge.error/1` document for this failure.
    pub fn document<'a>(&'a self, command: &'a str) -> ErrorDocument<'a> {
        ErrorDocument {
            schema: ERROR_SCHEMA,
            command,
            ok: false,
            error: ErrorBody {
                kind: self.kind,
                message: &self.message,
                detail: &self.detail,
            },
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

/// `barge.error/1`: the one document a failed command prints.
#[derive(Debug, Serialize)]
pub struct ErrorDocument<'a> {
    /// Always `barge.error/1`.
    pub schema: &'static str,
    /// The command that failed, or `barge` when no command could be named.
    pub command: &'a str,
    /// Always `false`.
    pub ok: bool,
    /// The classified failure.
    pub error: ErrorBody<'a>,
}

/// The failure body of `barge.error/1`.
#[derive(Debug, Serialize)]
pub struct ErrorBody<'a> {
    /// One value from the closed set.
    pub kind: ErrorKind,
    /// What happened, for a human.
    pub message: &'a str,
    /// Bounded facts, for a machine.
    pub detail: &'a BTreeMap<String, Value>,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::{Error, ErrorKind};
    use serde_json::Value;

    #[test]
    fn every_kind_has_its_contract_name_and_exit_code() {
        let expected = [
            (ErrorKind::Usage, "usage", 2),
            (ErrorKind::RefusedBoundary, "refused_boundary", 2),
            (ErrorKind::RefusedContainment, "refused_containment", 2),
            (ErrorKind::VolumeAbsent, "volume_absent", 2),
            (ErrorKind::VolumeMismatch, "volume_mismatch", 2),
            (ErrorKind::UnknownState, "unknown_state", 3),
            (ErrorKind::ForeignOwner, "foreign_owner", 2),
            (ErrorKind::CustodyNotProven, "custody_not_proven", 2),
            (ErrorKind::ResourceBound, "resource_bound", 2),
            (ErrorKind::Locked, "locked", 2),
            (ErrorKind::Io, "io", 3),
        ];
        for (kind, name, exit_code) in expected {
            assert_eq!(kind.as_str(), name);
            assert_eq!(kind.exit_code(), exit_code, "{name} exit code");
            let json = serde_json::to_value(kind).expect("a kind serialises");
            assert_eq!(json, Value::String(name.to_owned()));
        }
    }

    #[test]
    fn the_kind_set_is_closed() {
        assert!(serde_json::from_value::<ErrorKind>(Value::String("banana".to_owned())).is_err());
    }

    #[test]
    fn a_document_names_the_kind_and_keeps_detail_an_object() {
        let error = Error::new(ErrorKind::VolumeMismatch, "a different volume is attached")
            .with_detail("expected_volume_uuid", "AAAA".into())
            .with_detail("actual_volume_uuid", "BBBB".into());
        let document =
            serde_json::to_value(error.document("describe")).expect("a document encodes");
        assert_eq!(document["schema"], "barge.error/1");
        assert_eq!(document["command"], "describe");
        assert_eq!(document["ok"], false);
        assert_eq!(document["error"]["kind"], "volume_mismatch");
        assert_eq!(
            document["error"]["message"],
            "a different volume is attached"
        );
        assert_eq!(document["error"]["detail"]["actual_volume_uuid"], "BBBB");
        assert!(document["error"]["detail"].is_object());
    }
}
