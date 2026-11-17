//! APFS volume identity on macOS: `getattrlist` with `ATTR_VOL_UUID`.

#![allow(unsafe_code)]

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use crate::error::{Error, Result};

use super::canonical_uuid;

/// The volume UUID that owns `mount`.
///
/// `Ok(None)` means the fact could not be read: the path is absent, or the
/// caller may not ask. Barge treats that as "unknown", never as a match.
pub fn volume_uuid(mount: &Path) -> Result<Option<String>> {
    let c_path = CString::new(mount.as_os_str().as_bytes())
        .map_err(|_| Error::usage(format!("path {} contains a NUL byte", mount.display())))?;
    let attributes = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: 0,
        volattr: libc::ATTR_VOL_UUID,
        dirattr: 0,
        fileattr: 0,
        forkattr: 0,
    };
    let mut buffer = [0_u8; 40];
    // SAFETY: `getattrlist` writes at most `buffer.len()` bytes into `buffer`,
    // reads one `attrlist` from `attributes`, and takes a NUL-terminated path
    // that outlives the call.
    let result = unsafe {
        libc::getattrlist(
            c_path.as_ptr(),
            &attributes as *const libc::attrlist as *mut libc::c_void,
            buffer.as_mut_ptr().cast::<libc::c_void>(),
            buffer.len(),
            0,
        )
    };
    if result != 0 {
        let source = std::io::Error::last_os_error();
        return match source.raw_os_error() {
            Some(libc::ENOENT) | Some(libc::ENOTDIR) | Some(libc::EACCES) | Some(libc::EPERM) => {
                Ok(None)
            }
            _ => Err(Error::io(
                "cannot read the volume identity at",
                mount,
                &source,
            )),
        };
    }
    // The returned attribute buffer starts with its total length, then the
    // 16-byte UUID for `ATTR_VOL_UUID`.
    let length = u32::from_ne_bytes([buffer[0], buffer[1], buffer[2], buffer[3]]);
    if length < 20 {
        return Ok(None);
    }
    let mut uuid = [0_u8; 16];
    uuid.copy_from_slice(&buffer[4..20]);
    if uuid.iter().all(|byte| *byte == 0) {
        return Ok(None);
    }
    let text = format!(
        "{:02X}{:02X}{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
        uuid[0],
        uuid[1],
        uuid[2],
        uuid[3],
        uuid[4],
        uuid[5],
        uuid[6],
        uuid[7],
        uuid[8],
        uuid[9],
        uuid[10],
        uuid[11],
        uuid[12],
        uuid[13],
        uuid[14],
        uuid[15]
    );
    Ok(canonical_uuid(&text).ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn a_missing_path_has_no_identity() {
        let path = PathBuf::from("/nonexistent-barge-identity-probe");
        assert_eq!(volume_uuid(&path).ok(), Some(None));
    }
}
