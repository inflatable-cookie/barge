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

use std::fmt;
use std::path::Path;
use std::sync::Arc;

use crate::error::{Error, Result};

pub use fake::FakeOs;

/// The effective user ID for filesystem ownership checks.
pub(crate) fn effective_uid() -> u32 {
    // SAFETY: geteuid has no preconditions and returns the process's effective UID.
    #[allow(unsafe_code)]
    unsafe {
        libc::geteuid()
    }
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
