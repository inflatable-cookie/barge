//! Machine records and account registry.
use crate::state::{io_error, publish, StateLock};
use crate::wol::{format_mac, parse_mac};
use crate::{
    auth, Error, ErrorKind, Result, DEFAULT_BROADCAST, DEFAULT_DAEMON_PORT, DEFAULT_PORT,
    REGISTRY_SCHEMA,
};
use serde::{Deserialize, Serialize};
use std::{fmt, fs, io, net::Ipv4Addr, path::PathBuf};

/// One registered machine and its LAN wake destination.
///
/// The fields are private: a record is built through [`Machine::new`] (or
/// [`Machine::discovered`]) and the `with_*` setters, so an invalid value is
/// refused where it is given rather than at the registry (a usage error, not an
/// unknown state). A record read from disk is validated on every read, so even
/// a hand-edited file cannot reach a caller unchecked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Machine {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    name: String,
    hostname: String,
    tailscale_name: Option<String>,
    #[serde(default)]
    mac: Option<String>,
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
            id: None,
            name,
            hostname,
            tailscale_name: None,
            mac: Some(format_mac(parse_mac(mac)?)),
            broadcast: DEFAULT_BROADCAST,
            port: DEFAULT_PORT,
            daemon_port: DEFAULT_DAEMON_PORT,
        })
    }

    /// A machine discovered on the LAN before pairing supplies its MAC and
    /// broadcast address.
    pub fn discovered(
        id: impl Into<String>,
        name: impl Into<String>,
        hostname: impl Into<String>,
        daemon_port: u16,
    ) -> Result<Self> {
        let id = id.into();
        let name = name.into();
        let hostname = hostname.into();
        if id.trim().is_empty() {
            return Err(Error::usage("machine id must not be empty"));
        }
        if !valid_name(&name) {
            return Err(Error::usage(NAME_RULE));
        }
        if hostname.trim().is_empty() {
            return Err(Error::usage("hostname must not be empty"));
        }
        if daemon_port == 0 {
            return Err(Error::usage("daemon port must be 1-65535"));
        }
        Ok(Self {
            id: Some(id),
            name,
            hostname,
            tailscale_name: None,
            mac: None,
            broadcast: DEFAULT_BROADCAST,
            port: DEFAULT_PORT,
            daemon_port,
        })
    }

    /// The daemon's stable machine ID, when recorded from discovery or pairing.
    pub fn id(&self) -> Option<&str> {
        self.id.as_deref()
    }

    /// The registry name, unique within the account.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The hostname (or LAN IP) used when the machine has no recorded tailnet name.
    pub fn hostname(&self) -> &str {
        &self.hostname
    }

    /// The tailnet identity recorded at pairing, when there is one.
    pub fn tailscale_name(&self) -> Option<&str> {
        self.tailscale_name.as_deref()
    }

    /// The MAC address as the canonical `00:11:22:33:44:ff` form, when recorded.
    pub fn mac(&self) -> Option<&str> {
        self.mac.as_deref()
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

    /// Record the daemon's stable machine ID.
    pub fn with_id(mut self, value: Option<String>) -> Self {
        self.id = value;
        self
    }

    /// Update the LAN hostname/address.
    pub fn with_hostname(mut self, value: impl Into<String>) -> Result<Self> {
        let hostname = value.into();
        if hostname.trim().is_empty() {
            return Err(Error::usage("hostname must not be empty"));
        }
        self.hostname = hostname;
        Ok(self)
    }

    /// Record or update the canonical MAC address.
    pub fn with_mac(mut self, mac: &str) -> Result<Self> {
        self.mac = Some(format_mac(parse_mac(mac)?));
        Ok(self)
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

/// Stored LAN authentication credentials for one registered machine.
#[derive(Clone, PartialEq, Eq)]
pub struct ClientCredentials {
    /// Client ID sent in `x-barge-client-id`, or `None` for legacy shared keys.
    pub client_id: Option<String>,
    /// 256-bit HMAC key.
    pub key: Vec<u8>,
}

impl fmt::Debug for ClientCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientCredentials")
            .field("client_id", &self.client_id)
            .finish_non_exhaustive()
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredCredentials {
    client_id: String,
    key: String,
}

/// Sanitize a discovered display name into a valid registry machine name.
pub fn sanitize_machine_name(raw: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in raw.trim().chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
            out.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !out.is_empty() && !last_dash {
            out.push('-');
            last_dash = true;
        }
        if out.len() >= 64 {
            break;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        "machine".to_string()
    } else {
        out
    }
}

/// The rule a machine name has to meet, quoted in every usage error.
pub(crate) const NAME_RULE: &str = "machine name must be 1-64 letters, digits, '.', '_' or '-'";

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
        let state_dir =
            if let Some(path) = std::env::var_os("BARGE_STATE_DIR").filter(|v| !v.is_empty()) {
                PathBuf::from(path)
            } else if let Some(path) = std::env::var_os("XDG_STATE_HOME")
                .filter(|v| cfg!(target_os = "linux") && !v.is_empty())
            {
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
        Self::open_read_only(state_dir)
    }

    /// The registry in an explicit state directory, created when it does not
    /// exist.
    pub fn open(state_dir: impl Into<PathBuf>) -> Result<Self> {
        let registry = Self::open_read_only(state_dir)?;
        fs::create_dir_all(&registry.state_dir)
            .map_err(|e| io_error("cannot create state directory", &registry.state_dir, &e))?;
        Ok(registry)
    }

    /// Open an account path without creating state; writes create it on demand.
    pub fn open_read_only(state_dir: impl Into<PathBuf>) -> Result<Self> {
        let state_dir = state_dir.into();
        if !state_dir.is_absolute() {
            return Err(Error::usage(format!(
                "state directory {} is not absolute",
                state_dir.display()
            )));
        }
        Ok(Self {
            path: state_dir.join("machines.json"),
            state_dir,
        })
    }

    /// Store a legacy shared LAN key separately from the public registry.
    pub fn pair(&self, name: &str, key: &str) -> Result<()> {
        let _lock = StateLock::acquire(&self.state_dir)?;
        self.get(name)?;
        if auth::decode(key.trim(), 32).is_none() {
            return Err(Error::usage(
                "key must be 64 lowercase hexadecimal characters",
            ));
        }
        let path = self.key_path(name);
        auth::write_key(&path, key.trim())
            .map_err(|e| io_error("cannot write machine key", &path, &e))
    }

    /// Store a per-client ID and 256-bit LAN key in owner-only storage.
    pub fn pair_client(&self, name: &str, client_id: &str, key: &[u8]) -> Result<()> {
        if client_id.trim().is_empty() {
            return Err(Error::usage("client_id must not be empty"));
        }
        if key.len() != 32 {
            return Err(Error::usage("client key must be 32 bytes"));
        }
        let _lock = StateLock::acquire(&self.state_dir)?;
        self.get(name)?;
        let encoded = serde_json::to_string(&StoredCredentials {
            client_id: client_id.trim().to_string(),
            key: auth::hex(key),
        })
        .map_err(|e| Error::new(ErrorKind::UnknownState, format!("cannot encode key: {e}")))?;
        let path = self.key_path(name);
        auth::write_key(&path, &encoded)
            .map_err(|e| io_error("cannot write machine key", &path, &e))
    }

    /// Remove a stored LAN key.
    pub fn unpair(&self, name: &str) -> Result<()> {
        let _lock = StateLock::acquire(&self.state_dir)?;
        self.get(name)?;
        let path = self.key_path(name);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                Err(Error::not_found(format!("machine {name:?} is not paired")))
            }
            Err(e) => Err(io_error("cannot remove machine key", &path, &e)),
        }
    }

    /// Read a machine's LAN credentials (per-client ID + key, or legacy key).
    pub fn credentials(&self, name: &str) -> Result<Option<ClientCredentials>> {
        self.get(name)?;
        let path = self.key_path(name);
        let value = match fs::read_to_string(&path) {
            Ok(value) => value,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io_error("cannot read machine key", &path, &e)),
        };
        let trimmed = value.trim();
        if trimmed.starts_with('{') {
            let parsed: StoredCredentials = serde_json::from_str(trimmed)
                .map_err(|_| Error::new(ErrorKind::UnknownState, "invalid machine credentials"))?;
            if parsed.client_id.trim().is_empty() {
                return Err(Error::new(
                    ErrorKind::UnknownState,
                    "invalid machine credentials: empty client_id",
                ));
            }
            let key = auth::decode(parsed.key.trim(), 32)
                .ok_or_else(|| Error::new(ErrorKind::UnknownState, "invalid machine key"))?;
            return Ok(Some(ClientCredentials {
                client_id: Some(parsed.client_id),
                key,
            }));
        }
        auth::decode(trimmed, 32)
            .map(|key| {
                Some(ClientCredentials {
                    client_id: None,
                    key,
                })
            })
            .ok_or_else(|| Error::new(ErrorKind::UnknownState, "invalid machine key"))
    }

    /// Read a machine's LAN key, if paired.
    pub fn key(&self, name: &str) -> Result<Option<Vec<u8>>> {
        Ok(self.credentials(name)?.map(|creds| creds.key))
    }

    /// Every registered machine, in name order.
    pub fn list(&self) -> Result<Vec<Machine>> {
        self.read()
    }

    /// Register a machine. A name (or non-empty machine ID) already in the
    /// registry is refused, and the record is checked again here so a machine
    /// built by hand cannot slip in.
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
        if let Some(id) = machine.id.as_deref() {
            if let Some(existing) = machines
                .iter()
                .find(|entry| entry.id.as_deref() == Some(id))
            {
                return Err(Error::refused(format!(
                    "machine id {id:?} is already registered as {:?}",
                    existing.name
                )));
            }
        }
        machines.push(machine);
        machines.sort_by(|a, b| a.name.cmp(&b.name));
        self.write(&machines)
    }

    /// Update an existing machine record in place under the state lock.
    pub fn update(
        &self,
        name: &str,
        edit: impl FnOnce(&mut Machine) -> Result<()>,
    ) -> Result<Machine> {
        let _lock = StateLock::acquire(&self.state_dir)?;
        let mut machines = self.read()?;
        let index = machines
            .iter()
            .position(|entry| entry.name == name)
            .ok_or_else(|| Error::not_found(format!("machine {name:?} is not registered")))?;
        let mut updated = machines[index].clone();
        edit(&mut updated)?;
        if let Some(fault) = fault(&updated) {
            return Err(Error::usage(format!(
                "machine {:?} is not valid: {fault}",
                updated.name
            )));
        }
        machines[index] = updated.clone();
        machines.sort_by(|a, b| a.name.cmp(&b.name));
        self.write(&machines)?;
        Ok(updated)
    }

    /// Remove a machine by name and return what was removed.
    pub fn remove(&self, name: &str) -> Result<Machine> {
        let _lock = StateLock::acquire(&self.state_dir)?;
        let mut machines = self.read()?;
        let index = machines
            .iter()
            .position(|entry| entry.name == name)
            .ok_or_else(|| Error::not_found(format!("machine {name:?} is not registered")))?;
        let key_path = self.key_path(name);
        match fs::remove_file(&key_path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(io_error("cannot remove machine key", &key_path, &error)),
        }
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

    /// Whether `machines.json` exists on disk yet.
    pub fn exists(&self) -> bool {
        self.path.exists()
    }

    fn key_path(&self, name: &str) -> PathBuf {
        self.state_dir
            .join("machine-keys")
            .join(format!("{name}.key"))
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
    if let Some(id) = &machine.id {
        if id.trim().is_empty() {
            return Some("machine id must not be empty");
        }
    }
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
    if let Some(mac) = &machine.mac {
        match parse_mac(mac) {
            Ok(bytes) if format_mac(bytes) == *mac => {}
            Ok(_) => return Some("MAC address is not canonical"),
            Err(_) => return Some("MAC address is invalid"),
        }
    }
    None
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

#[cfg(test)]
mod discovery_tests {
    use super::*;
    use barge::testing::TempDir;

    #[test]
    fn unpaired_machine_and_bad_key_are_reported() {
        let temp = TempDir::new("machine-pair").unwrap();
        let registry = Registry::open(temp.path()).unwrap();
        registry
            .add(Machine::new("pc", "localhost", "00:11:22:33:44:ff").unwrap())
            .unwrap();
        assert_eq!(
            registry.unpair("pc").unwrap_err().kind(),
            ErrorKind::NotFound
        );
        assert_eq!(
            registry.pair("pc", "bad").unwrap_err().kind(),
            ErrorKind::Usage
        );
        assert!(registry.key("pc").unwrap().is_none());
    }

    #[test]
    fn client_credentials_and_legacy_keys_round_trip() {
        let temp = TempDir::new("machine-creds").unwrap();
        let registry = Registry::open(temp.path()).unwrap();
        registry
            .add(Machine::discovered("mid-1", "pc", "192.168.1.20", 9777).unwrap())
            .unwrap();
        let key = [0xab; 32];
        registry.pair_client("pc", "cid-1", &key).unwrap();
        let creds = registry.credentials("pc").unwrap().unwrap();
        assert_eq!(creds.client_id.as_deref(), Some("cid-1"));
        assert_eq!(creds.key, key.to_vec());
        assert_eq!(registry.key("pc").unwrap(), Some(key.to_vec()));

        // Legacy key file without client_id still reads cleanly.
        registry.pair("pc", &"cd".repeat(32)).unwrap();
        let legacy = registry.credentials("pc").unwrap().unwrap();
        assert_eq!(legacy.client_id, None);
        assert_eq!(legacy.key, vec![0xcd; 32]);
    }

    #[test]
    fn storage_and_machine_discovery_share_state_and_lock() {
        const CHILD: &str = "BARGE_DISCOVERY_TEST_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let storage = barge::Barge::open_read_only().unwrap();
            let registry = Registry::discover().unwrap();
            assert_eq!(registry.state_dir, storage.state_dir());
            let lock = StateLock::acquire(&registry.state_dir).unwrap();
            let storage_lock = storage.state_dir().join("lock");
            assert!(storage_lock.exists());
            #[cfg(unix)]
            {
                use std::os::fd::AsRawFd;
                let file = fs::OpenOptions::new()
                    .write(true)
                    .open(storage_lock)
                    .unwrap();
                assert_eq!(
                    // SAFETY: file owns a valid descriptor for this lock probe.
                    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
                    -1
                );
            }
            drop(lock);
            return;
        }
        let temp = TempDir::new("machine-discovery").unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "registry::discovery_tests::storage_and_machine_discovery_share_state_and_lock",
            ])
            .env(CHILD, "1")
            .env_remove("BARGE_STATE_DIR")
            .env("HOME", temp.path())
            .env("XDG_STATE_HOME", temp.child("xdg"))
            .status()
            .unwrap();
        assert!(status.success());
    }
}
