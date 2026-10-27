//! Durable owned-root identity, independent of a mount's device number.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::area::{folder_components, MarkerArea, RootKind};
use crate::error::{Error, ErrorKind, Result};
use crate::os::{DirHandle, DirId, DirOps, EntryKind, RenameOutcome, VolumeUuid};
use crate::state::{validate_identifier, LockMode, LockRank, StateLock, StatePaths};

/// The provenance of a durable root identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdentityOrigin {
    /// Recorded when Barge created the root.
    Created,
    /// Recorded when Barge adopted an existing root.
    AdoptedExisting,
    /// Explicitly identified after its marker already existed.
    Identified,
}

/// The immutable account-state record for one owned root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RootIdentity {
    pub schema: String,
    pub root_id: String,
    pub nonce: String,
    /// Canonical UUID text; kept as text so noncanonical input is refused.
    pub volume_uuid: String,
    pub mount_relative_path: String,
    pub inode: u64,
    pub device_at_record: u64,
    pub mount_at_record: PathBuf,
    pub kind: RootKind,
    pub area: MarkerArea,
    pub origin: IdentityOrigin,
    pub marker_generation_at_record: u64,
    pub recorded_ms: u64,
}

impl RootIdentity {
    /// Validate the complete record before using it as authority.
    pub fn validate(&self) -> Result<()> {
        let invalid = || Error::new(ErrorKind::UnknownState, "invalid root identity record");
        if self.schema != "barge.root.identity/1" {
            return Err(invalid());
        }
        validate_identifier("root_id", &self.root_id)?;
        validate_identifier("nonce", &self.nonce)?;
        let uuid = VolumeUuid::parse(&self.volume_uuid)?;
        if uuid.as_str() != self.volume_uuid
            || self.root_id.bytes().any(|byte| byte.is_ascii_uppercase())
            || self.nonce.bytes().any(|byte| byte.is_ascii_uppercase())
        {
            return Err(invalid());
        }
        folder_components("mount_relative_path", &self.mount_relative_path)?;
        if self.inode == 0
            || self.device_at_record == 0
            || !self.mount_at_record.is_absolute()
            || !matches!(
                (self.kind, self.marker_generation_at_record),
                (_, 1) | (RootKind::ScratchRun, 2)
            )
            || self.area.effective.is_some()
        {
            return Err(invalid());
        }
        crate::area::validate_name("area.namespace", &self.area.namespace)?;
        crate::area::validate_name("area.area", &self.area.area)?;
        Ok(())
    }
}

/// Construct a record from a root just written through an opened descriptor.
pub fn from_owned(
    root: &crate::area::OwnedRoot,
    mount: &std::path::Path,
    origin: IdentityOrigin,
) -> Result<RootIdentity> {
    let relative = root
        .path
        .strip_prefix(mount)
        .map_err(|_| Error::new(ErrorKind::UnknownState, "root is outside its mount"))?
        .to_str()
        .ok_or_else(|| Error::new(ErrorKind::UnknownState, "root path is not UTF-8"))?;
    let record = RootIdentity {
        schema: "barge.root.identity/1".to_owned(),
        root_id: root.root_id.clone(),
        nonce: root.nonce.clone(),
        volume_uuid: root.volume_uuid.as_str().to_owned(),
        mount_relative_path: relative.to_owned(),
        inode: root.identity.inode,
        device_at_record: root.identity.device.get(),
        mount_at_record: mount.to_path_buf(),
        kind: root.kind,
        area: MarkerArea::identity(&root.area.namespace, &root.area.area),
        origin,
        marker_generation_at_record: 1,
        recorded_ms: crate::state::now_ms(),
    };
    record.validate()?;
    Ok(record)
}

const IDENTITY_CAP: usize = 4096;

fn private_directory(
    dirs: &dyn DirOps,
    parent: &DirHandle,
    name: &str,
    uid: u32,
    create: bool,
) -> Result<Option<DirHandle>> {
    let mut facts = dirs.child_facts(parent, name)?;
    if facts.is_none() && create {
        match dirs.create_child_dir(parent, name, 0o700) {
            Ok(_) => {
                dirs.sync_dir(parent)?;
            }
            Err(error) => {
                if dirs.child_facts(parent, name)?.is_none() {
                    return Err(error);
                }
            }
        }
        facts = dirs.child_facts(parent, name)?;
    }
    let Some(facts) = facts else {
        return Ok(None);
    };
    if facts.kind != EntryKind::Directory
        || facts.uid != uid
        || !facts.is_private()
        || facts.device != parent.identity().device
    {
        return Err(Error::new(
            ErrorKind::UnknownState,
            "identity state directory is not private and owned",
        ));
    }
    let handle = dirs.open_child_dir(parent, name)?;
    if handle.identity()
        != (DirId {
            device: facts.device,
            inode: facts.inode,
        })
    {
        return Err(Error::new(
            ErrorKind::UnknownState,
            "identity state directory changed during open",
        ));
    }
    Ok(Some(handle))
}

fn identity_directory(
    state: &StatePaths,
    dirs: &dyn DirOps,
    root_id: &str,
    uid: u32,
    create: bool,
) -> Result<Option<DirHandle>> {
    validate_identifier("root_id", root_id)?;
    let state_dir = dirs.open_path(state.root())?;
    let facts = dirs.dir_facts(&state_dir)?;
    if facts.kind != EntryKind::Directory || facts.uid != uid || !facts.is_private() {
        return Err(Error::new(
            ErrorKind::UnknownState,
            "state directory is not private and owned",
        ));
    }
    let Some(roots) = private_directory(dirs, &state_dir, "roots", uid, create)? else {
        return Ok(None);
    };
    private_directory(dirs, &roots, root_id, uid, create)
}

/// Read the immutable record without creating state. Missing remains distinct
/// from invalid, and no link or foreign or nonprivate file is accepted.
pub fn read(
    state: &StatePaths,
    dirs: &dyn DirOps,
    root_id: &str,
    uid: u32,
) -> Result<Option<RootIdentity>> {
    if !state.is_initialized()? {
        return Ok(None);
    }
    let Some(directory) = identity_directory(state, dirs, root_id, uid, false)? else {
        return Ok(None);
    };
    let Some(before) = dirs.child_facts(&directory, "identity.json")? else {
        return Ok(None);
    };
    if before.kind != EntryKind::File
        || before.uid != uid
        || !before.is_private()
        || before.size > IDENTITY_CAP as u64
    {
        return Err(Error::new(
            ErrorKind::UnknownState,
            "identity record is not a private owned regular file",
        ));
    }
    let bytes = dirs
        .read_child_file(&directory, "identity.json", IDENTITY_CAP)?
        .ok_or_else(|| Error::new(ErrorKind::UnknownState, "identity record disappeared"))?;
    let after = dirs
        .child_facts(&directory, "identity.json")?
        .ok_or_else(|| Error::new(ErrorKind::UnknownState, "identity record disappeared"))?;
    if before != after || dirs.recheck_dir(&directory)? != directory.identity() {
        return Err(Error::new(
            ErrorKind::UnknownState,
            "identity record changed during read",
        ));
    }
    let record: RootIdentity = serde_json::from_slice(&bytes).map_err(|error| {
        Error::new(
            ErrorKind::UnknownState,
            format!("invalid identity record: {error}"),
        )
    })?;
    record.validate()?;
    if record.root_id != root_id {
        return Err(Error::new(
            ErrorKind::UnknownState,
            "identity record names another root",
        ));
    }
    Ok(Some(record))
}

/// Publish once under the matching exclusive root lock. A pre-existing record
/// is never replaced, including if it appears during publication.
pub fn write_once(
    state: &StatePaths,
    dirs: &dyn DirOps,
    lock: &StateLock,
    uid: u32,
    record: &RootIdentity,
) -> Result<()> {
    record.validate()?;
    if lock.rank() != LockRank::Root
        || lock.mode() != LockMode::Exclusive
        || lock.path() != state.root_lock_path(&record.root_id)?
    {
        return Err(Error::new(
            ErrorKind::UnknownState,
            "identity write requires its exclusive root lock",
        ));
    }
    let directory = identity_directory(state, dirs, &record.root_id, uid, true)?
        .ok_or_else(|| Error::new(ErrorKind::UnknownState, "identity directory missing"))?;
    if dirs.child_facts(&directory, "identity.json")?.is_some() {
        return Err(Error::new(
            ErrorKind::UnknownState,
            "identity record already exists",
        ));
    }
    let mut bytes = serde_json::to_vec_pretty(record)
        .map_err(|error| Error::new(ErrorKind::Io, format!("cannot encode identity: {error}")))?;
    bytes.push(b'\n');
    if bytes.len() > IDENTITY_CAP {
        return Err(Error::new(
            ErrorKind::ResourceBound,
            "identity record exceeds cap",
        ));
    }
    let random = dirs.random_bytes(16)?;
    let stage = format!(
        ".identity-{}",
        random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    if dirs.child_facts(&directory, &stage)?.is_some() {
        return Err(Error::new(
            ErrorKind::UnknownState,
            "identity staging name occupied",
        ));
    }
    dirs.publish_child_file(&directory, &stage, &bytes, 0o600)?;
    let staged = dirs
        .child_facts(&directory, &stage)?
        .ok_or_else(|| Error::new(ErrorKind::UnknownState, "identity staging file disappeared"))?;
    if staged.kind != EntryKind::File
        || staged.uid != uid
        || !staged.is_private()
        || dirs.recheck_dir(&directory)? != directory.identity()
    {
        return Err(Error::new(
            ErrorKind::UnknownState,
            "identity staging binding changed",
        ));
    }
    if dirs.rename_child(&directory, &stage, "identity.json")? == RenameOutcome::TargetExists {
        return Err(Error::new(
            ErrorKind::UnknownState,
            "identity record appeared before publication",
        ));
    }
    dirs.sync_dir(&directory)?;
    if read(state, dirs, &record.root_id, uid)?.as_ref() != Some(record) {
        return Err(Error::new(
            ErrorKind::UnknownState,
            "published identity did not read back",
        ));
    }
    Ok(())
}

/// Facts read from the verified volume and its opened root for one observation.
/// Callers must obtain these through the descriptor walk; no stored verdict is
/// authority for a later use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootObservation {
    pub matching_mounts: usize,
    pub volume_uuid: String,
    pub mount: PathBuf,
    pub device: u64,
    pub inode: u64,
    pub root_uid: u32,
    pub effective_uid: u32,
    pub root_id: String,
    pub nonce: String,
    pub mount_relative_path: String,
    pub kind: RootKind,
    pub area: MarkerArea,
}

/// A durable identity comparison. Unknown keeps the root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum IdentityVerdict {
    Verified {
        device_at_record: u64,
        device_live: u64,
        device_changed: bool,
        mount_moved: bool,
    },
    Unknown {
        reason: IdentityReason,
    },
    Gone,
}

/// The reason an observation cannot establish the recorded identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IdentityReason {
    IdentityRecordMissing,
    IdentityRecordInvalid,
    VolumeUnverified,
    DuplicateVolumeUuid,
    NotARealDirectory,
    PathChanged,
    InodeChanged,
    MarkerIdentityMismatch,
    MarkerAbsent,
    MarkerUnreadable,
    MarkerForeignOwner,
    MarkerVersionUnknown,
    MarkerSchemaDivergent,
    MarkerGenerationUnexpected,
}

/// Verify against a complete, freshly observed set of mounts for the UUID.
/// The caller supplies that census; an unavailable census must never be passed
/// as an empty or complete set. All filesystem facts are read again here.
pub fn verify(
    state: &StatePaths,
    dirs: &dyn DirOps,
    facts: &dyn crate::os::VolumeFacts,
    mounts: &dyn crate::os::MountRoot,
    uid: u32,
    root_id: &str,
    candidates: &[PathBuf],
) -> IdentityVerdict {
    let unknown = |reason| IdentityVerdict::Unknown { reason };
    let record = match read(state, dirs, root_id, uid) {
        Ok(Some(record)) => record,
        Ok(None) => return unknown(IdentityReason::IdentityRecordMissing),
        Err(_) => return unknown(IdentityReason::IdentityRecordInvalid),
    };
    if candidates.len() > 1 {
        return unknown(IdentityReason::DuplicateVolumeUuid);
    }
    let Some(mount) = candidates.first() else {
        return unknown(IdentityReason::VolumeUnverified);
    };
    let Ok(uuid) = VolumeUuid::parse(&record.volume_uuid) else {
        return unknown(IdentityReason::IdentityRecordInvalid);
    };
    let expected = crate::os::ExpectedVolume {
        volume_uuid: &uuid,
        mount_hint: None,
    };
    let Ok(volume) = crate::os::verify_volume(facts, mounts, mount, &expected) else {
        return unknown(IdentityReason::VolumeUnverified);
    };
    let Ok(mut current) = dirs.open_path(volume.path()) else {
        return unknown(IdentityReason::VolumeUnverified);
    };
    if current.identity().device != volume.identity().device {
        return unknown(IdentityReason::VolumeUnverified);
    }
    let Ok(components) = folder_components("mount_relative_path", &record.mount_relative_path)
    else {
        return unknown(IdentityReason::IdentityRecordInvalid);
    };
    for (index, component) in components.iter().enumerate() {
        let entry = match dirs.child_facts(&current, component) {
            Ok(Some(entry)) => entry,
            Ok(None) if index + 1 != components.len() => {
                return unknown(IdentityReason::PathChanged)
            }
            Ok(None) => {
                return if volume.reverify(facts).is_ok() {
                    IdentityVerdict::Gone
                } else {
                    unknown(IdentityReason::VolumeUnverified)
                }
            }
            Err(_) => return unknown(IdentityReason::NotARealDirectory),
        };
        if entry.kind != EntryKind::Directory
            || entry.uid != uid
            || entry.device != volume.identity().device
        {
            return unknown(IdentityReason::NotARealDirectory);
        }
        let Ok(opened) = dirs.open_child_dir(&current, component) else {
            return unknown(IdentityReason::PathChanged);
        };
        if opened.identity()
            != (DirId {
                device: entry.device,
                inode: entry.inode,
            })
        {
            return unknown(IdentityReason::PathChanged);
        }
        current = opened;
    }
    if current.identity().inode != record.inode {
        return unknown(IdentityReason::InodeChanged);
    }
    let marker_facts = match dirs.child_facts(&current, crate::area::ROOT_MARKER_FILE) {
        Ok(Some(value)) => value,
        Ok(None) => return unknown(IdentityReason::MarkerAbsent),
        Err(_) => return unknown(IdentityReason::MarkerUnreadable),
    };
    if marker_facts.uid != uid {
        return unknown(IdentityReason::MarkerForeignOwner);
    }
    if marker_facts.kind != EntryKind::File
        || !marker_facts.is_private()
        || marker_facts.size > 4096
    {
        return unknown(IdentityReason::MarkerUnreadable);
    }
    let bytes = match dirs.read_child_file(&current, crate::area::ROOT_MARKER_FILE, 4096) {
        Ok(Some(bytes)) => bytes,
        _ => return unknown(IdentityReason::MarkerUnreadable),
    };
    let value: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(_) => return unknown(IdentityReason::MarkerSchemaDivergent),
    };
    if !matches!(
        value.get("schema").and_then(serde_json::Value::as_str),
        Some("barge.root/1" | "barge.root/2")
    ) {
        return unknown(IdentityReason::MarkerVersionUnknown);
    }
    let marker: crate::area::RootMarker = match serde_json::from_slice(&bytes) {
        Ok(marker) => marker,
        Err(_) => return unknown(IdentityReason::MarkerSchemaDivergent),
    };
    if marker.schema == "barge.root/2"
        && (marker.mount_relative_path.as_deref() != Some(record.mount_relative_path.as_str())
            || marker.mount_at_record.as_ref() != Some(&record.mount_at_record))
    {
        return unknown(IdentityReason::PathChanged);
    }
    if marker.owner != "barge" {
        return unknown(IdentityReason::MarkerForeignOwner);
    }
    if !matches!(
        (marker.kind, marker.generation, marker.conclusion.is_some()),
        (_, 1, false) | (RootKind::ScratchRun, 2, true)
    ) {
        return unknown(IdentityReason::MarkerGenerationUnexpected);
    }
    let expectation = crate::area::MarkerExpectation {
        volume_uuid: &uuid,
        resolved_path: &mount.join(&record.mount_relative_path),
        effective_uid: uid,
        now_ms: crate::state::now_ms(),
    };
    if !matches!(
        crate::area::read_marker(dirs, &current, &expectation),
        crate::area::MarkerState::Owned(_)
    ) {
        return unknown(IdentityReason::MarkerSchemaDivergent);
    }
    if dirs
        .child_facts(&current, crate::area::ROOT_MARKER_FILE)
        .ok()
        .flatten()
        .as_ref()
        != Some(&marker_facts)
        || dirs.recheck_dir(&current).ok() != Some(current.identity())
        || volume.reverify(facts).is_err()
    {
        return unknown(IdentityReason::PathChanged);
    }
    compare(
        &record,
        &RootObservation {
            matching_mounts: candidates.len(),
            volume_uuid: uuid.as_str().to_owned(),
            mount: mount.clone(),
            device: current.identity().device.get(),
            inode: current.identity().inode,
            root_uid: uid,
            effective_uid: uid,
            root_id: marker.root_id,
            nonce: marker.nonce,
            mount_relative_path: record.mount_relative_path.clone(),
            kind: marker.kind,
            area: marker.area,
        },
    )
}

/// Compare an already walked root with its immutable record. A device change
/// across runs is diagnostic only; the live walk must itself check the device.
pub fn compare(record: &RootIdentity, live: &RootObservation) -> IdentityVerdict {
    let reason = if record.validate().is_err() {
        Some(IdentityReason::IdentityRecordInvalid)
    } else if live.matching_mounts > 1 {
        Some(IdentityReason::DuplicateVolumeUuid)
    } else if live.matching_mounts != 1 || record.volume_uuid != live.volume_uuid {
        Some(IdentityReason::VolumeUnverified)
    } else if live.root_uid != live.effective_uid || live.inode == 0 || live.device == 0 {
        Some(IdentityReason::NotARealDirectory)
    } else if record.mount_relative_path != live.mount_relative_path {
        Some(IdentityReason::PathChanged)
    } else if record.inode != live.inode {
        Some(IdentityReason::InodeChanged)
    } else if record.root_id != live.root_id
        || record.nonce != live.nonce
        || record.kind != live.kind
        || record.area.namespace != live.area.namespace
        || record.area.area != live.area.area
    {
        Some(IdentityReason::MarkerIdentityMismatch)
    } else {
        None
    };
    match reason {
        Some(reason) => IdentityVerdict::Unknown { reason },
        None => IdentityVerdict::Verified {
            device_at_record: record.device_at_record,
            device_live: live.device,
            device_changed: record.device_at_record != live.device,
            mount_moved: record.mount_at_record != live.mount,
        },
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn record() -> RootIdentity {
        RootIdentity {
            schema: "barge.root.identity/1".into(),
            root_id: "0123456789abcdef0123456789abcdef".into(),
            nonce: "abcdef0123456789abcdef0123456789".into(),
            volume_uuid: "01234567-89AB-CDEF-0123-456789ABCDEF".into(),
            mount_relative_path: "oxide/outputs/root".into(),
            inode: 42,
            device_at_record: 7,
            mount_at_record: "/Volumes/Builds".into(),
            kind: RootKind::AreaRoot,
            area: MarkerArea::identity("oxide", "outputs"),
            origin: IdentityOrigin::Created,
            marker_generation_at_record: 1,
            recorded_ms: 0,
        }
    }

    fn observation(record: &RootIdentity) -> RootObservation {
        RootObservation {
            matching_mounts: 1,
            volume_uuid: record.volume_uuid.clone(),
            mount: record.mount_at_record.clone(),
            device: record.device_at_record,
            inode: record.inode,
            root_uid: 501,
            effective_uid: 501,
            root_id: record.root_id.clone(),
            nonce: record.nonce.clone(),
            mount_relative_path: record.mount_relative_path.clone(),
            kind: record.kind,
            area: record.area.clone(),
        }
    }

    #[test]
    fn remount_changes_are_diagnostic_not_identity() {
        let identity = record();
        let mut live = observation(&identity);
        live.device += 1;
        live.mount = "/Volumes/Builds 1".into();
        assert_ne!(identity.device_at_record, live.device); // The old comparison refused.
        assert_eq!(
            compare(&identity, &live),
            IdentityVerdict::Verified {
                device_at_record: identity.device_at_record,
                device_live: live.device,
                device_changed: true,
                mount_moved: true,
            }
        );
    }

    #[test]
    fn mismatches_and_duplicate_mounts_keep_the_root() {
        let identity = record();
        let mut live = observation(&identity);
        live.matching_mounts = 2;
        assert_eq!(
            compare(&identity, &live),
            IdentityVerdict::Unknown {
                reason: IdentityReason::DuplicateVolumeUuid
            }
        );
        live = observation(&identity);
        live.root_uid += 1;
        assert_eq!(
            compare(&identity, &live),
            IdentityVerdict::Unknown {
                reason: IdentityReason::NotARealDirectory
            }
        );
        live = observation(&identity);
        live.inode += 1;
        assert_eq!(
            compare(&identity, &live),
            IdentityVerdict::Unknown {
                reason: IdentityReason::InodeChanged
            }
        );
        live = observation(&identity);
        live.nonce = identity.root_id.clone();
        assert_eq!(
            compare(&identity, &live),
            IdentityVerdict::Unknown {
                reason: IdentityReason::MarkerIdentityMismatch
            }
        );
        // Inode reuse cannot pass when the replacement's nonce is different.
        assert_eq!(live.inode, identity.inode);
    }

    #[test]
    fn record_round_trips_and_rejects_unknown_fields() {
        let value = serde_json::to_value(record()).unwrap();
        let parsed: RootIdentity = serde_json::from_value(value.clone()).unwrap();
        parsed.validate().unwrap();
        let mut extra = value;
        extra["unexpected"] = true.into();
        assert!(serde_json::from_value::<RootIdentity>(extra).is_err());
    }

    #[test]
    fn record_rejects_noncanonical_or_escaping_facts() {
        let mut value = record();
        value.volume_uuid.make_ascii_lowercase();
        assert!(value.validate().is_err());
        value = record();
        value.mount_relative_path = "oxide/../root".into();
        assert!(value.validate().is_err());
        value = record();
        value.inode = 0;
        assert!(value.validate().is_err());
        value = record();
        value.nonce.make_ascii_uppercase();
        assert!(value.validate().is_err());
    }
}
