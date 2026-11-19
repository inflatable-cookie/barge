//! Roots: a directory Barge created inside an area, its marker, and what a
//! listing reports about it.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::os::TreeSize;
use crate::state::publish;

/// The schema name of a root marker.
pub(crate) const ROOT_SCHEMA: &str = "barge.root/1";
/// The schema name of an adopted root marker with a recorded folder.
pub(crate) const ADOPTED_ROOT_SCHEMA: &str = "barge.root/2";

/// The marker file that makes a folder a Barge-owned root.
pub(crate) const MARKER_NAME: &str = ".barge-root.json";

const MAX_ID: usize = 64;
const MINT_ID_BYTES: usize = 8;
const MAX_LABEL: usize = 32;

/// A root's marker: the area it belongs to, its ID, its label and when it was
/// created. It is the only claim Barge accepts as ownership.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RootMarker {
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
    /// Root tree size, or zero when using `Area::list_without_bytes`.
    pub bytes: u64,
    /// True when the tree size could not be measured exactly: an entry could not
    /// be read, or the tree is deeper than the walk goes.
    pub bytes_approximate: bool,
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
    read_marker_opened(crate::os::open_marker(folder, MARKER_NAME), folder)
}

/// Read the marker through an already open folder, so the ownership check names
/// the folder the caller then acts on.
pub(crate) fn read_marker_in(directory: &File, folder: &Path) -> MarkerRead {
    read_marker_opened(crate::os::open_marker_at(directory, MARKER_NAME), folder)
}

fn read_marker_opened(opened: std::io::Result<File>, folder: &Path) -> MarkerRead {
    let path = folder.join(MARKER_NAME);
    let bytes = match opened.and_then(|mut file| {
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(bytes)
    }) {
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
    if crate::area::AreaKey::parse(&marker.area).is_err() || validate_label(&marker.label).is_err()
    {
        return MarkerRead::Invalid(format!("{} has an invalid area or label", path.display()));
    }
    if marker.schema == ROOT_SCHEMA && marker.folder.is_some() {
        return MarkerRead::Invalid(format!("{} has a folder in a v1 marker", path.display()));
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
pub(crate) fn derived_folder(label: &str, id: &str) -> String {
    format!("{label}-{id}")
}

/// Validate a label a consumer supplies for a root.
pub(crate) fn validate_label(label: &str) -> Result<()> {
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
pub(crate) fn valid_id(id: &str) -> bool {
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
pub(crate) fn validate_id(id: &str) -> Result<()> {
    if valid_id(id) {
        Ok(())
    } else {
        Err(Error::usage(format!(
            "root id {id:?} must be a 1-{MAX_ID} character plain token of letters, digits, '-' or '_'",
        )))
    }
}

/// Whether `folder` is one plain directory name.
pub(crate) fn valid_folder(folder: &str) -> bool {
    !folder.is_empty()
        && folder != "."
        && folder != ".."
        && !folder.contains(['/', '\\', '\0'])
        && !folder.chars().any(char::is_control)
}

/// The directory recorded by a marker, or the legacy derived name.
pub(crate) fn marker_folder(marker: &RootMarker) -> String {
    marker
        .folder
        .clone()
        .unwrap_or_else(|| derived_folder(&marker.label, &marker.id))
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
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in bytes {
        id.push(char::from(HEX[usize::from(byte >> 4)]));
        id.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    Ok(id)
}

/// The bytes a folder holds, without following symlinks or crossing into
/// another filesystem. Any part of the tree the walk could not see is reported
/// as an approximate size rather than silently counted as nothing.
pub(crate) fn tree_bytes(path: &Path) -> TreeSize {
    #[cfg(test)]
    TREE_BYTES_CALLS.with(|calls| calls.set(calls.get() + 1));
    crate::os::measure_tree(path)
}

#[cfg(test)]
thread_local! {
    static TREE_BYTES_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn reset_tree_bytes_calls() {
    TREE_BYTES_CALLS.with(|calls| calls.set(0));
}

#[cfg(test)]
pub(crate) fn tree_bytes_calls() -> usize {
    TREE_BYTES_CALLS.with(std::cell::Cell::get)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    /// The bytes a file is charged for, as a measured tree counts them.
    fn allocated(path: &Path) -> u64 {
        fs::symlink_metadata(path).unwrap().blocks() * 512
    }

    /// Restores a folder's mode so the fixture can still remove it.
    struct ModeGuard(PathBuf);

    impl Drop for ModeGuard {
        fn drop(&mut self) {
            let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
        }
    }

    fn folder(label: &str) -> (crate::testing::TempDir, PathBuf) {
        let temp = crate::testing::TempDir::new(label).unwrap();
        let root = temp.path().join("slot-0123456789abcdef");
        fs::create_dir(&root).unwrap();
        (temp, root)
    }

    #[test]
    fn validators_check_root_components() {
        for value in ["", "-a", "a-", "A", "a/b"] {
            assert!(validate_label(value).is_err(), "{value:?}");
        }
        assert!(validate_label("a-1").is_ok());
        for value in ["", "-a", "a/b", "a.b"] {
            assert!(!valid_id(value), "{value:?}");
        }
        assert!(valid_id("A_1-2"));
        for value in ["", ".", "..", "a/b", "a\\b", "a\n"] {
            assert!(!valid_folder(value), "{value:?}");
        }
        assert!(valid_folder("a folder"));
    }

    #[test]
    fn invalid_marker_fields_are_rejected() {
        let (_tmp, path) = folder("marker-validation");
        let marker = RootMarker {
            schema: ROOT_SCHEMA.to_string(),
            area: "test/area".into(),
            id: "abc".into(),
            label: "root".into(),
            created_unix_ms: 0,
            expires_unix_ms: None,
            folder: None,
        };
        for field in 0..4 {
            let mut invalid = marker.clone();
            match field {
                0 => invalid.area = "bad".into(),
                1 => invalid.label = "Bad".into(),
                2 => invalid.folder = Some("folder".into()),
                _ => invalid.schema = "barge.root/99".into(),
            }
            write_marker(&path, &invalid).unwrap();
            assert!(matches!(read_marker(&path), MarkerRead::Invalid(_)));
        }
        let mut adopted = marker;
        adopted.schema = ADOPTED_ROOT_SCHEMA.into();
        adopted.folder = Some("folder".into());
        write_marker(&path, &adopted).unwrap();
        assert!(matches!(read_marker(&path), MarkerRead::Valid(_)));
    }

    #[test]
    fn an_unreadable_entry_makes_the_size_approximate() {
        if crate::os::running_as_root() {
            return; // Root reads a 0o000 folder, so the mode denies nothing.
        }
        let (_temp, root) = folder("root-unreadable");
        fs::write(root.join("readable.bin"), vec![b'x'; 6]).unwrap();
        let hidden = root.join("hidden");
        fs::create_dir(&hidden).unwrap();
        fs::write(hidden.join("unread.bin"), vec![b'x'; 16]).unwrap();
        fs::set_permissions(&hidden, fs::Permissions::from_mode(0o000)).unwrap();
        let _guard = ModeGuard(hidden);

        let size = tree_bytes(&root);
        assert_eq!(
            size.bytes,
            allocated(&root.join("readable.bin")),
            "the readable file is still counted"
        );
        assert!(size.approximate, "an unreadable entry is never zero");
    }
}
