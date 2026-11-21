//! Shared daemon protocol documents.
use std::net::Ipv4Addr;

use crate::{
    CANCEL_RESULT_SCHEMA, CANCEL_SCHEMA, CLIENTS_SCHEMA, CLIENT_REVOKED_SCHEMA,
    DAEMON_ERROR_SCHEMA, PAIR_APPROVED_SCHEMA, PAIR_DECISION_SCHEMA, PAIR_DENIED_SCHEMA,
    PAIR_PENDING_SCHEMA, PAIR_REQUEST_SCHEMA, PAIR_RESPONSE_SCHEMA, PAIR_STATUS_SCHEMA,
    POWER_REQUEST_SCHEMA, POWER_RESULT_SCHEMA, STATE_SCHEMA,
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
    /// The daemon's stable random machine ID.
    pub machine_id: String,
    /// The MAC address of the interface `barged` binds, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mac: Option<String>,
    /// The broadcast address of the interface `barged` binds, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub broadcast: Option<Ipv4Addr>,
    /// The node's MagicDNS name on the tailnet, when `barged` serves there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tailscale_name: Option<String>,
    /// The node's tailnet IPv4 address, when `barged` serves there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tailscale_ip: Option<Ipv4Addr>,
}

impl State {
    /// A state with no pending action.
    pub fn new(power: PowerState, observed_at_unix_ms: u64) -> Self {
        Self {
            schema: STATE_SCHEMA.to_string(),
            power,
            observed_at_unix_ms,
            pending: None,
            machine_id: String::new(),
            mac: None,
            broadcast: None,
            tailscale_name: None,
            tailscale_ip: None,
        }
    }

    /// Record the node's tailnet identity, when `barged` serves on the tailnet.
    pub fn with_tailscale(mut self, name: Option<String>, ip: Option<Ipv4Addr>) -> Self {
        self.tailscale_name = name;
        self.tailscale_ip = ip;
        self
    }

    /// Record the action counting down, if any.
    pub fn with_pending(mut self, pending: Option<Pending>) -> Self {
        self.pending = pending;
        self
    }

    /// Record the daemon's stable machine ID.
    pub fn with_machine_id(mut self, machine_id: impl Into<String>) -> Self {
        self.machine_id = machine_id.into();
        self
    }

    /// Record the bound interface's MAC address.
    pub fn with_mac(mut self, mac: Option<String>) -> Self {
        self.mac = mac;
        self
    }

    /// Record the bound interface's broadcast address.
    pub fn with_broadcast(mut self, broadcast: Option<Ipv4Addr>) -> Self {
        self.broadcast = broadcast;
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

/// A pairing request sent to `POST /v1/pair`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairRequest {
    /// The document's schema, [`PAIR_REQUEST_SCHEMA`].
    pub schema: String,
    /// Display name of the client requesting pairing.
    pub client_name: String,
    /// Client's 32-byte X25519 public key as 64 lowercase hex characters.
    pub public_key: String,
}

impl PairRequest {
    /// Build a pairing initiation document.
    pub fn new(client_name: impl Into<String>, public_key: impl Into<String>) -> Self {
        Self {
            schema: PAIR_REQUEST_SCHEMA.to_string(),
            client_name: client_name.into(),
            public_key: public_key.into(),
        }
    }
}

/// `barged`'s response to `POST /v1/pair`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairResponse {
    /// The document's schema, [`PAIR_RESPONSE_SCHEMA`].
    pub schema: String,
    /// Unique identifier for polling and approval.
    pub request_id: String,
    /// Daemon's 32-byte ephemeral X25519 public key as 64 lowercase hex characters.
    pub public_key: String,
    /// When the pending pairing request expires, in Unix milliseconds.
    pub expires_at_unix_ms: u64,
}

impl PairResponse {
    /// Build a pairing response document.
    pub fn new(
        request_id: impl Into<String>,
        public_key: impl Into<String>,
        expires_at_unix_ms: u64,
    ) -> Self {
        Self {
            schema: PAIR_RESPONSE_SCHEMA.to_string(),
            request_id: request_id.into(),
            public_key: public_key.into(),
            expires_at_unix_ms,
        }
    }
}

/// State of a pairing request returned by `GET /v1/pair/<id>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PairStatus {
    /// Waiting for approval.
    Pending,
    /// Approved; the client ID is assigned.
    Approved,
    /// Denied by an operator or trusted client.
    Denied,
    /// Timed out after two minutes.
    Expired,
}

/// `barged`'s response to `GET /v1/pair/<id>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairStatusResponse {
    /// The document's schema, [`PAIR_STATUS_SCHEMA`].
    pub schema: String,
    /// The pairing request ID.
    pub request_id: String,
    /// Current status of the request.
    pub status: PairStatus,
    /// Assigned client ID once approved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
}

impl PairStatusResponse {
    /// Build a pairing status document.
    pub fn new(
        request_id: impl Into<String>,
        status: PairStatus,
        client_id: Option<String>,
    ) -> Self {
        Self {
            schema: PAIR_STATUS_SCHEMA.to_string(),
            request_id: request_id.into(),
            status,
            client_id,
        }
    }
}

/// One pending pairing request returned by `GET /v1/pair/pending`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingPairRequest {
    /// The pairing request ID.
    pub request_id: String,
    /// Name of the client requesting pairing.
    pub client_name: String,
    /// Six-digit verification code.
    pub code: String,
    /// Creation time in Unix milliseconds.
    pub created_at_unix_ms: u64,
    /// Expiry time in Unix milliseconds.
    pub expires_at_unix_ms: u64,
}

/// `barged`'s response to `GET /v1/pair/pending`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingPairList {
    /// The document's schema, [`PAIR_PENDING_SCHEMA`].
    pub schema: String,
    /// Currently pending, unexpired pairing requests.
    pub requests: Vec<PendingPairRequest>,
}

impl PendingPairList {
    /// Build a pending pairing list document.
    pub fn new(requests: Vec<PendingPairRequest>) -> Self {
        Self {
            schema: PAIR_PENDING_SCHEMA.to_string(),
            requests,
        }
    }
}

/// Optional body for `POST /v1/pair/<id>/approve` and `POST /v1/pair/<id>/deny`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairDecision {
    /// The document's schema, [`PAIR_DECISION_SCHEMA`].
    pub schema: String,
}

impl Default for PairDecision {
    fn default() -> Self {
        Self {
            schema: PAIR_DECISION_SCHEMA.to_string(),
        }
    }
}

/// A paired client's public metadata (never includes the key).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairedClient {
    /// The client's unique ID sent in `x-barge-client-id`.
    pub id: String,
    /// The client's display name.
    pub name: String,
    /// When the client paired (or migrated), in Unix milliseconds.
    pub paired_at_unix_ms: u64,
}

/// `barged`'s response to `POST /v1/pair/<id>/approve`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairApproved {
    /// The document's schema, [`PAIR_APPROVED_SCHEMA`].
    pub schema: String,
    /// The approved pairing request ID.
    pub request_id: String,
    /// The newly registered client metadata.
    pub client: PairedClient,
}

impl PairApproved {
    /// Build an approval response document.
    pub fn new(request_id: impl Into<String>, client: PairedClient) -> Self {
        Self {
            schema: PAIR_APPROVED_SCHEMA.to_string(),
            request_id: request_id.into(),
            client,
        }
    }
}

/// `barged`'s response to `POST /v1/pair/<id>/deny`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairDenied {
    /// The document's schema, [`PAIR_DENIED_SCHEMA`].
    pub schema: String,
    /// The denied pairing request ID.
    pub request_id: String,
}

impl PairDenied {
    /// Build a denial response document.
    pub fn new(request_id: impl Into<String>) -> Self {
        Self {
            schema: PAIR_DENIED_SCHEMA.to_string(),
            request_id: request_id.into(),
        }
    }
}

/// `barged`'s response to `GET /v1/clients`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientList {
    /// The document's schema, [`CLIENTS_SCHEMA`].
    pub schema: String,
    /// All paired clients (without keys).
    pub clients: Vec<PairedClient>,
}

impl ClientList {
    /// Build a client list document.
    pub fn new(clients: Vec<PairedClient>) -> Self {
        Self {
            schema: CLIENTS_SCHEMA.to_string(),
            clients,
        }
    }
}

/// `barged`'s response to `DELETE /v1/clients/<id>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientRevoked {
    /// The document's schema, [`CLIENT_REVOKED_SCHEMA`].
    pub schema: String,
    /// The ID of the revoked client.
    pub client_id: String,
}

impl ClientRevoked {
    /// Build a client revocation document.
    pub fn new(client_id: impl Into<String>) -> Self {
        Self {
            schema: CLIENT_REVOKED_SCHEMA.to_string(),
            client_id: client_id.into(),
        }
    }
}
