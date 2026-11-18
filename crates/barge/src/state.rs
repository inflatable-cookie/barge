//! The account state directory: `volumes.json`, `areas/<namespace>.<area>.json`,
//! one state lock and atomic publication.
//!
//! A read that fails validation is an error, never an empty state: unknown is
//! never safe. A missing `volumes.json` or `areas/` is simply an account that
//! has not registered anything yet.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::area::{AreaKey, AreaRecord, AREA_SCHEMA};
use crate::error::{Error, Result};
use crate::hold::Mode;
use crate::volume::{VolumeSpec, VOLUMES_SCHEMA};

/// The state lock's file name inside the account state directory.
const LOCK_NAME: &str = "lock";
/// The file that lists registered volumes.
const VOLUMES_NAME: &str = "volumes.json";
/// The folder holding one file per declared area.
const AREAS_DIR: &str = "areas";

static PUBLISH_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A directory that holds Barge's account state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StateDir {
    root: PathBuf,
}

impl StateDir {
    /// The state directory named by `BARGE_STATE_DIR`, or the platform default.
    pub fn discover() -> Result<PathBuf> {
        if let Some(explicit) = std::env::var_os("BARGE_STATE_DIR") {
            if !explicit.is_empty() {
                return Ok(PathBuf::from(explicit));
            }
        }
        #[cfg(target_os = "linux")]
        if let Some(state_home) = std::env::var_os("XDG_STATE_HOME") {
            if !state_home.is_empty() {
                return Ok(PathBuf::from(state_home).join("barge"));
            }
        }
        let home = std::env::var_os("HOME")
            .filter(|home| !home.is_empty())
            .map(PathBuf::from)
            .ok_or_else(|| {
                Error::unknown_state(
                    "cannot locate the account state directory: set BARGE_STATE_DIR",
                )
            })?;
        if cfg!(target_os = "macos") {
            Ok(home.join("Library/Application Support/barge"))
        } else {
            Ok(home.join(".local/state/barge"))
        }
    }

    /// Open (creating when needed) an account state directory.
    pub fn open(root: PathBuf) -> Result<Self> {
        if !root.is_absolute() {
            return Err(Error::usage(format!(
                "state directory {} is not absolute",
                root.display()
            )));
        }
        let state = Self { root };
        fs::create_dir_all(state.areas_dir()).map_err(|source| {
            Error::io(
                "cannot create state directory",
                state.areas_dir().as_path(),
                &source,
            )
        })?;
        Ok(state)
    }

    /// Open an account path for reads without creating it or any child.
    pub fn open_read_only(root: PathBuf) -> Result<Self> {
        if !root.is_absolute() {
            return Err(Error::usage(format!(
                "state directory {} is not absolute",
                root.display()
            )));
        }
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn volumes_path(&self) -> PathBuf {
        self.root.join(VOLUMES_NAME)
    }

    fn areas_dir(&self) -> PathBuf {
        self.root.join(AREAS_DIR)
    }

    fn area_path(&self, key: &AreaKey) -> PathBuf {
        self.areas_dir().join(format!("{}.json", key.file_stem()))
    }

    /// Take the account state lock, waiting for another writer.
    pub fn lock(&self) -> Result<StateLock> {
        StateLock::acquire(&self.root)
    }

    /// Read `volumes.json`. A missing file is an empty account.
    pub fn read_volumes(&self) -> Result<Vec<VolumeSpec>> {
        let path = self.volumes_path();
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => return Err(Error::io("cannot read", &path, &source)),
        };
        let document: VolumesDocument = serde_json::from_slice(&bytes).map_err(|source| {
            Error::unknown_state(format!("cannot parse {}: {source}", path.display()))
        })?;
        if document.schema != VOLUMES_SCHEMA {
            return Err(Error::unknown_state(format!(
                "{} has schema {}, not {VOLUMES_SCHEMA}",
                path.display(),
                document.schema
            )));
        }
        for spec in &document.volumes {
            spec.validate()
                .map_err(|e| Error::unknown_state(format!("invalid {}: {e}", path.display())))?;
        }
        Ok(document.volumes)
    }

    /// Publish `volumes.json` by atomic rename.
    pub fn write_volumes(&self, volumes: &[VolumeSpec]) -> Result<()> {
        let document = VolumesDocument {
            schema: VOLUMES_SCHEMA.to_string(),
            volumes: volumes.to_vec(),
        };
        let bytes = serde_json::to_vec_pretty(&document)
            .map_err(|source| Error::unknown_state(format!("cannot encode volumes: {source}")))?;
        publish(&self.volumes_path(), &bytes)
    }

    /// Read one area declaration, `None` when it was never declared.
    pub fn read_area(&self, key: &AreaKey) -> Result<Option<AreaRecord>> {
        let path = self.area_path(key);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(Error::io("cannot read", &path, &source)),
        };
        let record: AreaRecord = serde_json::from_slice(&bytes).map_err(|source| {
            Error::unknown_state(format!("cannot parse {}: {source}", path.display()))
        })?;
        record
            .validate()
            .map_err(|e| Error::unknown_state(format!("invalid {}: {e}", path.display())))?;
        if record.key() != *key {
            return Err(Error::unknown_state(format!(
                "{} declares area {}, not {}",
                path.display(),
                record.key().label(),
                key.label()
            )));
        }
        Ok(Some(record))
    }

    /// Read every area declaration in the account.
    pub fn read_areas(&self) -> Result<Vec<AreaRecord>> {
        let dir = self.areas_dir();
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => return Err(Error::io("cannot list", &dir, &source)),
        };
        let mut records = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| Error::io("cannot list", &dir, &source))?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let bytes =
                fs::read(&path).map_err(|source| Error::io("cannot read", &path, &source))?;
            let record: AreaRecord = serde_json::from_slice(&bytes).map_err(|source| {
                Error::unknown_state(format!("cannot parse {}: {source}", path.display()))
            })?;
            record
                .validate()
                .map_err(|e| Error::unknown_state(format!("invalid {}: {e}", path.display())))?;
            if path != self.area_path(&record.key()) {
                return Err(Error::unknown_state(format!(
                    "{} declares area {}, but its file name differs",
                    path.display(),
                    record.key().label()
                )));
            }
            records.push(record);
        }
        Ok(records)
    }

    /// Publish one area declaration by atomic rename.
    pub fn write_area(&self, record: &AreaRecord) -> Result<()> {
        record.validate()?;
        if record.schema != AREA_SCHEMA {
            return Err(Error::unknown_state(format!(
                "refusing to publish area schema {}, not {AREA_SCHEMA}",
                record.schema
            )));
        }
        let bytes = serde_json::to_vec_pretty(record)
            .map_err(|source| Error::unknown_state(format!("cannot encode area: {source}")))?;
        publish(&self.area_path(&record.key()), &bytes)
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct VolumesDocument {
    schema: String,
    volumes: Vec<VolumeSpec>,
}

/// The held account state lock. Dropping it releases the lock.
#[derive(Debug)]
pub(crate) struct StateLock {
    /// Held only for the lock its descriptor carries; dropping it releases the
    /// lock, so the value is never read.
    _file: File,
}

impl StateLock {
    fn acquire(root: &Path) -> Result<Self> {
        let path = root.join(LOCK_NAME);
        let file = crate::os::open_lock(&path, "cannot open the state lock")?;
        crate::os::flock(&file, Mode::Exclusive, true)?;
        Ok(Self { _file: file })
    }
}

/// Write `bytes` to `path` through a temporary file in the same directory and an
/// atomic rename, so a reader sees the old document or the new one, never half.
pub(crate) fn publish(path: &Path, bytes: &[u8]) -> Result<()> {
    let directory = path.parent().ok_or_else(|| {
        Error::unknown_state(format!("{} has no parent directory", path.display()))
    })?;
    fs::create_dir_all(directory)
        .map_err(|source| Error::io("cannot create", directory, &source))?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("document");
    let temporary = directory.join(format!(".{name}.{}.tmp", unique_suffix()));
    let write = (|| -> std::io::Result<()> {
        let mut file = File::create(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()
    })();
    if let Err(source) = write {
        let _ = fs::remove_file(&temporary);
        return Err(Error::io("cannot write", &temporary, &source));
    }
    if let Err(source) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(Error::io("cannot publish", path, &source));
    }
    // Best effort: make the rename durable without making it a failure.
    if let Ok(directory) = File::open(directory) {
        let _ = directory.sync_all();
    }
    Ok(())
}

fn unique_suffix() -> String {
    let counter = PUBLISH_COUNTER.fetch_add(1, Ordering::SeqCst);
    format!("{}.{counter}", std::process::id())
}

/// Validate an account identifier: a lowercase name safe in a file name.
pub fn validate_name(field: &str, value: &str) -> Result<()> {
    let mut bytes = value.bytes();
    let first = bytes.next();
    let valid_first = first.is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit());
    let valid_rest = bytes.all(|byte| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_' | b'.')
    });
    if !valid_first || !valid_rest || value.len() > 64 || value == "." || value == ".." {
        return Err(Error::usage(format!(
            "{field} {value:?} must be 1-64 characters of lowercase letters, digits, '-', '_' or '.'"
        )));
    }
    Ok(())
}

/// Validate a volume's display name: letters, digits, '-', '_' and '.'.
///
/// A volume name is a label, not a folder: it may carry capitals ("Builds") and
/// never reaches a path.
pub fn validate_volume_name(field: &str, value: &str) -> Result<()> {
    let valid = !value.is_empty()
        && value.len() <= 64
        && !value.starts_with('.')
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
    if !valid {
        return Err(Error::usage(format!(
            "{field} {value:?} must be 1-64 letters, digits, '-', '_' or '.'"
        )));
    }
    Ok(())
}

/// Validate a relative folder path: slash-separated, no `..`, no absolute path.
pub fn validate_relative_path(field: &str, value: &str) -> Result<()> {
    if value.is_empty() {
        return Ok(());
    }
    for component in value.split('/') {
        if component.is_empty()
            || component == "."
            || component == ".."
            || component.contains('\0')
            || component.contains('\\')
        {
            return Err(Error::usage(format!(
                "{field} {value:?} must be a relative folder path without '.', '..' or empty components"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorKind;

    #[test]
    fn validators_reject_unsafe_names_and_paths() {
        for name in ["", ".", "..", "Upper", "a/b", "-a"] {
            assert!(validate_name("name", name).is_err(), "{name:?}");
        }
        for name in ["a", "a-b_c.d", "0"] {
            assert!(validate_name("name", name).is_ok(), "{name:?}");
        }
        for path in ["/a", "a/../b", "a//b", "a/./b", "a\\b"] {
            assert!(validate_relative_path("path", path).is_err(), "{path:?}");
        }
        assert!(validate_relative_path("path", "a/b").is_ok());
    }

    #[test]
    fn area_filename_and_invalid_state_are_rejected() {
        let tmp = crate::testing::TempDir::new("state-read").unwrap();
        let state = StateDir::open(tmp.path().to_path_buf()).unwrap();
        let key = AreaKey::new("test", "area").unwrap();
        let record = crate::area::AreaSpec::new("test", "area", "Builds").into_record();
        state.write_area(&record).unwrap();
        fs::rename(state.area_path(&key), state.areas_dir().join("wrong.json")).unwrap();
        assert_eq!(
            state.read_areas().unwrap_err().kind(),
            ErrorKind::UnknownState
        );
        fs::write(state.volumes_path(), br#"{"schema":"future","volumes":[]}"#).unwrap();
        assert_eq!(
            state.read_volumes().unwrap_err().kind(),
            ErrorKind::UnknownState
        );
    }

    #[test]
    fn failed_publish_removes_temporary_file() {
        let tmp = crate::testing::TempDir::new("publish").unwrap();
        let destination = tmp.folder("destination").unwrap();
        assert!(publish(&destination, b"data").is_err());
        assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 1);
    }
}
