//! Durable owned-root identity, independent of a mount's device number.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::area::{folder_components, MarkerArea, RootKind};
use crate::error::{Error, ErrorKind, Result};
use crate::os::VolumeUuid;
use crate::state::validate_identifier;

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
}

/// The reason an observation cannot establish the recorded identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IdentityReason {
    IdentityRecordInvalid,
    VolumeUnverified,
    DuplicateVolumeUuid,
    NotARealDirectory,
    PathChanged,
    InodeChanged,
    MarkerIdentityMismatch,
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
