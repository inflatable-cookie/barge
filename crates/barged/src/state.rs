//! Daemon persistent state: stable machine ID, per-client LAN keys, and
//! one-time migration of a legacy shared `lan.key` file.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use barge_machine::{auth, PairedClient};
use serde::{Deserialize, Serialize};

use crate::{Error, Result};

const CLIENTS_SCHEMA: &str = "barge.daemon-clients/1";
pub const LEGACY_CLIENT_ID: &str = "legacy";

/// Stored record for one paired LAN client, including its 256-bit HMAC key.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientRecord {
    pub id: String,
    pub name: String,
    pub key: String,
    pub paired_at_unix_ms: u64,
}

impl fmt::Debug for ClientRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientRecord")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("paired_at_unix_ms", &self.paired_at_unix_ms)
            .finish_non_exhaustive()
    }
}

impl ClientRecord {
    /// Public view of this client (never carries the secret key).
    pub fn summary(&self) -> PairedClient {
        PairedClient {
            id: self.id.clone(),
            name: self.name.clone(),
            paired_at_unix_ms: self.paired_at_unix_ms,
        }
    }

    /// Decode the 32-byte HMAC key.
    pub fn key_bytes(&self) -> Option<Vec<u8>> {
        auth::decode(&self.key, 32)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClientsDocument {
    schema: String,
    clients: Vec<ClientRecord>,
}

/// Persistent daemon state under `state_dir`.
#[derive(Debug, Clone)]
pub struct DaemonState {
    dir: PathBuf,
    machine_id: String,
}

impl DaemonState {
    /// Open or initialize `state_dir`, creating `machine-id` on first start and
    /// migrating `legacy_key_path` into `clients.json` if upgrading.
    pub fn open(
        dir: impl Into<PathBuf>,
        legacy_key_path: Option<&Path>,
        now_unix_ms: u64,
    ) -> Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir).map_err(|e| {
            Error::new(format!(
                "cannot create state directory {}: {e}",
                dir.display()
            ))
        })?;
        let _lock = StateLock::acquire(&dir)?;
        let machine_id = ensure_machine_id(&dir.join("machine-id"))?;
        let clients_path = dir.join("clients.json");
        if !clients_path.exists() {
            if let Some(legacy_path) = legacy_key_path.filter(|p| p.exists()) {
                let raw = fs::read_to_string(legacy_path).map_err(|e| {
                    Error::new(format!(
                        "cannot read legacy LAN key {}: {e}",
                        legacy_path.display()
                    ))
                })?;
                let trimmed = raw.trim();
                if auth::decode(trimmed, 32).is_none() {
                    return Err(Error::new(format!(
                        "legacy LAN key {} is not 64 lowercase hex characters",
                        legacy_path.display()
                    )));
                }
                write_clients(
                    &clients_path,
                    &[ClientRecord {
                        id: LEGACY_CLIENT_ID.to_string(),
                        name: LEGACY_CLIENT_ID.to_string(),
                        key: trimmed.to_string(),
                        paired_at_unix_ms: now_unix_ms,
                    }],
                )?;
            }
        }
        Ok(Self { dir, machine_id })
    }

    pub fn machine_id(&self) -> &str {
        &self.machine_id
    }

    pub fn list_clients(&self) -> Result<Vec<ClientRecord>> {
        let _lock = StateLock::acquire(&self.dir)?;
        read_clients(&self.dir.join("clients.json"))
    }

    pub fn find_client(&self, id: &str) -> Result<Option<ClientRecord>> {
        let clients = self.list_clients()?;
        Ok(clients.into_iter().find(|client| client.id == id))
    }

    pub fn add_client(
        &self,
        name: &str,
        key: &[u8; 32],
        paired_at_unix_ms: u64,
    ) -> Result<ClientRecord> {
        let _lock = StateLock::acquire(&self.dir)?;
        let path = self.dir.join("clients.json");
        let mut clients = read_clients(&path)?;
        let id = loop {
            let candidate = auth::hex(
                &auth::random(8)
                    .map_err(|e| Error::new(format!("cannot generate client id: {e}")))?,
            );
            if !clients.iter().any(|c| c.id == candidate) {
                break candidate;
            }
        };
        let record = ClientRecord {
            id,
            name: name.to_string(),
            key: auth::hex(key),
            paired_at_unix_ms,
        };
        clients.push(record.clone());
        write_clients(&path, &clients)?;
        Ok(record)
    }

    pub fn remove_client(&self, id: &str) -> Result<bool> {
        let _lock = StateLock::acquire(&self.dir)?;
        let path = self.dir.join("clients.json");
        let mut clients = read_clients(&path)?;
        let Some(index) = clients.iter().position(|c| c.id == id) else {
            return Ok(false);
        };
        clients.remove(index);
        write_clients(&path, &clients)?;
        Ok(true)
    }
}

fn ensure_machine_id(path: &Path) -> Result<String> {
    match fs::read_to_string(path) {
        Ok(existing) => {
            let trimmed = existing.trim();
            if trimmed.is_empty()
                || trimmed.len() > 64
                || !trimmed
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            {
                return Err(Error::new(format!(
                    "corrupt machine ID in {}",
                    path.display()
                )));
            }
            Ok(trimmed.to_string())
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let id = auth::hex(
                &auth::random(16)
                    .map_err(|err| Error::new(format!("cannot generate machine ID: {err}")))?,
            );
            write_atomic(path, format!("{id}\n").as_bytes(), 0o644)?;
            Ok(id)
        }
        Err(e) => Err(Error::new(format!(
            "cannot read machine ID {}: {e}",
            path.display()
        ))),
    }
}

fn read_clients(path: &Path) -> Result<Vec<ClientRecord>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(Error::new(format!(
                "cannot read clients {}: {e}",
                path.display()
            )))
        }
    };
    let doc: ClientsDocument = serde_json::from_slice(&bytes)
        .map_err(|e| Error::new(format!("cannot parse clients {}: {e}", path.display())))?;
    if doc.schema != CLIENTS_SCHEMA {
        return Err(Error::new(format!(
            "clients {} has schema {}, not {CLIENTS_SCHEMA}",
            path.display(),
            doc.schema
        )));
    }
    for client in &doc.clients {
        if client.id.trim().is_empty() || client.key_bytes().is_none() {
            return Err(Error::new(format!(
                "clients {} contains an invalid entry {:?}",
                path.display(),
                client.id
            )));
        }
    }
    Ok(doc.clients)
}

fn write_clients(path: &Path, clients: &[ClientRecord]) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(&ClientsDocument {
        schema: CLIENTS_SCHEMA.to_string(),
        clients: clients.to_vec(),
    })
    .map_err(|e| Error::new(format!("cannot encode clients: {e}")))?;
    write_atomic(path, &bytes, 0o600)
}

fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::new("state path has no parent"))?;
    let suffix = auth::hex(
        &auth::random(8).map_err(|e| Error::new(format!("cannot generate temp name: {e}")))?,
    );
    let temp = parent.join(format!(".tmp-{suffix}"));
    let result = (|| -> io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temp, path)?;
        if let Ok(dir) = File::open(parent) {
            let _ = dir.sync_all();
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result.map_err(|e| Error::new(format!("cannot write {}: {e}", path.display())))
}

struct StateLock {
    _file: File,
}

impl StateLock {
    fn acquire(dir: &Path) -> Result<Self> {
        let path = dir.join("state.lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| Error::new(format!("cannot open state lock {}: {e}", path.display())))?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            // SAFETY: `file` owns a valid descriptor for this process lock.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == -1 {
                return Err(Error::new(format!(
                    "cannot lock {}: {}",
                    path.display(),
                    io::Error::last_os_error()
                )));
            }
        }
        Ok(Self { _file: file })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn machine_id_is_created_once_and_persists_across_restarts() {
        let temp = barge::testing::TempDir::new("barged-state-id").unwrap();
        let first = DaemonState::open(temp.path(), None, 100).unwrap();
        let second = DaemonState::open(temp.path(), None, 200).unwrap();
        assert_eq!(first.machine_id(), second.machine_id());
        assert_eq!(first.machine_id().len(), 32);
    }

    #[test]
    fn legacy_lan_key_migrates_once_and_revoke_persists() {
        let temp = barge::testing::TempDir::new("barged-legacy-migrate").unwrap();
        let legacy_path = temp.child("lan.key");
        let legacy_key = "ab".repeat(32);
        auth::write_key(&legacy_path, &legacy_key).unwrap();

        let state = DaemonState::open(temp.path(), Some(&legacy_path), 500).unwrap();
        let clients = state.list_clients().unwrap();
        assert_eq!(clients.len(), 1);
        assert_eq!(clients[0].id, "legacy");
        assert_eq!(clients[0].name, "legacy");
        assert_eq!(clients[0].key, legacy_key);
        assert_eq!(clients[0].paired_at_unix_ms, 500);
        assert_eq!(
            fs::metadata(temp.child("clients.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );

        // Revoking legacy stays revoked across restart even if lan.key remains on disk.
        assert!(state.remove_client("legacy").unwrap());
        let reopened = DaemonState::open(temp.path(), Some(&legacy_path), 600).unwrap();
        assert!(reopened.list_clients().unwrap().is_empty());
    }
}
