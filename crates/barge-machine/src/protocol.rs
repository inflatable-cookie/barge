//! Shared daemon protocol documents.
use crate::{
    CANCEL_RESULT_SCHEMA, CANCEL_SCHEMA, DAEMON_ERROR_SCHEMA, POWER_REQUEST_SCHEMA,
    POWER_RESULT_SCHEMA, STATE_SCHEMA,
};
use serde::{Deserialize, Serialize};

/// Machine state reported by `barged`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct State {
    /// The document's schema, [`STATE_SCHEMA`].
    pub schema: String,
    /// The power state `barged` observed.
    pub power: PowerState,
    /// When that state was observed, in Unix milliseconds.
    pub observed_at_unix_ms: u64,
    /// The action counting down, when one is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<Pending>,
}

impl State {
    /// A state with no pending action.
    pub fn new(power: PowerState, observed_at_unix_ms: u64) -> Self {
        Self {
            schema: STATE_SCHEMA.to_string(),
            power,
            observed_at_unix_ms,
            pending: None,
        }
    }

    /// Record the action counting down, if any.
    pub fn with_pending(mut self, pending: Option<Pending>) -> Self {
        self.pending = pending;
        self
    }
}

/// A power action that will fire when its deadline passes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pending {
    /// The action that will fire.
    pub action: PowerActionKind,
    /// When it fires, in Unix milliseconds.
    pub deadline_unix_ms: u64,
}

/// The power state a machine was observed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PowerState {
    /// The machine is awake.
    Awake,
    /// The machine is asleep.
    Asleep,
    /// The state could not be observed.
    Unknown,
}

/// The closed set of power actions `barged` performs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PowerActionKind {
    /// Suspend the machine.
    Sleep,
    /// Reboot the machine.
    Restart,
    /// Power the machine off.
    Shutdown,
}

impl PowerActionKind {
    /// The stable snake_case name used in documents and messages.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sleep => "sleep",
            Self::Restart => "restart",
            Self::Shutdown => "shutdown",
        }
    }
}

/// The daemon's acceptance of a power request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PowerResult {
    /// The document's schema, [`POWER_RESULT_SCHEMA`].
    pub schema: String,
    /// The action accepted.
    pub action: PowerActionKind,
    /// When it will fire, in Unix milliseconds.
    pub deadline_unix_ms: u64,
}

impl PowerResult {
    /// The acceptance of `action`, firing at `deadline_unix_ms`.
    pub fn new(action: PowerActionKind, deadline_unix_ms: u64) -> Self {
        Self {
            schema: POWER_RESULT_SCHEMA.to_string(),
            action,
            deadline_unix_ms,
        }
    }
}

/// The daemon's answer to a cancel: whether a countdown was stopped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancelResult {
    /// The document's schema, [`CANCEL_RESULT_SCHEMA`].
    pub schema: String,
    /// True when a countdown was stopped.
    pub cancelled: bool,
}

impl CancelResult {
    /// An answer saying whether a countdown was stopped.
    pub fn new(cancelled: bool) -> Self {
        Self {
            schema: CANCEL_RESULT_SCHEMA.to_string(),
            cancelled,
        }
    }
}

/// The daemon's error document; every non-2xx response carries one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonError {
    /// The document's schema, [`DAEMON_ERROR_SCHEMA`].
    pub schema: String,
    /// A stable machine-readable error name, for example `not_authorized`.
    pub error: String,
    /// The human-readable explanation.
    pub message: String,
}

impl DaemonError {
    /// An error document with a name and an explanation.
    pub fn new(error: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            schema: DAEMON_ERROR_SCHEMA.to_string(),
            error: error.into(),
            message: message.into(),
        }
    }
}

/// A power request sent to `barged`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PowerRequest {
    /// The document's schema, [`POWER_REQUEST_SCHEMA`].
    pub schema: String,
    /// The action requested.
    pub action: PowerActionKind,
    /// The countdown length in seconds.
    pub delay_s: u32,
}

impl PowerRequest {
    /// A request for `action` after `delay_s` seconds.
    pub fn new(action: PowerActionKind, delay_s: u32) -> Self {
        Self {
            schema: POWER_REQUEST_SCHEMA.to_string(),
            action,
            delay_s,
        }
    }
}

/// A cancel request sent to `barged`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cancel {
    /// The document's schema, [`CANCEL_SCHEMA`].
    pub schema: String,
}

impl Default for Cancel {
    fn default() -> Self {
        Self {
            schema: CANCEL_SCHEMA.to_string(),
        }
    }
}
