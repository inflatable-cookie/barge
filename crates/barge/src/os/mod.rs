//! The OS layer: every platform fact Barge reads, behind narrow traits, plus
//! the platform-neutral verification policy over them.
//!
//! No module above `os` may name a macOS syscall, path convention or errno;
//! the traits and value types in `facts` are the whole surface they get. The
//! macOS implementation is `macos`, and the Linux layer is compile-time
//! absent (stage 1 design, Q-005): a non-macOS build fails here instead of
//! falling back to a runtime stub that would pretend to read the platform.

use std::path::Path;

mod facts;

#[cfg(target_os = "macos")]
mod macos;

pub use facts::{
    AccountFacts, DeviceId, ExpectedVolume, FileSystem, MountPoint, MountRoot, VerifiedVolume,
    VolumeFacts, VolumeIdentity, VolumeUuid,
};

#[cfg(target_os = "macos")]
pub use macos::MacOs;

#[cfg(not(target_os = "macos"))]
compile_error!(
    "Barge implements macOS on APFS only (stage 1 design, Q-005). The Linux OS layer is \
     compile-time absent: there is no fallback implementation to build against."
);

use crate::error::{Error, ErrorKind, Result};

/// The platform's OS layer. It is the only way a caller reaches the platform,
/// so an implementation is chosen once, at the bottom of the crate.
#[cfg(target_os = "macos")]
pub const fn platform() -> MacOs {
    MacOs::new()
}

/// Verify that `path` is the volume `expected` names.
///
/// The facts are read through an opened descriptor, the volume must be APFS, its
/// UUID must match the record, its mount point must be proved to be a mount
/// root, and the boot volume is refused. Any unreadable fact is unknown state;
/// any different fact is `volume_mismatch`; a vanished volume is an error. A
/// refusal here changes nothing, because nothing has been written yet.
pub fn verify_volume(
    facts: &dyn VolumeFacts,
    mounts: &dyn MountRoot,
    path: &Path,
    expected: &ExpectedVolume<'_>,
) -> Result<VerifiedVolume> {
    let identity = facts.identify(path)?;

    if identity.boot_volume {
        return Err(Error::new(
            ErrorKind::RefusedBoundary,
            format!(
                "{} is on the boot volume, which Barge never manages",
                path.display()
            ),
        )
        .with_detail("path", path.display().to_string().into()));
    }

    if identity.filesystem != FileSystem::Apfs {
        return Err(Error::new(
            ErrorKind::VolumeMismatch,
            format!(
                "{} is on a {} volume; Barge manages APFS only",
                path.display(),
                identity.filesystem
            ),
        )
        .with_detail("path", path.display().to_string().into())
        .with_detail("filesystem", identity.filesystem.as_str().into())
        .with_detail("expected_filesystem", "apfs".into()));
    }

    match &identity.volume_uuid {
        Some(volume_uuid) if volume_uuid == expected.volume_uuid => {}
        Some(volume_uuid) => {
            return Err(Error::new(
                ErrorKind::VolumeMismatch,
                format!(
                    "the volume at {} is {volume_uuid}, not the registered {}",
                    path.display(),
                    expected.volume_uuid
                ),
            )
            .with_detail("path", path.display().to_string().into())
            .with_detail("expected_volume_uuid", expected.volume_uuid.as_str().into())
            .with_detail("actual_volume_uuid", volume_uuid.as_str().into()));
        }
        None => {
            return Err(Error::new(
                ErrorKind::UnknownState,
                format!(
                    "the volume UUID at {} is unreadable; Barge does not default it",
                    path.display()
                ),
            )
            .with_detail("path", path.display().to_string().into()));
        }
    }

    if let Some(mount_hint) = expected.mount_hint {
        let mount_root = mounts.mount_root(path)?;
        if mount_root.as_path() != mount_hint {
            return Err(mount_mismatch(
                path,
                mount_hint,
                mount_root.as_path(),
                "the path is not the recorded mount root",
            ));
        }
        if identity.mount_point != mount_hint {
            return Err(mount_mismatch(
                path,
                mount_hint,
                &identity.mount_point,
                "the kernel reports a different mount point",
            ));
        }
    }

    Ok(VerifiedVolume::new(path.to_path_buf(), identity))
}

/// The refusal for a volume that is not where the record says it is.
fn mount_mismatch(path: &Path, expected: &Path, actual: &Path, reason: &str) -> Error {
    Error::new(
        ErrorKind::VolumeMismatch,
        format!(
            "{} does not resolve to the recorded mount root {}: {reason}",
            path.display(),
            expected.display()
        ),
    )
    .with_detail("path", path.display().to_string().into())
    .with_detail("expected_mount", expected.display().to_string().into())
    .with_detail("actual_mount", actual.display().to_string().into())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::path::{Path, PathBuf};

    use super::{
        verify_volume, DeviceId, ExpectedVolume, FileSystem, MountPoint, MountRoot, VolumeFacts,
        VolumeIdentity, VolumeUuid,
    };
    use crate::error::{Error, ErrorKind, Result};

    const UUID: &str = "A1B2C3D4-E5F6-4A7B-8C9D-0E1F2A3B4C5D";
    const OTHER_UUID: &str = "0F1E2D3C-4B5A-6978-8796-A5B4C3D2E1F0";

    /// Injected platform facts: a test decides exactly what the volume says.
    struct FakeFacts(Result<VolumeIdentity>);

    impl VolumeFacts for FakeFacts {
        fn identify(&self, _path: &Path) -> Result<VolumeIdentity> {
            self.0.clone()
        }
    }

    /// Injected mount facts: a test decides the mount root the platform proves.
    struct FakeMounts(PathBuf);

    impl MountRoot for FakeMounts {
        fn mount_root(&self, _path: &Path) -> Result<MountPoint> {
            Ok(MountPoint::new(self.0.clone()))
        }
    }

    fn identity(volume_uuid: Option<&str>, mount_point: &str) -> VolumeIdentity {
        VolumeIdentity {
            mount_point: PathBuf::from(mount_point),
            filesystem: FileSystem::Apfs,
            device: DeviceId::new(16_777_219),
            volume_uuid: volume_uuid.map(|raw| VolumeUuid::parse(raw).expect("a test UUID")),
            boot_volume: false,
        }
    }

    fn expected<'a>(
        volume_uuid: &'a VolumeUuid,
        mount_hint: Option<&'a Path>,
    ) -> ExpectedVolume<'a> {
        ExpectedVolume {
            volume_uuid,
            mount_hint,
        }
    }

    #[test]
    fn a_matching_volume_verifies() {
        let uuid = VolumeUuid::parse(UUID).expect("a test UUID");
        let facts = FakeFacts(Ok(identity(Some(UUID), "/Volumes/Builds")));
        let mounts = FakeMounts(PathBuf::from("/Volumes/Builds"));
        let path = Path::new("/Volumes/Builds/scratch");
        let verified = verify_volume(
            &facts,
            &mounts,
            path,
            &expected(&uuid, Some(Path::new("/Volumes/Builds"))),
        )
        .expect("the facts match the record");
        assert_eq!(
            verified.identity().mount_point,
            PathBuf::from("/Volumes/Builds")
        );
        assert_eq!(verified.path(), path);
    }

    #[test]
    fn an_unreadable_volume_uuid_refuses_rather_than_defaulting() {
        let uuid = VolumeUuid::parse(UUID).expect("a test UUID");
        let facts = FakeFacts(Ok(identity(None, "/Volumes/Builds")));
        let mounts = FakeMounts(PathBuf::from("/Volumes/Builds"));
        let error = verify_volume(
            &facts,
            &mounts,
            Path::new("/Volumes/Builds"),
            &expected(&uuid, None),
        )
        .expect_err("an unreadable UUID is unknown state");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
    }

    #[test]
    fn a_different_volume_at_the_mount_point_is_a_mismatch() {
        let uuid = VolumeUuid::parse(UUID).expect("a test UUID");
        let facts = FakeFacts(Ok(identity(Some(OTHER_UUID), "/Volumes/Builds")));
        let mounts = FakeMounts(PathBuf::from("/Volumes/Builds"));
        let error = verify_volume(
            &facts,
            &mounts,
            Path::new("/Volumes/Builds"),
            &expected(&uuid, Some(Path::new("/Volumes/Builds"))),
        )
        .expect_err("a different UUID is a mismatch");
        assert_eq!(error.kind(), ErrorKind::VolumeMismatch);
        assert_eq!(error.detail()["expected_volume_uuid"], UUID);
        assert_eq!(error.detail()["actual_volume_uuid"], OTHER_UUID);
    }

    #[test]
    fn a_mount_point_that_is_not_the_recorded_root_is_a_mismatch() {
        let uuid = VolumeUuid::parse(UUID).expect("a test UUID");
        // The UUID matches but the kernel says the volume is mounted elsewhere:
        // the identity is right and the place is wrong.
        let facts = FakeFacts(Ok(identity(Some(UUID), "/Volumes/Elsewhere")));
        let mounts = FakeMounts(PathBuf::from("/Volumes/Elsewhere"));
        let error = verify_volume(
            &facts,
            &mounts,
            Path::new("/Volumes/Builds"),
            &expected(&uuid, Some(Path::new("/Volumes/Builds"))),
        )
        .expect_err("the recorded mount root is not this volume's");
        assert_eq!(error.kind(), ErrorKind::VolumeMismatch);
        assert_eq!(error.detail()["expected_mount"], "/Volumes/Builds");
        assert_eq!(error.detail()["actual_mount"], "/Volumes/Elsewhere");
    }

    #[test]
    fn a_vanished_volume_is_an_error_not_an_absent_and_safe_answer() {
        let uuid = VolumeUuid::parse(UUID).expect("a test UUID");
        let facts = FakeFacts(Err(Error::new(
            ErrorKind::VolumeAbsent,
            "the volume holding /Volumes/Builds is not available",
        )));
        let mounts = FakeMounts(PathBuf::from("/Volumes/Builds"));
        let error = verify_volume(
            &facts,
            &mounts,
            Path::new("/Volumes/Builds"),
            &expected(&uuid, Some(Path::new("/Volumes/Builds"))),
        )
        .expect_err("a vanished volume refuses");
        assert_eq!(error.kind(), ErrorKind::VolumeAbsent);
    }

    #[test]
    fn the_boot_volume_is_refused() {
        let uuid = VolumeUuid::parse(UUID).expect("a test UUID");
        let mut boot = identity(Some(UUID), "/");
        boot.boot_volume = true;
        let facts = FakeFacts(Ok(boot));
        let mounts = FakeMounts(PathBuf::from("/"));
        let error = verify_volume(&facts, &mounts, Path::new("/Users"), &expected(&uuid, None))
            .expect_err("the boot volume is never managed");
        assert_eq!(error.kind(), ErrorKind::RefusedBoundary);
    }

    #[test]
    fn a_non_apfs_volume_is_refused() {
        let uuid = VolumeUuid::parse(UUID).expect("a test UUID");
        let mut other = identity(Some(UUID), "/Volumes/USB");
        other.filesystem = FileSystem::Other("exfat".to_owned());
        let facts = FakeFacts(Ok(other));
        let mounts = FakeMounts(PathBuf::from("/Volumes/USB"));
        let error = verify_volume(
            &facts,
            &mounts,
            Path::new("/Volumes/USB"),
            &expected(&uuid, None),
        )
        .expect_err("only APFS is managed");
        assert_eq!(error.kind(), ErrorKind::VolumeMismatch);
        assert_eq!(error.detail()["filesystem"], "exfat");
    }

    #[test]
    fn re_verification_refuses_a_changed_volume() {
        let uuid = VolumeUuid::parse(UUID).expect("a test UUID");
        let matching = FakeFacts(Ok(identity(Some(UUID), "/Volumes/Builds")));
        let mounts = FakeMounts(PathBuf::from("/Volumes/Builds"));
        let verified = verify_volume(
            &matching,
            &mounts,
            Path::new("/Volumes/Builds"),
            &expected(&uuid, None),
        )
        .expect("the facts match");
        verified
            .reverify(&matching)
            .expect("the same facts verify again");

        let changed = FakeFacts(Ok(identity(Some(OTHER_UUID), "/Volumes/Builds")));
        let error = verified
            .reverify(&changed)
            .expect_err("a changed volume is refused");
        assert_eq!(error.kind(), ErrorKind::VolumeMismatch);
    }
}
