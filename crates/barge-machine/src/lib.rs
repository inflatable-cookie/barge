//! Machine records, Wake-on-LAN and the JSON types shared with `barged`.

#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

pub const REGISTRY_SCHEMA: &str = "barge.machines/1";
pub const STATE_SCHEMA: &str = "barge.machine-state/1";
pub const POWER_ACTION_SCHEMA: &str = "barge.power-action/1";
pub const POWER_REQUEST_SCHEMA: &str = "barge.power-request/1";
pub const CANCEL_SCHEMA: &str = "barge.power-cancel/1";
const DEFAULT_BROADCAST: Ipv4Addr = Ipv4Addr::new(255, 255, 255, 255);
const DEFAULT_PORT: u16 = 9;
static PUBLISH_COUNTER: AtomicU64 = AtomicU64::new(0);

/// One registered machine and its LAN wake destination.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Machine {
    pub name: String,
    pub hostname: String,
    pub tailscale_name: Option<String>,
    pub mac: String,
    pub broadcast: Ipv4Addr,
    pub port: u16,
}

impl Machine {
    pub fn new(name: impl Into<String>, hostname: impl Into<String>, mac: &str) -> Result<Self> {
        let name = name.into();
        let hostname = hostname.into();
        validate_name(&name)?;
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
        })
    }

    pub fn tailscale_name(mut self, value: Option<String>) -> Self {
        self.tailscale_name = value;
        self
    }

    pub fn broadcast(mut self, value: Ipv4Addr) -> Self {
        self.broadcast = value;
        self
    }

    pub fn port(mut self, value: u16) -> Self {
        self.port = value;
        self
    }
}

/// Machine state reported by the future daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct State {
    pub schema: String,
    pub power: PowerState,
    pub observed_at_unix_ms: u64,
}

impl State {
    pub fn new(power: PowerState, observed_at_unix_ms: u64) -> Self {
        Self {
            schema: STATE_SCHEMA.to_string(),
            power,
            observed_at_unix_ms,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PowerState {
    Awake,
    Asleep,
    Unknown,
}

/// A daemon power operation; each standalone JSON document is versioned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PowerAction {
    pub schema: String,
    pub action: PowerActionKind,
}

impl PowerAction {
    pub fn new(action: PowerActionKind) -> Self {
        Self {
            schema: POWER_ACTION_SCHEMA.to_string(),
            action,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PowerActionKind {
    Sleep,
    Restart,
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PowerRequest {
    pub schema: String,
    pub action: PowerActionKind,
    pub delay_s: u32,
}

impl PowerRequest {
    pub fn new(action: PowerActionKind, delay_s: u32) -> Self {
        Self {
            schema: POWER_REQUEST_SCHEMA.to_string(),
            action,
            delay_s,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cancel {
    pub schema: String,
}

impl Default for Cancel {
    fn default() -> Self {
        Self {
            schema: CANCEL_SCHEMA.to_string(),
        }
    }
}

/// A domain error reported by the registry or wake operation.
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

    pub fn usage(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Usage, message)
    }
    pub fn refused(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Refused, message)
    }
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::NotFound, message)
    }
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    Usage,
    Refused,
    NotFound,
    Io,
    UnknownState,
}

pub type Result<T> = std::result::Result<T, Error>;

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

    pub fn list(&self) -> Result<Vec<Machine>> {
        self.read()
    }

    pub fn add(&self, machine: Machine) -> Result<()> {
        validate_machine(&machine)?;
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
            validate_machine(machine)?;
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

fn validate_machine(machine: &Machine) -> Result<()> {
    validate_name(&machine.name)?;
    if machine.hostname.trim().is_empty() {
        return Err(Error::new(
            ErrorKind::UnknownState,
            "registered hostname must not be empty",
        ));
    }
    if machine.port == 0 {
        return Err(Error::new(
            ErrorKind::UnknownState,
            "registered WoL port must be 1-65535",
        ));
    }
    let parsed = parse_mac(&machine.mac)
        .map_err(|_| Error::new(ErrorKind::UnknownState, "registered MAC address is invalid"))?;
    if format_mac(parsed) != machine.mac {
        return Err(Error::new(
            ErrorKind::UnknownState,
            "registered MAC address is not canonical",
        ));
    }
    Ok(())
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    {
        return Err(Error::usage(
            "machine name must be 1-64 letters, digits, '.', '_' or '-'",
        ));
    }
    Ok(())
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

/// Minimal injectable UDP sender; tests can inspect traffic without a network send.
pub trait DatagramSender {
    fn send_to(&self, bytes: &[u8], destination: SocketAddr) -> io::Result<usize>;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct UdpSender;

impl DatagramSender for UdpSender {
    fn send_to(&self, bytes: &[u8], destination: SocketAddr) -> io::Result<usize> {
        let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
        socket.set_broadcast(true)?;
        socket.send_to(bytes, destination)
    }
}

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

fn publish(path: &Path, bytes: &[u8]) -> Result<()> {
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
    Ok(())
}

fn io_error(action: &str, path: &Path, source: &io::Error) -> Error {
    Error::new(
        ErrorKind::Io,
        format!("{action} {}: {source}", path.display()),
    )
}

#[derive(Debug)]
struct StateLock(File);

impl StateLock {
    fn acquire(root: &Path) -> Result<Self> {
        let path = root.join("lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| io_error("cannot open account state lock", &path, &e))?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            // SAFETY: flock operates on the descriptor owned by this guard.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == -1 {
                return Err(io_error(
                    "cannot acquire account state lock",
                    &path,
                    &io::Error::last_os_error(),
                ));
            }
        }
        Ok(Self(file))
    }
}

impl Drop for StateLock {
    fn drop(&mut self) {
        let _ = &self.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeSender(Mutex<Vec<(Vec<u8>, SocketAddr)>>);
    impl DatagramSender for FakeSender {
        fn send_to(&self, bytes: &[u8], destination: SocketAddr) -> io::Result<usize> {
            self.0.lock().unwrap().push((bytes.to_vec(), destination));
            Ok(bytes.len())
        }
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
            .broadcast(Ipv4Addr::new(192, 168, 1, 255));
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
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "barge-machine-test-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        let registry = Registry::open(&path).unwrap();
        let machine = Machine::new("living-room", "pc.local", "001122334455").unwrap();
        registry.add(machine.clone()).unwrap();
        assert_eq!(registry.list().unwrap(), vec![machine.clone()]);
        assert_eq!(
            registry.add(machine).unwrap_err().kind(),
            ErrorKind::Refused
        );
        let _ = fs::remove_dir_all(path);
    }

    #[test]
    fn protocol_documents_round_trip_with_schema() {
        let docs = [
            serde_json::to_value(State::new(PowerState::Awake, 42)).unwrap(),
            serde_json::to_value(PowerAction::new(PowerActionKind::Sleep)).unwrap(),
            serde_json::to_value(PowerRequest::new(PowerActionKind::Shutdown, 15)).unwrap(),
            serde_json::to_value(Cancel::default()).unwrap(),
        ];
        for document in &docs {
            assert!(document.get("schema").is_some());
        }
        let state: State = serde_json::from_value(docs[0].clone()).unwrap();
        let action: PowerAction = serde_json::from_value(docs[1].clone()).unwrap();
        let request: PowerRequest = serde_json::from_value(docs[2].clone()).unwrap();
        let cancel: Cancel = serde_json::from_value(docs[3].clone()).unwrap();
        assert_eq!(state.observed_at_unix_ms, 42);
        assert_eq!(action.action, PowerActionKind::Sleep);
        assert_eq!(request.delay_s, 15);
        assert_eq!(cancel, Cancel::default());
        assert_eq!(serde_json::to_value(state).unwrap(), docs[0]);
        assert_eq!(serde_json::to_value(action).unwrap(), docs[1]);
        assert_eq!(serde_json::to_value(request).unwrap(), docs[2]);
        assert_eq!(serde_json::to_value(cancel).unwrap(), docs[3]);
    }
}
