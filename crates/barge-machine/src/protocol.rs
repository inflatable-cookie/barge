//! Shared daemon protocol documents.
use std::collections::BTreeMap;
use std::net::Ipv4Addr;

use barge::run::{LogRef, RunOwner, RunResult, RunStage};

use crate::{
    CANCEL_RESULT_SCHEMA, CANCEL_SCHEMA, CLIENTS_SCHEMA, CLIENT_REVOKED_SCHEMA,
    DAEMON_ERROR_SCHEMA, PAIR_APPROVED_SCHEMA, PAIR_DECISION_SCHEMA, PAIR_DENIED_SCHEMA,
    PAIR_PENDING_SCHEMA, PAIR_REQUEST_SCHEMA, PAIR_RESPONSE_SCHEMA, PAIR_STATUS_SCHEMA,
    POWER_REQUEST_SCHEMA, POWER_RESULT_SCHEMA, RUN_CANCEL_REQUEST_SCHEMA,
    RUN_CANCEL_RESPONSE_SCHEMA, RUN_FINISH_RESPONSE_SCHEMA, RUN_HAVE_REQUEST_SCHEMA,
    RUN_HAVE_RESPONSE_SCHEMA, RUN_LOG_RESPONSE_SCHEMA, RUN_START_REQUEST_SCHEMA,
    RUN_START_RESPONSE_SCHEMA, RUN_STREAM_EVENT_SCHEMA, RUN_UPLOAD_REQUEST_SCHEMA,
    RUN_UPLOAD_RESPONSE_SCHEMA, STATE_SCHEMA,
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

/// Request sent to `POST /v1/runs/have` to ask which commits the daemon's bare
/// mirror of `repo` already contains.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunHaveRequest {
    /// The document's schema, [`RUN_HAVE_REQUEST_SCHEMA`].
    pub schema: String,
    /// Repository URL or stable identifier.
    pub repo: String,
    /// Candidate commit SHAs to probe in the mirror.
    pub commits: Vec<String>,
}

impl RunHaveRequest {
    /// Build a mirror commit-probe request.
    pub fn new(repo: impl Into<String>, commits: Vec<String>) -> Self {
        Self {
            schema: RUN_HAVE_REQUEST_SCHEMA.to_string(),
            repo: repo.into(),
            commits,
        }
    }
}

/// `barged`'s response to `POST /v1/runs/have`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunHaveResponse {
    /// The document's schema, [`RUN_HAVE_RESPONSE_SCHEMA`].
    pub schema: String,
    /// Repository URL or stable identifier.
    pub repo: String,
    /// Subset of requested commits already present in the mirror.
    pub have: Vec<String>,
}

impl RunHaveResponse {
    /// Build a mirror commit-probe response.
    pub fn new(repo: impl Into<String>, have: Vec<String>) -> Self {
        Self {
            schema: RUN_HAVE_RESPONSE_SCHEMA.to_string(),
            repo: repo.into(),
            have,
        }
    }
}

/// Request sent to `POST /v1/runs/upload` carrying missing git objects as a
/// bundle and/or an optional binary patch for a run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunUploadRequest {
    /// The document's schema, [`RUN_UPLOAD_REQUEST_SCHEMA`].
    pub schema: String,
    /// Caller-assigned run request ID that associates a staged patch with `start`.
    pub request_id: String,
    /// Repository URL or stable identifier.
    pub repo: String,
    /// Base64-encoded git bundle of objects missing from the remote mirror.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_base64: Option<String>,
    /// Base64-encoded `git diff --binary` patch to apply on top of `base`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patch_base64: Option<String>,
}

impl RunUploadRequest {
    /// Build an upload request for `request_id` and `repo`.
    pub fn new(
        request_id: impl Into<String>,
        repo: impl Into<String>,
        bundle_base64: Option<String>,
        patch_base64: Option<String>,
    ) -> Self {
        Self {
            schema: RUN_UPLOAD_REQUEST_SCHEMA.to_string(),
            request_id: request_id.into(),
            repo: repo.into(),
            bundle_base64,
            patch_base64,
        }
    }
}

/// `barged`'s response to `POST /v1/runs/upload`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunUploadResponse {
    /// The document's schema, [`RUN_UPLOAD_RESPONSE_SCHEMA`].
    pub schema: String,
    /// The run request ID associated with this upload.
    pub request_id: String,
    /// Repository URL or stable identifier.
    pub repo: String,
    /// True when a git bundle was applied to the bare mirror.
    pub bundle_applied: bool,
    /// SHA-256 digest (`sha256:<hex>`) of the staged patch, when one was supplied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patch_digest: Option<String>,
}

impl RunUploadResponse {
    /// Build an upload acknowledgement.
    pub fn new(
        request_id: impl Into<String>,
        repo: impl Into<String>,
        bundle_applied: bool,
        patch_digest: Option<String>,
    ) -> Self {
        Self {
            schema: RUN_UPLOAD_RESPONSE_SCHEMA.to_string(),
            request_id: request_id.into(),
            repo: repo.into(),
            bundle_applied,
            patch_digest,
        }
    }
}

/// Request sent to `POST /v1/runs/start` to execute a command in a Barge build
/// area on a specific repo slot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunStartRequest {
    /// The document's schema, [`RUN_START_REQUEST_SCHEMA`].
    pub schema: String,
    /// Unique identifier for this run, used for streaming, cancel, result and log lookup.
    pub request_id: String,
    /// Repository URL or stable identifier.
    pub repo: String,
    /// Base commit SHA to check out from the bare mirror.
    pub base: String,
    /// Expected tree identity (`git write-tree` SHA, or `<head>+<write-tree>`).
    pub expected_tree: String,
    /// Command argument vector, executed directly with no shell.
    pub command: Vec<String>,
    /// Explicit environment allowlist; nothing else is inherited from `barged`.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Wall-clock execution budget in milliseconds.
    pub budget_ms: u64,
    /// Slot number from the caller's machine lease; concurrent runs on the same
    /// repo plus slot are refused.
    pub slot: u32,
    /// Who owns this run.
    pub owner: RunOwner,
}

impl RunStartRequest {
    /// Build a start-run request.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        request_id: impl Into<String>,
        repo: impl Into<String>,
        base: impl Into<String>,
        expected_tree: impl Into<String>,
        command: Vec<String>,
        env: BTreeMap<String, String>,
        budget_ms: u64,
        slot: u32,
        owner: RunOwner,
    ) -> Self {
        Self {
            schema: RUN_START_REQUEST_SCHEMA.to_string(),
            request_id: request_id.into(),
            repo: repo.into(),
            base: base.into(),
            expected_tree: expected_tree.into(),
            command,
            env,
            budget_ms,
            slot,
            owner,
        }
    }
}

/// `barged`'s response to `POST /v1/runs/start`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunStartResponse {
    /// The document's schema, [`RUN_START_RESPONSE_SCHEMA`].
    pub schema: String,
    /// The accepted run request ID.
    pub request_id: String,
    /// Path to stream live events over chunked HTTP (`GET`).
    pub stream_path: String,
    /// Path to fetch the final [`RunFinishResponse`] (`GET`).
    pub result_path: String,
    /// Path to cancel the run (`POST`).
    pub cancel_path: String,
}

impl RunStartResponse {
    /// Build a start-run acceptance document.
    pub fn new(request_id: impl Into<String>) -> Self {
        let request_id = request_id.into();
        Self {
            schema: RUN_START_RESPONSE_SCHEMA.to_string(),
            stream_path: format!("/v1/runs/{request_id}/stream"),
            result_path: format!("/v1/runs/{request_id}/result"),
            cancel_path: format!("/v1/runs/{request_id}/cancel"),
            request_id,
        }
    }
}

/// Which output pipe produced a streamed chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunOutputStream {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
}

/// One event emitted on the chunked NDJSON stream `GET /v1/runs/<id>/stream`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunStreamEvent {
    /// The document's schema, [`RUN_STREAM_EVENT_SCHEMA`].
    pub schema: String,
    /// Monotonic per-run sequence number starting at 1.
    pub seq: u64,
    /// Event timestamp in Unix milliseconds.
    pub timestamp_unix_ms: u64,
    /// Event payload.
    pub event: RunStreamEventKind,
}

impl RunStreamEvent {
    /// Build a streamed run event.
    pub fn new(seq: u64, timestamp_unix_ms: u64, event: RunStreamEventKind) -> Self {
        Self {
            schema: RUN_STREAM_EVENT_SCHEMA.to_string(),
            seq,
            timestamp_unix_ms,
            event,
        }
    }
}

/// Payload of a [`RunStreamEvent`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RunStreamEventKind {
    /// Execution entered a new stage (`syncing`, `toolchain`, `running`).
    Stage {
        /// The stage now running.
        stage: RunStage,
    },
    /// A chunk of live command output.
    Output {
        /// Whether the bytes came from stdout or stderr.
        stream: RunOutputStream,
        /// Base64-encoded raw output bytes.
        data_base64: String,
    },
    /// Terminal event carrying the complete [`RunResult`].
    Finished {
        /// Final run result, outcome and log reference.
        result: Box<RunResult>,
    },
}

/// Request sent to `POST /v1/runs/<id>/cancel`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunCancelRequest {
    /// The document's schema, [`RUN_CANCEL_REQUEST_SCHEMA`].
    pub schema: String,
}

impl Default for RunCancelRequest {
    fn default() -> Self {
        Self {
            schema: RUN_CANCEL_REQUEST_SCHEMA.to_string(),
        }
    }
}

/// `barged`'s response to `POST /v1/runs/<id>/cancel`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunCancelResponse {
    /// The document's schema, [`RUN_CANCEL_RESPONSE_SCHEMA`].
    pub schema: String,
    /// The run request ID.
    pub request_id: String,
    /// True when cancellation was signalled to an active run.
    pub cancelled: bool,
}

impl RunCancelResponse {
    /// Build a run-cancel response.
    pub fn new(request_id: impl Into<String>, cancelled: bool) -> Self {
        Self {
            schema: RUN_CANCEL_RESPONSE_SCHEMA.to_string(),
            request_id: request_id.into(),
            cancelled,
        }
    }
}

/// `barged`'s response to `GET /v1/runs/<id>/result`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunFinishResponse {
    /// The document's schema, [`RUN_FINISH_RESPONSE_SCHEMA`].
    pub schema: String,
    /// The run request ID.
    pub request_id: String,
    /// Final execution result, including [`barge::run::RunOutcome`] and [`LogRef`].
    pub result: RunResult,
}

impl RunFinishResponse {
    /// Build a final run-result document.
    pub fn new(request_id: impl Into<String>, result: RunResult) -> Self {
        Self {
            schema: RUN_FINISH_RESPONSE_SCHEMA.to_string(),
            request_id: request_id.into(),
            result,
        }
    }
}

/// `barged`'s response to `GET /v1/runs/logs/<id>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunLogResponse {
    /// The document's schema, [`RUN_LOG_RESPONSE_SCHEMA`].
    pub schema: String,
    /// Reference matching the fetched log's digest and location.
    pub log: LogRef,
    /// Base64-encoded full raw log bytes.
    pub content_base64: String,
}

impl RunLogResponse {
    /// Build a fetched log response.
    pub fn new(log: LogRef, content_base64: impl Into<String>) -> Self {
        Self {
            schema: RUN_LOG_RESPONSE_SCHEMA.to_string(),
            log,
            content_base64: content_base64.into(),
        }
    }
}
