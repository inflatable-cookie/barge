//! Machine records and account registry.
use crate::state::{io_error, publish, StateLock};
use crate::wol::{format_mac, parse_mac};
use crate::{
    auth, Error, ErrorKind, Result, DEFAULT_BROADCAST, DEFAULT_DAEMON_PORT, DEFAULT_PORT,
    REGISTRY_SCHEMA,
};
use serde::{Deserialize, Serialize};
use std::{fs, io, net::Ipv4Addr, path::PathBuf};

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

    /// Store a LAN key separately from the public registry.
    pub fn pair(&self, name: &str, key: &str) -> Result<()> {
        let _lock = StateLock::acquire(&self.state_dir)?;
        self.get(name)?;
        if auth::decode(key.trim(), 32).is_none() {
            return Err(Error::usage(
                "key must be 64 lowercase hexadecimal characters",
            ));
        }
        let path = self
            .state_dir
            .join("machine-keys")
            .join(format!("{name}.key"));
        auth::write_key(&path, key.trim())
            .map_err(|e| io_error("cannot write machine key", &path, &e))
    }

    /// Remove a stored LAN key.
    pub fn unpair(&self, name: &str) -> Result<()> {
        let _lock = StateLock::acquire(&self.state_dir)?;
        self.get(name)?;
        let path = self
            .state_dir
            .join("machine-keys")
            .join(format!("{name}.key"));
        fs::remove_file(&path).map_err(|e| io_error("cannot remove machine key", &path, &e))
    }

    /// Read a machine's LAN key, if paired.
    pub fn key(&self, name: &str) -> Result<Option<Vec<u8>>> {
        self.get(name)?;
        let path = self
            .state_dir
            .join("machine-keys")
            .join(format!("{name}.key"));
        match fs::read_to_string(&path) {
            Ok(value) => auth::decode(value.trim(), 32)
                .map(Some)
                .ok_or_else(|| Error::new(ErrorKind::UnknownState, "invalid machine key")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(io_error("cannot read machine key", &path, &e)),
        }
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
        let key_path = self
            .state_dir
            .join("machine-keys")
            .join(format!("{name}.key"));
        match fs::remove_file(&key_path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
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

#[cfg(test)]
mod discovery_tests {
    use super::*;
    use barge::testing::TempDir;

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
                let file = std::fs::OpenOptions::new()
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
