//! Machine records, Wake-on-LAN, the JSON types shared with `barged` and the
//! daemon client.
//!
//! The registry is `machines.json` in the account state directory, which is the
//! same directory the storage library keeps its documents in. Publication and
//! the account state lock are a copy of `barge::state` (documented in
//! `docs/knowledge/machines.md`, "Registry state"): both take the same `lock`
//! file, so a registry write and a storage write never interleave, and the two
//! copies have to be changed together.
#![deny(missing_docs)]
#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]

use std::fs::{self, File};
use std::io::{self, Write};
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

pub mod daemon;

pub use daemon::{request_power, AssumeYes, Confirmation, DaemonClient};

/// The schema of `machines.json`.
pub const REGISTRY_SCHEMA: &str = "barge.machines/1";
/// The schema of the state `barged` reports.
pub const STATE_SCHEMA: &str = "barge.machine-state/1";
/// The schema of a power action document.
pub const POWER_ACTION_SCHEMA: &str = "barge.power-action/1";
/// The schema of a power request sent to `barged`.
pub const POWER_REQUEST_SCHEMA: &str = "barge.power-request/1";
/// The schema of `barged`'s acceptance of a power request.
pub const POWER_RESULT_SCHEMA: &str = "barge.power-result/1";
/// The schema of a cancel request sent to `barged`.
pub const CANCEL_SCHEMA: &str = "barge.power-cancel/1";
/// The schema of `barged`'s answer to a cancel.
pub const CANCEL_RESULT_SCHEMA: &str = "barge.power-cancel-result/1";
/// The schema of the error document every `barged` refusal carries.
pub const DAEMON_ERROR_SCHEMA: &str = "barge.daemon-error/1";
const DEFAULT_BROADCAST: Ipv4Addr = Ipv4Addr::new(255, 255, 255, 255);
const DEFAULT_PORT: u16 = 9;
/// `barged`'s default port. The daemon's config names the address it binds; the
/// registry names the port it dials, so either side can be moved deliberately.
pub const DEFAULT_DAEMON_PORT: u16 = 9777;
/// Power commands default to the countdown MoonDeck Remote used.
pub const DEFAULT_POWER_DELAY_S: u32 = 15;
/// A countdown longer than this is a mistake, not a schedule.
pub const MAX_POWER_DELAY_S: u32 = 3600;
static PUBLISH_COUNTER: AtomicU64 = AtomicU64::new(0);

/// One registered machine and its LAN wake destination.
///
/// The fields are private: a record is built through [`Machine::new`] and the
/// `with_*` setters, so an invalid value is refused where it is given rather
/// than at the registry (a usage error, not an unknown state). A record read
/// from disk is validated on every read, so even a hand-edited file cannot
/// reach a caller unchecked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Machine {
    name: String,
    hostname: String,
    tailscale_name: Option<String>,
    mac: String,
    broadcast: Ipv4Addr,
    port: u16,
    /// The port `barged` listens on. Records written before step 2 read as the
    /// default, so an existing registry keeps working.
    #[serde(default = "default_daemon_port")]
    daemon_port: u16,
}

fn default_daemon_port() -> u16 {
    DEFAULT_DAEMON_PORT
}

impl Machine {
    /// A machine with the default broadcast address, WoL port and daemon port.
    ///
    /// The name, hostname and MAC address are operator input, so a bad value is
    /// a usage error.
    pub fn new(name: impl Into<String>, hostname: impl Into<String>, mac: &str) -> Result<Self> {
        let name = name.into();
        let hostname = hostname.into();
        if !valid_name(&name) {
            return Err(Error::usage(NAME_RULE));
        }
        if hostname.trim().is_empty() {
            return Err(Error::usage("hostname must not be empty"));
        }
        Ok(Self {
            name,
            hostname,
            tailscale_name: None,
            mac: format_mac(parse_mac(mac)?),
            broadcast: DEFAULT_BROADCAST,
            port: DEFAULT_PORT,
            daemon_port: DEFAULT_DAEMON_PORT,
        })
    }

    /// The registry name, unique within the account.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The hostname used when the machine has no recorded tailnet name.
    pub fn hostname(&self) -> &str {
        &self.hostname
    }

    /// The tailnet identity recorded at pairing, when there is one.
    pub fn tailscale_name(&self) -> Option<&str> {
        self.tailscale_name.as_deref()
    }

    /// The MAC address as the canonical `00:11:22:33:44:ff` form.
    pub fn mac(&self) -> &str {
        &self.mac
    }

    /// The IPv4 address the Wake-on-LAN packet is sent to.
    pub fn broadcast(&self) -> Ipv4Addr {
        self.broadcast
    }

    /// The UDP port the Wake-on-LAN packet is sent to.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The port `barged` listens on.
    pub fn daemon_port(&self) -> u16 {
        self.daemon_port
    }

    /// Record the tailnet identity used instead of the hostname.
    pub fn with_tailscale_name(mut self, value: Option<String>) -> Self {
        self.tailscale_name = value;
        self
    }

    /// Set the broadcast address the Wake-on-LAN packet goes to.
    pub fn with_broadcast(mut self, value: Ipv4Addr) -> Self {
        self.broadcast = value;
        self
    }

    /// Set the Wake-on-LAN UDP port; zero is a usage error.
    pub fn with_port(mut self, value: u16) -> Result<Self> {
        if value == 0 {
            return Err(Error::usage("Wake-on-LAN port must be 1-65535"));
        }
        self.port = value;
        Ok(self)
    }

    /// Set the port `barged` listens on; zero is a usage error.
    pub fn with_daemon_port(mut self, value: u16) -> Result<Self> {
        if value == 0 {
            return Err(Error::usage("daemon port must be 1-65535"));
        }
        self.daemon_port = value;
        Ok(self)
    }

    /// The host a client dials: the tailnet name when one is recorded.
    pub fn daemon_host(&self) -> &str {
        self.tailscale_name.as_deref().unwrap_or(&self.hostname)
    }
}

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

/// A daemon power operation; each standalone JSON document is versioned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PowerAction {
    /// The document's schema, [`POWER_ACTION_SCHEMA`].
    pub schema: String,
    /// The action named.
    pub action: PowerActionKind,
}

impl PowerAction {
    /// A power action document naming `action`.
    pub fn new(action: PowerActionKind) -> Self {
        Self {
            schema: POWER_ACTION_SCHEMA.to_string(),
            action,
        }
    }
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

/// A domain error reported by the registry, a wake operation or the client.
#[derive(Debug)]
pub struct Error {
    kind: ErrorKind,
    message: String,
}

impl Error {
    fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    /// Bad input from the operator: a name, a hostname, a port or a MAC.
    pub fn usage(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Usage, message)
    }

    /// A well-formed request that was refused, for example a duplicate name.
    pub fn refused(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Refused, message)
    }

    /// A machine or other named record that does not exist.
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::NotFound, message)
    }

    /// Which class of failure this is; the CLI maps it to its exit code.
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// The human-readable explanation.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for Error {}

/// The class of a [`Error`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// Bad input from the operator.
    Usage,
    /// A well-formed request that was refused; nothing changed.
    Refused,
    /// The named record does not exist.
    NotFound,
    /// Reading, writing or sending failed.
    Io,
    /// Stored state could not be read or is not valid; nothing is assumed.
    UnknownState,
    /// The machine's daemon could not be reached. Distinct from every state a
    /// daemon can report, so an absent daemon is never read as asleep.
    Unreachable,
}

/// This crate's result type.
pub type Result<T> = std::result::Result<T, Error>;

/// The rule a machine name has to meet, quoted in every usage error.
const NAME_RULE: &str = "machine name must be 1-64 letters, digits, '.', '_' or '-'";

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RegistryDocument {
    schema: String,
    machines: Vec<Machine>,
}

/// The registry stored at `machines.json` under the Barge account state root.
#[derive(Debug, Clone)]
pub struct Registry {
    path: PathBuf,
    state_dir: PathBuf,
}

impl Registry {
    /// The registry in the account state directory named by `BARGE_STATE_DIR`,
    /// `XDG_STATE_HOME` or the platform default.
    pub fn discover() -> Result<Self> {
        let state_dir = if let Some(path) =
            std::env::var_os("BARGE_STATE_DIR").filter(|v| !v.is_empty())
        {
            PathBuf::from(path)
        } else if let Some(path) = std::env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()) {
            PathBuf::from(path).join("barge")
        } else {
            let home = std::env::var_os("HOME")
                .filter(|v| !v.is_empty())
                .ok_or_else(|| {
                    Error::new(
                        ErrorKind::UnknownState,
                        "cannot locate Barge account state; set BARGE_STATE_DIR",
                    )
                })?;
            if cfg!(target_os = "macos") {
                PathBuf::from(home).join("Library/Application Support/barge")
            } else {
                PathBuf::from(home).join(".local/state/barge")
            }
        };
        Self::open(state_dir)
    }

    /// The registry in an explicit state directory, created when it does not
    /// exist.
    pub fn open(state_dir: impl Into<PathBuf>) -> Result<Self> {
        let state_dir = state_dir.into();
        if !state_dir.is_absolute() {
            return Err(Error::usage(format!(
                "state directory {} is not absolute",
                state_dir.display()
            )));
        }
        fs::create_dir_all(&state_dir)
            .map_err(|e| io_error("cannot create state directory", &state_dir, &e))?;
        Ok(Self {
            path: state_dir.join("machines.json"),
            state_dir,
        })
    }

    /// Every registered machine, in name order.
    pub fn list(&self) -> Result<Vec<Machine>> {
        self.read()
    }

    /// Register a machine. A name already in the registry is refused, and the
    /// record is checked again here so a machine built by hand cannot slip in.
    pub fn add(&self, machine: Machine) -> Result<()> {
        if let Some(fault) = fault(&machine) {
            return Err(Error::usage(format!(
                "machine {:?} is not valid: {fault}",
                machine.name
            )));
        }
        let _lock = StateLock::acquire(&self.state_dir)?;
        let mut machines = self.read()?;
        if machines.iter().any(|entry| entry.name == machine.name) {
            return Err(Error::refused(format!(
                "machine {:?} is already registered",
                machine.name
            )));
        }
        machines.push(machine);
        machines.sort_by(|a, b| a.name.cmp(&b.name));
        self.write(&machines)
    }

    /// Remove a machine by name and return what was removed.
    pub fn remove(&self, name: &str) -> Result<Machine> {
        let _lock = StateLock::acquire(&self.state_dir)?;
        let mut machines = self.read()?;
        let index = machines
            .iter()
            .position(|entry| entry.name == name)
            .ok_or_else(|| Error::not_found(format!("machine {name:?} is not registered")))?;
        let removed = machines.remove(index);
        self.write(&machines)?;
        Ok(removed)
    }

    /// One machine by name.
    pub fn get(&self, name: &str) -> Result<Machine> {
        self.read()?
            .into_iter()
            .find(|entry| entry.name == name)
            .ok_or_else(|| Error::not_found(format!("machine {name:?} is not registered")))
    }

    fn read(&self) -> Result<Vec<Machine>> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(io_error("cannot read registry", &self.path, &e)),
        };
        let doc: RegistryDocument = serde_json::from_slice(&bytes).map_err(|e| {
            Error::new(
                ErrorKind::UnknownState,
                format!("cannot parse {}: {e}", self.path.display()),
            )
        })?;
        if doc.schema != REGISTRY_SCHEMA {
            return Err(Error::new(
                ErrorKind::UnknownState,
                format!(
                    "{} has schema {}, not {REGISTRY_SCHEMA}",
                    self.path.display(),
                    doc.schema
                ),
            ));
        }
        for machine in &doc.machines {
            if let Some(fault) = fault(machine) {
                return Err(Error::new(
                    ErrorKind::UnknownState,
                    format!(
                        "{} registers an invalid machine {:?}: {fault}",
                        self.path.display(),
                        machine.name
                    ),
                ));
            }
        }
        if doc
            .machines
            .iter()
            .enumerate()
            .any(|(i, a)| doc.machines[..i].iter().any(|b| a.name == b.name))
        {
            return Err(Error::new(
                ErrorKind::UnknownState,
                "machine registry contains duplicate names",
            ));
        }
        Ok(doc.machines)
    }

    fn write(&self, machines: &[Machine]) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(&RegistryDocument {
            schema: REGISTRY_SCHEMA.to_string(),
            machines: machines.to_vec(),
        })
        .map_err(|e| {
            Error::new(
                ErrorKind::UnknownState,
                format!("cannot encode registry: {e}"),
            )
        })?;
        publish(&self.path, &bytes)
    }
}

/// What is wrong with `machine` as a registry record, if anything.
///
/// The caller decides the kind: a value read from disk is an unknown state,
/// while the same value given by an operator is a usage error.
fn fault(machine: &Machine) -> Option<&'static str> {
    if !valid_name(&machine.name) {
        return Some(NAME_RULE);
    }
    if machine.hostname.trim().is_empty() {
        return Some("hostname must not be empty");
    }
    if machine.port == 0 {
        return Some("Wake-on-LAN port must be 1-65535");
    }
    if machine.daemon_port == 0 {
        return Some("daemon port must be 1-65535");
    }
    match parse_mac(&machine.mac) {
        Ok(mac) if format_mac(mac) == machine.mac => None,
        Ok(_) => Some("MAC address is not canonical"),
        Err(_) => Some("MAC address is invalid"),
    }
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// Parse colon, hyphen or compact 12-digit MAC addresses.
pub fn parse_mac(value: &str) -> Result<[u8; 6]> {
    let bytes = value.as_bytes();
    let compact = match bytes.len() {
        12 if bytes.iter().all(u8::is_ascii_hexdigit) => value.to_string(),
        17 if matches!(bytes[2], b':' | b'-')
            && [2, 5, 8, 11, 14]
                .iter()
                .all(|index| bytes[*index] == bytes[2])
            && bytes.iter().enumerate().all(|(index, byte)| {
                [2, 5, 8, 11, 14].contains(&index) || byte.is_ascii_hexdigit()
            }) =>
        {
            value
                .chars()
                .filter(|character| *character != ':' && *character != '-')
                .collect()
        }
        14 if bytes[4] == b'.'
            && bytes[9] == b'.'
            && bytes
                .iter()
                .enumerate()
                .all(|(index, byte)| [4, 9].contains(&index) || byte.is_ascii_hexdigit()) =>
        {
            value
                .chars()
                .filter(|character| *character != '.')
                .collect()
        }
        _ => return Err(Error::usage(format!("invalid MAC address {value:?}"))),
    };
    let mut mac = [0; 6];
    for (i, byte) in mac.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&compact[i * 2..i * 2 + 2], 16)
            .map_err(|_| Error::usage(format!("invalid MAC address {value:?}")))?;
    }
    Ok(mac)
}

fn format_mac(mac: [u8; 6]) -> String {
    mac.iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// Build the 102-byte Wake-on-LAN magic packet.
pub fn magic_packet(mac: [u8; 6]) -> [u8; 102] {
    let mut packet = [0xff; 102];
    for chunk in packet[6..].chunks_exact_mut(6) {
        chunk.copy_from_slice(&mac);
    }
    packet
}

/// Minimal injectable UDP sender; tests can inspect traffic without a network
/// send.
pub trait DatagramSender {
    /// Send one datagram to `destination`, reporting how many bytes went out.
    fn send_to(&self, bytes: &[u8], destination: SocketAddr) -> io::Result<usize>;
}

/// The real sender: one UDP socket with broadcast enabled.
#[derive(Debug, Default, Clone, Copy)]
pub struct UdpSender;

impl DatagramSender for UdpSender {
    fn send_to(&self, bytes: &[u8], destination: SocketAddr) -> io::Result<usize> {
        let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
        socket.set_broadcast(true)?;
        socket.send_to(bytes, destination)
    }
}

/// Send the magic packet for `machine` and report the address it went to.
pub fn wake(machine: &Machine, sender: &impl DatagramSender) -> Result<SocketAddr> {
    let destination = SocketAddr::from((machine.broadcast, machine.port));
    let sent = sender
        .send_to(&magic_packet(parse_mac(&machine.mac)?), destination)
        .map_err(|e| {
            Error::new(
                ErrorKind::Io,
                format!("cannot send Wake-on-LAN packet to {destination}: {e}"),
            )
        })?;
    if sent != 102 {
        return Err(Error::new(
            ErrorKind::Io,
            format!("sent {sent} of 102 Wake-on-LAN bytes to {destination}"),
        ));
    }
    Ok(destination)
}

/// Write `bytes` to `path` through a temporary file in the same directory and an
/// atomic rename, so a reader sees the old document or the new one, never half.
///
/// This is the same publication step as `barge::state::publish`, kept here so
/// the storage library's dependency tree does not grow (docs/knowledge/machines.md,
/// "Registry state").
fn publish(path: &Path, bytes: &[u8]) -> Result<()> {
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("registry");
    let temporary = path.with_file_name(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        PUBLISH_COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    let mut file = File::create(&temporary)
        .map_err(|e| io_error("cannot create registry temporary file", &temporary, &e))?;
    if let Err(e) = file.write_all(bytes).and_then(|()| file.sync_all()) {
        let _ = fs::remove_file(&temporary);
        return Err(io_error("cannot write registry", &temporary, &e));
    }
    if let Err(e) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(io_error("cannot publish registry", path, &e));
    }
    // Best effort: make the rename durable without making it a failure.
    if let Ok(directory) = File::open(directory) {
        let _ = directory.sync_all();
    }
    Ok(())
}

fn io_error(action: &str, path: &Path, source: &io::Error) -> Error {
    Error::new(
        ErrorKind::Io,
        format!("{action} {}: {source}", path.display()),
    )
}

/// The held account state lock.
///
/// The `lock` file is the one `barge::state` also takes, so registry writes and
/// storage writes exclude each other. Closing the descriptor releases the lock,
/// which is why there is no explicit unlock and no `Drop` body.
#[derive(Debug)]
struct StateLock {
    /// Held only for the lock its descriptor carries; never read.
    _file: File,
}

impl StateLock {
    fn acquire(root: &Path) -> Result<Self> {
        let path = root.join("lock");
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| io_error("cannot open account state lock", &path, &e))?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            // SAFETY: flock operates on the descriptor owned by this guard, and
            // the lock is released when that descriptor is closed.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == -1 {
                return Err(io_error(
                    "cannot acquire account state lock",
                    &path,
                    &io::Error::last_os_error(),
                ));
            }
        }
        Ok(Self { _file: file })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use barge::testing::TempDir;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeSender(Mutex<Vec<(Vec<u8>, SocketAddr)>>);
    impl DatagramSender for FakeSender {
        fn send_to(&self, bytes: &[u8], destination: SocketAddr) -> io::Result<usize> {
            self.0.lock().unwrap().push((bytes.to_vec(), destination));
            Ok(bytes.len())
        }
    }

    /// A fresh registry directory; the `TempDir` is returned so it outlives the
    /// registry and removes itself even when a test panics.
    fn registry() -> (TempDir, Registry) {
        let temp = TempDir::new("machine-registry").unwrap();
        let registry = Registry::open(temp.path()).unwrap();
        (temp, registry)
    }

    /// One machine record as a registry entry.
    fn machine_json(name: &str, mac: &str) -> String {
        format!(
            r#"{{"name":"{name}","hostname":"pc.local","tailscale_name":null,"mac":"{mac}","broadcast":"255.255.255.255","port":9,"daemon_port":9777}}"#
        )
    }

    /// A registry document holding `machines`, as another writer would have
    /// left it.
    fn registry_json(machines: &[(&str, &str)]) -> String {
        let entries = machines
            .iter()
            .map(|(name, mac)| machine_json(name, mac))
            .collect::<Vec<_>>()
            .join(",");
        format!(r#"{{"schema":"{REGISTRY_SCHEMA}","machines":[{entries}]}}"#)
    }

    fn document(name: &str, mac: &str) -> String {
        registry_json(&[(name, mac)])
    }

    fn write_document(temp: &TempDir, bytes: &str) {
        std::fs::write(temp.child("machines.json"), bytes).unwrap();
    }

    #[test]
    fn parses_common_macs_and_rejects_invalid_values() {
        let expected = [0, 17, 34, 51, 68, 255];
        for input in [
            "00:11:22:33:44:ff",
            "00-11-22-33-44-FF",
            "0011223344ff",
            "0011.2233.44ff",
        ] {
            assert_eq!(parse_mac(input).unwrap(), expected);
        }
        for input in [
            "",
            "0011223344",
            "0011223344gg",
            "00:11:22:33:44:55:66",
            "00-11:22:33:44:ff",
        ] {
            assert!(parse_mac(input).is_err(), "{input}");
        }
    }

    #[test]
    fn magic_packet_and_wake_destination_are_exact() {
        let mac = [0, 17, 34, 51, 68, 255];
        let packet = magic_packet(mac);
        assert_eq!(&packet[..6], &[255; 6]);
        assert!(packet[6..].chunks_exact(6).all(|chunk| chunk == mac));
        let machine = Machine::new("pc", "pc.local", "00:11:22:33:44:ff")
            .unwrap()
            .with_broadcast(Ipv4Addr::new(192, 168, 1, 255));
        let sender = FakeSender::default();
        assert_eq!(
            wake(&machine, &sender).unwrap(),
            SocketAddr::from(([192, 168, 1, 255], 9))
        );
        let sent = sender.0.lock().unwrap();
        assert_eq!(sent[0].0, packet);
        assert_eq!(sent[0].1, SocketAddr::from(([192, 168, 1, 255], 9)));
    }

    #[test]
    fn registry_round_trips_and_refuses_duplicate_names() {
        let (_temp, registry) = registry();
        let machine = Machine::new("living-room", "pc.local", "001122334455").unwrap();
        registry.add(machine.clone()).unwrap();
        assert_eq!(registry.list().unwrap(), vec![machine.clone()]);
        assert_eq!(registry.get("living-room").unwrap(), machine);
        assert_eq!(
            registry.add(machine).unwrap_err().kind(),
            ErrorKind::Refused
        );
    }

    #[test]
    fn get_and_remove_of_a_missing_name_are_not_found() {
        let (_temp, registry) = registry();
        assert_eq!(
            registry.get("absent").unwrap_err().kind(),
            ErrorKind::NotFound
        );
        assert_eq!(
            registry.remove("absent").unwrap_err().kind(),
            ErrorKind::NotFound
        );
        assert!(registry.list().unwrap().is_empty());
    }

    #[test]
    fn an_unknown_registry_schema_is_unknown_state() {
        let (temp, registry) = registry();
        let bytes =
            document("pc", "00:11:22:33:44:ff").replace(REGISTRY_SCHEMA, "barge.machines/2");
        write_document(&temp, &bytes);
        let error = registry.list().unwrap_err();
        assert_eq!(error.kind(), ErrorKind::UnknownState);
        assert!(error.message().contains("barge.machines/2"), "{error}");
    }

    #[test]
    fn corrupt_registry_json_is_unknown_state() {
        let (temp, registry) = registry();
        write_document(&temp, "{ not json");
        let error = registry.list().unwrap_err();
        assert_eq!(error.kind(), ErrorKind::UnknownState);
        assert!(error.message().contains("cannot parse"), "{error}");
        assert_eq!(
            registry.get("pc").unwrap_err().kind(),
            ErrorKind::UnknownState,
            "a get reads the same document"
        );
    }

    #[test]
    fn duplicate_names_on_disk_are_unknown_state() {
        let (temp, registry) = registry();
        write_document(
            &temp,
            &registry_json(&[("pc", "00:11:22:33:44:ff"), ("pc", "00:11:22:33:44:ff")]),
        );
        let error = registry.list().unwrap_err();
        assert_eq!(error.kind(), ErrorKind::UnknownState);
        assert!(error.message().contains("duplicate names"), "{error}");
    }

    #[test]
    fn a_non_canonical_mac_on_disk_is_unknown_state() {
        let (temp, registry) = registry();
        write_document(&temp, &document("pc", "00-11-22-33-44-FF"));
        let error = registry.list().unwrap_err();
        assert_eq!(error.kind(), ErrorKind::UnknownState);
        assert!(error.message().contains("not canonical"), "{error}");
    }

    #[test]
    fn a_corrupt_name_on_disk_is_unknown_state_not_usage() {
        let (temp, registry) = registry();
        write_document(&temp, &document("not a name", "00:11:22:33:44:ff"));
        let error = registry.list().unwrap_err();
        assert_eq!(
            error.kind(),
            ErrorKind::UnknownState,
            "a stored record is not operator input; the CLI must exit 3, not 2"
        );
        assert!(error.message().contains(NAME_RULE), "{error}");
    }

    #[test]
    fn a_port_is_validated_where_the_machine_is_built() {
        let machine = Machine::new("pc", "pc.local", "00:11:22:33:44:ff").unwrap();
        assert_eq!(
            machine.clone().with_port(0).unwrap_err().kind(),
            ErrorKind::Usage
        );
        assert_eq!(
            machine.clone().with_daemon_port(0).unwrap_err().kind(),
            ErrorKind::Usage
        );
        let chosen = machine
            .clone()
            .with_port(7)
            .unwrap()
            .with_daemon_port(1)
            .unwrap();
        assert_eq!((chosen.port(), chosen.daemon_port()), (7, 1));
        assert_eq!(machine.port(), DEFAULT_PORT);
        assert_eq!(machine.daemon_port(), DEFAULT_DAEMON_PORT);
    }

    #[test]
    fn add_re_validates_a_record_that_bypassed_the_builders() {
        let (_temp, registry) = registry();
        let machine: Machine = serde_json::from_str(
            &machine_json("pc", "00:11:22:33:44:ff").replace("\"port\":9", "\"port\":0"),
        )
        .unwrap();
        let error = registry.add(machine).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Usage);
        assert!(error.message().contains("1-65535"), "{error}");
        assert!(registry.list().unwrap().is_empty());
    }

    #[test]
    fn protocol_documents_round_trip_with_schema() {
        let docs = [
            serde_json::to_value(
                State::new(PowerState::Awake, 42).with_pending(Some(Pending {
                    action: PowerActionKind::Sleep,
                    deadline_unix_ms: 15_042,
                })),
            )
            .unwrap(),
            serde_json::to_value(State::new(PowerState::Asleep, 42)).unwrap(),
            serde_json::to_value(PowerAction::new(PowerActionKind::Sleep)).unwrap(),
            serde_json::to_value(PowerRequest::new(PowerActionKind::Shutdown, 15)).unwrap(),
            serde_json::to_value(PowerResult::new(PowerActionKind::Restart, 15_000)).unwrap(),
            serde_json::to_value(Cancel::default()).unwrap(),
            serde_json::to_value(CancelResult::new(true)).unwrap(),
            serde_json::to_value(DaemonError::new("not_authorized", "no")).unwrap(),
        ];
        for document in &docs {
            assert!(document.get("schema").is_some());
        }
        let state: State = serde_json::from_value(docs[0].clone()).unwrap();
        let no_pending: State = serde_json::from_value(docs[1].clone()).unwrap();
        let action: PowerAction = serde_json::from_value(docs[2].clone()).unwrap();
        let request: PowerRequest = serde_json::from_value(docs[3].clone()).unwrap();
        let result: PowerResult = serde_json::from_value(docs[4].clone()).unwrap();
        let cancel: Cancel = serde_json::from_value(docs[5].clone()).unwrap();
        let cancelled: CancelResult = serde_json::from_value(docs[6].clone()).unwrap();
        let error: DaemonError = serde_json::from_value(docs[7].clone()).unwrap();
        assert_eq!(state.schema, STATE_SCHEMA);
        assert_eq!(state.observed_at_unix_ms, 42);
        assert_eq!(
            state.pending,
            Some(Pending {
                action: PowerActionKind::Sleep,
                deadline_unix_ms: 15_042,
            })
        );
        assert_eq!(no_pending.pending, None);
        assert!(docs[1].get("pending").is_none());
        assert_eq!(action.action, PowerActionKind::Sleep);
        assert_eq!(request.delay_s, 15);
        assert_eq!(result.schema, POWER_RESULT_SCHEMA);
        assert_eq!(result.action, PowerActionKind::Restart);
        assert_eq!(cancel, Cancel::default());
        assert_eq!(cancelled.schema, CANCEL_RESULT_SCHEMA);
        assert!(cancelled.cancelled);
        assert_eq!(error.schema, DAEMON_ERROR_SCHEMA);
        assert_eq!(error.error, "not_authorized");
        assert_eq!(serde_json::to_value(state).unwrap(), docs[0]);
        assert_eq!(serde_json::to_value(no_pending).unwrap(), docs[1]);
        assert_eq!(serde_json::to_value(action).unwrap(), docs[2]);
        assert_eq!(serde_json::to_value(request).unwrap(), docs[3]);
        assert_eq!(serde_json::to_value(result).unwrap(), docs[4]);
        assert_eq!(serde_json::to_value(cancel).unwrap(), docs[5]);
        assert_eq!(serde_json::to_value(cancelled).unwrap(), docs[6]);
        assert_eq!(serde_json::to_value(error).unwrap(), docs[7]);
    }

    #[test]
    fn a_record_written_before_step_two_reads_the_default_daemon_port() {
        let document = serde_json::json!({
            "name": "living-room",
            "hostname": "pc.local",
            "tailscale_name": null,
            "mac": "00:11:22:33:44:ff",
            "broadcast": "255.255.255.255",
            "port": 9,
        });
        let machine: Machine = serde_json::from_value(document).unwrap();
        assert_eq!(machine.daemon_port(), DEFAULT_DAEMON_PORT);
        assert_eq!(machine.daemon_host(), "pc.local");
        let round_trip = serde_json::to_value(&machine).unwrap();
        assert_eq!(round_trip["daemon_port"], DEFAULT_DAEMON_PORT);
    }
}
