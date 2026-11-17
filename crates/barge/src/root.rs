//! Roots: a directory Barge created inside an area, its marker, and what a
//! listing reports about it.

use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::state::publish;

/// The schema name of a root marker.
pub const ROOT_SCHEMA: &str = "barge.root/1";
/// The schema name of an adopted root marker with a recorded folder.
pub const ADOPTED_ROOT_SCHEMA: &str = "barge.root/2";

/// The marker file that makes a folder a Barge-owned root.
pub const MARKER_NAME: &str = ".barge-root.json";

const MAX_ID: usize = 64;
const MINT_ID_BYTES: usize = 8;
const MAX_LABEL: usize = 32;

/// A root's marker: the area it belongs to, its ID, its label and when it was
/// created. It is the only claim Barge accepts as ownership.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RootMarker {
    pub schema: String,
    /// `<namespace>/<area>`.
    pub area: String,
    pub id: String,
    pub label: String,
    pub created_unix_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
}

/// A root Barge created.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Root {
    pub id: String,
    pub label: String,
    pub path: PathBuf,
    /// The root's actual directory name, which can differ from its label and ID.
    pub folder: String,
    pub created_unix_ms: u64,
    pub expires_unix_ms: Option<u64>,
}

impl Root {
    /// The root's actual folder name.
    pub fn folder_name(&self) -> String {
        self.folder.clone()
    }

    /// Whether this root's expiry has passed at `now_unix_ms`.
    pub fn is_expired(&self, now_unix_ms: u64) -> bool {
        self.expires_unix_ms
            .is_some_and(|expires| expires <= now_unix_ms)
    }
}

/// One root in an area listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RootInfo {
    pub id: String,
    pub label: String,
    pub path: PathBuf,
    pub created_unix_ms: u64,
    pub last_used_unix_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_unix_ms: Option<u64>,
    pub held: bool,
    pub pinned: bool,
    pub bytes: u64,
    /// True when the root lives on the volume; false in the area's fallback.
    pub on_volume: bool,
}

impl RootInfo {
    /// True when nothing holds the root and no pin protects it.
    pub fn idle_and_unpinned(&self) -> bool {
        !self.held && !self.pinned
    }
}

/// A folder Barge will not delete: no marker, an unparsable marker, or a marker
/// that names another area.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Straggler {
    pub path: PathBuf,
    pub reason: String,
}

/// What an area contains.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Listing {
    pub roots: Vec<RootInfo>,
    pub stragglers: Vec<Straggler>,
}

impl Listing {
    pub fn bytes(&self) -> u64 {
        self.roots.iter().map(|root| root.bytes).sum()
    }

    pub fn get(&self, id: &str) -> Option<&RootInfo> {
        self.roots.iter().find(|root| root.id == id)
    }
}

/// The result of reading a marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MarkerRead {
    Missing,
    Invalid(String),
    Valid(RootMarker),
}

/// Read the marker in `folder`.
pub(crate) fn read_marker(folder: &Path) -> MarkerRead {
    let path = folder.join(MARKER_NAME);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return MarkerRead::Missing,
        Err(source) => {
            return MarkerRead::Invalid(format!("cannot read {}: {source}", path.display()))
        }
    };
    let marker: RootMarker = match serde_json::from_slice(&bytes) {
        Ok(marker) => marker,
        Err(source) => {
            return MarkerRead::Invalid(format!("cannot parse {}: {source}", path.display()))
        }
    };
    if marker.schema != ROOT_SCHEMA && marker.schema != ADOPTED_ROOT_SCHEMA {
        return MarkerRead::Invalid(format!(
            "{} has unsupported root schema {}",
            path.display(),
            marker.schema
        ));
    }
    if !valid_id(&marker.id) {
        return MarkerRead::Invalid(format!(
            "{} has an invalid root id {:?}",
            path.display(),
            marker.id
        ));
    }
    if marker.schema == ADOPTED_ROOT_SCHEMA
        && marker
            .folder
            .as_deref()
            .is_none_or(|folder| !valid_folder(folder))
    {
        return MarkerRead::Invalid(format!(
            "{} has an invalid recorded root folder",
            path.display()
        ));
    }
    MarkerRead::Valid(marker)
}

/// Publish a marker into a root folder by atomic rename.
pub(crate) fn write_marker(folder: &Path, marker: &RootMarker) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(marker)
        .map_err(|source| Error::unknown_state(format!("cannot encode a root marker: {source}")))?;
    publish(&folder.join(MARKER_NAME), &bytes)
}

/// The folder name of an owned root.
pub fn folder_name(label: &str, id: &str) -> String {
    format!("{label}-{id}")
}

/// Validate a label a consumer supplies for a root.
pub fn validate_label(label: &str) -> Result<()> {
    let mut bytes = label.bytes();
    let first = bytes.next();
    let valid_first = first.is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit());
    let valid_rest =
        bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
    if !valid_first || !valid_rest || label.len() > MAX_LABEL || label.ends_with('-') {
        return Err(Error::usage(format!(
            "root label {label:?} must be 1-{MAX_LABEL} characters of lowercase letters, digits or '-', not ending in '-'"
        )));
    }
    Ok(())
}

/// Validate a root ID.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        && id
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
}

/// Validate a root ID, naming the field on failure.
pub fn validate_id(id: &str) -> Result<()> {
    if valid_id(id) {
        Ok(())
    } else {
        Err(Error::usage(format!(
            "root id {id:?} must be a 1-{MAX_ID} character plain token of letters, digits, '-' or '_'",
        )))
    }
}

/// Whether `folder` is one plain directory name.
pub fn valid_folder(folder: &str) -> bool {
    !folder.is_empty()
        && folder != "."
        && folder != ".."
        && !folder.contains(['/', '\\', '\0'])
        && !folder.chars().any(char::is_control)
}

/// The directory recorded by a marker, or the legacy derived name.
pub fn marker_folder(marker: &RootMarker) -> String {
    marker
        .folder
        .clone()
        .unwrap_or_else(|| folder_name(&marker.label, &marker.id))
}

/// Mint a new root ID from the operating system's random source.
pub(crate) fn mint_id() -> Result<String> {
    let mut bytes = [0_u8; MINT_ID_BYTES];
    let mut source = File::open("/dev/urandom").map_err(|source| {
        Error::io(
            "cannot open the random source",
            Path::new("/dev/urandom"),
            &source,
        )
    })?;
    source.read_exact(&mut bytes).map_err(|source| {
        Error::io(
            "cannot read the random source",
            Path::new("/dev/urandom"),
            &source,
        )
    })?;
    let mut id = String::with_capacity(MINT_ID_BYTES * 2);
    for byte in bytes {
        id.push_str(&format!("{byte:02x}"));
    }
    Ok(id)
}

/// The bytes a folder holds, without following symlinks or crossing into
/// another filesystem.
pub(crate) fn tree_bytes(path: &Path) -> u64 {
    let mut total = 0_u64;
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return 0;
    };
    if !metadata.is_dir() {
        return metadata.len();
    }
    let Ok(entries) = fs::read_dir(path) else {
        return 0;
    };
    for entry in entries.flatten() {
        let child = entry.path();
        let Ok(child_metadata) = fs::symlink_metadata(&child) else {
            continue;
        };
        if child_metadata.file_type().is_symlink() {
            continue;
        }
        if child_metadata.is_dir() {
            total = total.saturating_add(tree_bytes(&child));
        } else {
            total = total.saturating_add(child_metadata.len());
        }
    }
    total
}
