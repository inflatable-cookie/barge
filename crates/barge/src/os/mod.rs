//! The small platform seam: volume identity at a mount and free space. Nothing
//! else in the crate names a syscall, errno or platform path convention.
//!
//! Locks, renames and markers are portable; a new platform is a new identity and
//! free-space implementation here, not a redesign.

#[cfg(not(unix))]
compile_error!("Barge supports unix platforms; macOS is first and Linux follows.");

pub mod fake;

#[cfg(target_os = "macos")]
mod macos;
/// Walks that never follow a symlink: `openat`, `fstatat`, `unlinkat`.
pub(crate) mod tree;

use std::fmt;
use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use crate::error::{Error, Result};

pub use fake::FakeOs;
pub(crate) use tree::{measure_tree, Removed, Tree, TreeSize};

/// The filesystem and inode an open descriptor names.
pub(crate) fn file_identity(file: &File) -> std::io::Result<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let metadata = file.metadata()?;
    Ok((metadata.dev(), metadata.ino()))
}

/// The filesystem and inode a path names right now, or `None` when it is gone.
pub(crate) fn path_identity(path: &Path) -> std::io::Result<Option<(u64, u64)>> {
    use std::os::unix::fs::MetadataExt;
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some((metadata.dev(), metadata.ino()))),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(source),
    }
}

/// The effective user ID for filesystem ownership checks.
pub(crate) fn effective_uid() -> u32 {
    // SAFETY: geteuid has no preconditions and returns the process's effective UID.
    #[allow(unsafe_code)]
    unsafe {
        libc::geteuid()
    }
}

/// Open a marker without following a symlink at either the root or marker.
pub(crate) fn open_marker(folder: &Path, name: &str) -> std::io::Result<File> {
    let directory = open_directory(folder)?;
    open_marker_at(&directory, name)
}

/// Open a folder without following a symlink at it.
pub(crate) fn open_directory(folder: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;

    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(folder)
}

/// Open a file inside an open folder, without following a symlink at it.
pub(crate) fn open_marker_at(directory: &File, name: &str) -> std::io::Result<File> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd};

    let name = CString::new(name).map_err(|_| std::io::ErrorKind::InvalidInput)?;
    // SAFETY: the directory descriptor and NUL-terminated name are valid;
    // a successful openat returns a new descriptor owned by File.
    #[allow(unsafe_code)]
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    #[allow(unsafe_code)]
    // SAFETY: openat returned a valid new descriptor, now owned by File.
    Ok(unsafe { std::fs::File::from_raw_fd(fd) })
}

/// The platform facts Barge needs. Tests replace this with [`FakeOs`].
pub trait Os: fmt::Debug + Send + Sync {
    /// The volume UUID that owns `mount`, or `None` when it cannot be read.
    fn volume_identity(&self, mount: &Path) -> Result<Option<String>>;

    /// Free bytes available to the current user at `path`.
    fn free_bytes(&self, path: &Path) -> Result<u64>;
}

/// The platform implementation: APFS volume identity and `statvfs` free space.
#[derive(Debug, Default, Clone, Copy)]
pub struct PlatformOs;

/// The real platform seam.
pub fn platform() -> Arc<dyn Os> {
    Arc::new(PlatformOs)
}

impl Os for PlatformOs {
    fn volume_identity(&self, mount: &Path) -> Result<Option<String>> {
        if let Some(uuid) = test_identity(mount) {
            return Ok(Some(uuid));
        }
        #[cfg(target_os = "macos")]
        {
            macos::volume_uuid(mount)
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = mount;
            Ok(None)
        }
    }

    fn free_bytes(&self, path: &Path) -> Result<u64> {
        free_space(path)
    }
}

/// The environment variable that replaces the real volume identity.
///
/// This is Barge's only test affordance in the shipped binary: the CLI contract
/// tests register a fixture folder, so they set this to a fixed UUID instead of
/// depending on the host's own volume. It exists because a platform whose real
/// identity Barge cannot read yet (Linux) would otherwise have no CLI contract
/// coverage at all. Nothing in Barge sets it.
const TEST_IDENTITY_VAR: &str = "BARGE_TEST_VOLUME_UUID";

/// The fake identity, when the test hook is set and the path is a temporary one.
///
/// Only a path inside the process's own temporary directory is affected, so the
/// hook can never name a registered volume on a real drive. A value that is not
/// a canonical UUID is ignored rather than refused: this hook only stands in for
/// a fact the platform could not supply, and a real run never has it.
fn test_identity(mount: &Path) -> Option<String> {
    let value = std::env::var_os(TEST_IDENTITY_VAR)?;
    let value = value.to_str()?.trim();
    if value.is_empty() {
        return None;
    }
    let temporary = std::fs::canonicalize(std::env::temp_dir()).ok()?;
    let candidate = std::fs::canonicalize(mount).ok()?;
    if !candidate.starts_with(&temporary) {
        return None;
    }
    canonical_uuid(value).ok()
}

/// Normalise a volume UUID to the canonical uppercase 8-4-4-4-12 form.
///
/// A UUID that Barge cannot name exactly is refused: an identity Barge guessed
/// is not an identity.
pub fn canonical_uuid(text: &str) -> Result<String> {
    let trimmed = text.trim();
    let bytes = trimmed.as_bytes();
    let valid = bytes.len() == 36
        && bytes.iter().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                *byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        });
    if !valid {
        return Err(Error::usage(format!(
            "volume UUID {trimmed:?} is not a 36-character 8-4-4-4-12 UUID"
        )));
    }
    Ok(trimmed.to_ascii_uppercase())
}

#[cfg(unix)]
#[allow(unsafe_code)] // The two `statvfs` calls below carry their own SAFETY notes.
fn free_space(path: &Path) -> Result<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| Error::usage(format!("path {} contains a NUL byte", path.display())))?;
    // SAFETY: `statvfs` writes one `libc::statvfs` into the buffer we own, and
    // the path is a valid NUL-terminated C string for the call's duration.
    let mut status: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers are valid for the call as described above.
    let result = unsafe { libc::statvfs(c_path.as_ptr(), &mut status) };
    if result != 0 {
        return Err(Error::io(
            "cannot read free space at",
            path,
            &std::io::Error::last_os_error(),
        ));
    }
    let free = u128::from(status.f_bavail).saturating_mul(u128::from(status.f_frsize));
    Ok(u64::try_from(free).unwrap_or(u64::MAX))
}

use crate::hold::Mode;
use std::os::fd::AsRawFd;

/// Take an advisory lock on a file. `wait` blocks; otherwise the call reports
/// whether the lock was available.
pub(crate) fn flock(file: &File, mode: Mode, wait: bool) -> Result<bool> {
    let mut operation = match mode {
        Mode::Exclusive => libc::LOCK_EX,
        Mode::Shared => libc::LOCK_SH,
    };
    if !wait {
        operation |= libc::LOCK_NB;
    }
    // SAFETY: `flock` takes one descriptor this process owns and one integer of
    // flags; it reads and writes no memory of ours.
    #[allow(unsafe_code)]
    let result = unsafe { libc::flock(file.as_raw_fd(), operation) };
    if result == 0 {
        return Ok(true);
    }
    let source = std::io::Error::last_os_error();
    if !wait && source.kind() == std::io::ErrorKind::WouldBlock {
        return Ok(false);
    }
    Err(Error::new(
        crate::error::ErrorKind::Io,
        format!("cannot take a file lock: {source}"),
    ))
}

pub(crate) fn set_inheritable(file: &File, inheritable: bool) -> Result<()> {
    let descriptor = file.as_raw_fd();
    // SAFETY: both `fcntl` calls take the descriptor this `Hold` owns and one
    // integer; the first reads its flags and the second writes them back.
    #[allow(unsafe_code)]
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
    if flags < 0 {
        return Err(Error::new(
            crate::error::ErrorKind::Io,
            format!(
                "cannot read the hold descriptor flags: {}",
                std::io::Error::last_os_error()
            ),
        ));
    }
    let wanted = if inheritable {
        flags & !libc::FD_CLOEXEC
    } else {
        flags | libc::FD_CLOEXEC
    };
    // SAFETY: as above: one owned descriptor and one integer flag set.
    #[allow(unsafe_code)]
    let result = unsafe { libc::fcntl(descriptor, libc::F_SETFD, wanted) };
    if result < 0 {
        return Err(Error::new(
            crate::error::ErrorKind::Io,
            format!(
                "cannot set the hold descriptor flags: {}",
                std::io::Error::last_os_error()
            ),
        ));
    }
    Ok(())
}

/// Open a persistent lock file without truncating it.
pub(crate) fn open_lock(path: &Path, context: &str) -> Result<File> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|e| Error::io(context, path, &e))
}

#[cfg(test)]
mod tests {
    use super::canonical_uuid;
    #[test]
    fn uuid_is_canonical_and_requires_exact_shape() {
        assert_eq!(
            canonical_uuid("  abcdef01-2345-6789-abcd-ef0123456789  ").unwrap(),
            "ABCDEF01-2345-6789-ABCD-EF0123456789"
        );
        for value in [
            "",
            "abcdef0123456789abcdef0123456789",
            "abcdef01-2345-6789-abcd-ef012345678z",
            "abcdef01_2345-6789-abcd-ef0123456789",
        ] {
            assert!(canonical_uuid(value).is_err(), "{value:?}");
        }
    }
}
