//! The OS layer's facts, traits and value types.
//!
//! This module is the whole surface a module above `os` may use. It names no
//! syscall, errno or platform path convention: the platform decides those
//! inside its own implementation, which is what keeps the Linux follow-on
//! additive (stage 1 design, "Crate and CLI skeleton").

use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, ErrorKind, Result};

/// A device identifier as the platform reports it for an opened file.
///
/// The volume UUID is the identity that matters across remounts; the device is
/// part of the root identity a later slice records and re-verifies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct DeviceId(u64);

impl DeviceId {
    /// The device from the platform's own number for it.
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// The number as recorded.
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for DeviceId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

/// A filesystem type as the platform names it.
///
/// Barge decides only whether the volume is APFS. Every other name is reported
/// and refused rather than guessed at, so an unexpected filesystem surfaces as
/// itself instead of as a wrong failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileSystem {
    /// APFS, the only filesystem stage 1 supports.
    Apfs,
    /// Any other filesystem, with the platform's own name for it.
    Other(String),
}

impl FileSystem {
    /// The name to report.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Apfs => "apfs",
            Self::Other(name) => name,
        }
    }
}

impl fmt::Display for FileSystem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The kernel's APFS volume UUID, normalised to one spelling so two readings of
/// one volume never compare unequal.
///
/// It reads and writes as its canonical text, so a record that holds one cannot
/// hold a malformed value: parsing is the only way in, and a document with a
/// malformed UUID is refused rather than repaired.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct VolumeUuid(String);

impl VolumeUuid {
    /// Parse a UUID as written: eight, four, four, four and twelve hexadecimal
    /// digits, separated by hyphens. A malformed value is unknown state, never
    /// a substituted one.
    pub fn parse(raw: &str) -> Result<Self> {
        let trimmed = raw.trim();
        let well_formed = trimmed.len() == 36
            && trimmed.char_indices().all(|(index, character)| {
                if matches!(index, 8 | 13 | 18 | 23) {
                    character == '-'
                } else {
                    character.is_ascii_hexdigit()
                }
            });
        if !well_formed {
            return Err(Error::new(
                ErrorKind::UnknownState,
                format!("`{trimmed}` is not a volume UUID"),
            )
            .with_detail("volume_uuid", raw.into()));
        }
        Ok(Self(trimmed.to_ascii_uppercase()))
    }

    /// The canonical form of the kernel's sixteen bytes.
    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        let mut text = String::with_capacity(36);
        for (index, byte) in bytes.iter().enumerate() {
            if matches!(index, 4 | 6 | 8 | 10) {
                text.push('-');
            }
            text.push_str(&format!("{byte:02X}"));
        }
        Self(text)
    }

    /// The canonical UUID text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for VolumeUuid {
    type Error = Error;

    fn try_from(raw: String) -> Result<Self> {
        Self::parse(&raw)
    }
}

impl From<VolumeUuid> for String {
    fn from(uuid: VolumeUuid) -> Self {
        uuid.0
    }
}

impl fmt::Display for VolumeUuid {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// The facts of one volume, read through an opened descriptor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeIdentity {
    /// The mount point the kernel reports for the volume.
    pub mount_point: PathBuf,
    /// The volume's filesystem.
    pub filesystem: FileSystem,
    /// The device of the directory that was opened.
    pub device: DeviceId,
    /// The kernel's volume UUID, or `None` when the platform reports no APFS
    /// UUID for the volume. An APFS volume's unreadable UUID refuses at the
    /// platform, and verification refuses `None` as unknown state; neither
    /// defaults it.
    pub volume_uuid: Option<VolumeUuid>,
    /// Whether this is the boot volume, which Barge never manages.
    pub boot_volume: bool,
}

/// What a registered volume record says a path's volume must be.
#[derive(Debug, Clone, Copy)]
pub struct ExpectedVolume<'a> {
    /// The volume UUID the record holds; the identity that matters.
    pub volume_uuid: &'a VolumeUuid,
    /// The mount point the record last observed. It is used to find the volume
    /// and to refuse a different volume mounted in its place, never as identity.
    pub mount_hint: Option<&'a Path>,
}

/// A path the platform proved is the mount root of its own volume.
///
/// The type is a proof marker: only a platform implementation, which has read
/// the kernel's own mount facts, can construct one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountPoint {
    path: PathBuf,
}

impl MountPoint {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// The mount root's path.
    pub fn as_path(&self) -> &Path {
        &self.path
    }
}

/// Platform facts about volumes, read through an opened descriptor.
pub trait VolumeFacts {
    /// Identify the volume that holds `path`.
    ///
    /// Every fact is read from a descriptor opened on `path`; a fact the
    /// platform cannot read is refused, never defaulted. A path whose volume
    /// has vanished is an error, never "absent and safe".
    fn identify(&self, path: &Path) -> Result<VolumeIdentity>;
}

/// Whether a path is the mount root of a volume Barge may manage.
pub trait MountRoot {
    /// Prove `path` is a mount root: a real directory of this user that the
    /// kernel reports as the mount point for itself, and that passes the
    /// platform's mount-parent rule.
    fn mount_root(&self, path: &Path) -> Result<MountPoint>;
}

/// The account facts Barge resolves state from.
///
/// The account home comes from the password database for the effective uid,
/// never from the environment, so a fixture `HOME` cannot move an account's
/// authority (stage 1 design, "Account-wide state directory").
pub trait AccountFacts {
    /// The account's home directory.
    fn account_home(&self) -> Result<PathBuf>;

    /// The account-wide state root this platform defines for Barge.
    fn state_root(&self) -> Result<PathBuf>;

    /// The account-wide state directory this platform gives another tool.
    ///
    /// Barge reads a foreign tool's state in place and never writes it, so the
    /// platform's convention belongs here and the leaf names stay in the module
    /// that knows that tool's formats.
    fn tool_state_dir(&self, tool: &str) -> Result<PathBuf>;

    /// The account-wide cache directory this platform gives another tool.
    fn tool_cache_dir(&self, tool: &str) -> Result<PathBuf>;
}

/// A volume whose facts were read and matched the expectation.
///
/// Holding one proves nothing about the next moment: verification is repeated
/// on every use, so a caller re-reads before an effect rather than trusting
/// this value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedVolume {
    path: PathBuf,
    identity: VolumeIdentity,
}

impl VerifiedVolume {
    pub(crate) fn new(path: PathBuf, identity: VolumeIdentity) -> Self {
        Self { path, identity }
    }

    /// The path that was opened and verified.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The facts that were read and matched.
    pub fn identity(&self) -> &VolumeIdentity {
        &self.identity
    }

    /// Re-read every fact from the same path and refuse when anything differs:
    /// a different volume, a remount, a vanished volume or an unreadable fact.
    pub fn reverify(&self, facts: &dyn VolumeFacts) -> Result<()> {
        let again = facts.identify(&self.path)?;
        if again == self.identity {
            return Ok(());
        }
        Err(Error::new(
            ErrorKind::VolumeMismatch,
            format!(
                "the volume at {} changed since it was verified",
                self.path.display()
            ),
        )
        .with_detail("path", self.path.display().to_string().into())
        .with_detail("verified_device", self.identity.device.to_string().into())
        .with_detail("actual_device", again.device.to_string().into())
        .with_detail(
            "verified_mount",
            self.identity.mount_point.display().to_string().into(),
        )
        .with_detail(
            "actual_mount",
            again.mount_point.display().to_string().into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::{DeviceId, FileSystem, VolumeUuid};
    use crate::error::ErrorKind;

    #[test]
    fn a_uuid_is_normalised_to_one_spelling() {
        let upper = VolumeUuid::parse("A1B2C3D4-E5F6-4A7B-8C9D-0E1F2A3B4C5D")
            .expect("a canonical UUID parses");
        let lower = VolumeUuid::parse("a1b2c3d4-e5f6-4a7b-8c9d-0e1f2a3b4c5d")
            .expect("a lowercase UUID parses");
        assert_eq!(upper, lower);
        assert_eq!(upper.as_str(), "A1B2C3D4-E5F6-4A7B-8C9D-0E1F2A3B4C5D");
    }

    #[test]
    fn the_kernel_bytes_become_the_canonical_text() {
        let uuid = VolumeUuid::from_bytes([
            0xA1, 0xB2, 0xC3, 0xD4, 0xE5, 0xF6, 0x4A, 0x7B, 0x8C, 0x9D, 0x0E, 0x1F, 0x2A, 0x3B,
            0x4C, 0x5D,
        ]);
        assert_eq!(uuid.as_str(), "A1B2C3D4-E5F6-4A7B-8C9D-0E1F2A3B4C5D");
    }

    #[test]
    fn a_malformed_uuid_is_unknown_state() {
        for raw in [
            "",
            "not-a-uuid",
            "A1B2C3D4E5F64A7B8C9D0E1F2A3B4C5D",
            "A1B2C3D4-E5F6-4A7B-8C9D-0E1F2A3B4C5",
            "G1B2C3D4-E5F6-4A7B-8C9D-0E1F2A3B4C5D",
            "A1B2C3D4+E5F6+4A7B+8C9D+0E1F2A3B4C5D",
        ] {
            let error = VolumeUuid::parse(raw).expect_err("a malformed UUID is refused");
            assert_eq!(error.kind(), ErrorKind::UnknownState, "{raw}");
        }
    }

    #[test]
    fn a_uuid_reads_and_writes_as_its_canonical_text() {
        let uuid = VolumeUuid::parse("a1b2c3d4-e5f6-4a7b-8c9d-0e1f2a3b4c5d")
            .expect("a lowercase UUID parses");
        let json = serde_json::to_value(&uuid).expect("a UUID serialises");
        assert_eq!(
            json,
            serde_json::json!("A1B2C3D4-E5F6-4A7B-8C9D-0E1F2A3B4C5D")
        );
        let read: VolumeUuid = serde_json::from_value(json).expect("its own text parses back");
        assert_eq!(read, uuid);
        assert!(
            serde_json::from_value::<VolumeUuid>(serde_json::json!("not-a-uuid")).is_err(),
            "a malformed UUID never becomes a value"
        );
    }

    #[test]
    fn a_filesystem_reports_the_platform_name() {
        assert_eq!(FileSystem::Apfs.as_str(), "apfs");
        assert_eq!(FileSystem::Other("exfat".to_owned()).as_str(), "exfat");
        assert_eq!(DeviceId::new(16_777_219).to_string(), "16777219");
    }
}
