//! Removing a tree without following a symlink.
//!
//! Every step is descriptor-relative: the folder is opened with `O_NOFOLLOW`,
//! each entry is classified with `fstatat(.., AT_SYMLINK_NOFOLLOW)`, each child
//! folder is opened from its parent's descriptor, and each removal is an
//! `unlinkat` on that descriptor. A process racing inside the tree therefore
//! cannot swap a folder for a symlink between the check and the descent: the
//! descriptor names the folder the check was made on.

#![allow(unsafe_code)]

use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs::File;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// The deepest tree a walk descends before it gives up on an entry. Measuring a
/// root and deleting it use the same cap, so a size never counts a tree the
/// deletion would refuse to reach.
pub const MAX_DEPTH: usize = 128;

/// What one no-follow tree removal did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Removed {
    /// The bytes of the regular files it removed.
    pub bytes: u64,
    /// Entries left in place: another filesystem, or a tree that is too deep.
    pub skipped: Vec<PathBuf>,
    /// True when the root folder itself was removed.
    pub removed: bool,
}

/// Remove `root` and the tree under it without following a symlink and without
/// crossing into another filesystem.
pub(crate) fn remove_tree(root: &Path) -> Result<Removed> {
    walk(root, &same_device)
}

/// The testable core of [`remove_tree`]: `device` answers which filesystem an
/// entry is on, so a test can claim a nested folder is another mount.
#[cfg(test)]
pub(crate) fn remove_tree_with(
    root: &Path,
    device: &dyn Fn(&OsStr, u64) -> u64,
) -> Result<Removed> {
    walk(root, device)
}

type DeviceFn<'a> = &'a dyn Fn(&OsStr, u64) -> u64;

fn same_device(_name: &OsStr, device: u64) -> u64 {
    device
}

fn walk(root: &Path, device: DeviceFn<'_>) -> Result<Removed> {
    let directory = open_at(None, root.as_os_str())
        .map_err(|source| Error::io("cannot open", root, &source))?;
    let root_device = directory
        .metadata()
        .map_err(|source| Error::io("cannot inspect", root, &source))?
        .dev();
    let mut removed = Removed::default();
    if remove_contents(&directory, root, root_device, 0, device, &mut removed)? {
        std::fs::remove_dir(root).map_err(|source| Error::io("cannot remove", root, &source))?;
        removed.removed = true;
    } else {
        // A nested mount or an over-deep tree stays: Barge never reaches across.
        removed.skipped.push(root.to_path_buf());
    }
    Ok(removed)
}

fn remove_contents(
    directory: &File,
    path: &Path,
    root_device: u64,
    depth: usize,
    device: DeviceFn<'_>,
    removed: &mut Removed,
) -> Result<bool> {
    if depth >= MAX_DEPTH {
        removed.skipped.push(path.to_path_buf());
        return Ok(false);
    }
    let entries =
        read_names(directory).map_err(|source| Error::io("cannot list", path, &source))?;
    let mut empty = true;
    for name in entries {
        let child = path.join(&name);
        let metadata = match stat_at(directory, &name) {
            Ok(metadata) => metadata,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => continue,
            Err(source) => return Err(Error::io("cannot inspect", &child, &source)),
        };
        let kind = metadata.st_mode & libc::S_IFMT;
        if kind == libc::S_IFLNK {
            unlink_at(directory, &name, 0)
                .map_err(|source| Error::io("cannot remove a symlink", &child, &source))?;
            continue;
        }
        if kind != libc::S_IFDIR {
            removed.bytes = removed
                .bytes
                .saturating_add(u64::try_from(metadata.st_size).unwrap_or(0));
            unlink_at(directory, &name, 0)
                .map_err(|source| Error::io("cannot remove", &child, &source))?;
            continue;
        }
        if device(&name, u64::try_from(metadata.st_dev).unwrap_or(0)) != root_device {
            removed.skipped.push(child);
            empty = false;
            continue;
        }
        let nested = open_at(Some(directory), &name)
            .map_err(|source| Error::io("cannot open", &child, &source))?;
        if remove_contents(&nested, &child, root_device, depth + 1, device, removed)? {
            unlink_at(directory, &name, libc::AT_REMOVEDIR)
                .map_err(|source| Error::io("cannot remove", &child, &source))?;
        } else {
            empty = false;
        }
    }
    Ok(empty)
}

/// The names in an open folder. A read error mid-stream is not distinguished
/// from the end of the stream: `errno` access is platform-specific, and a missed
/// entry leaves the folder non-empty, so the caller fails loudly on a removal
/// instead of claiming a folder it did not empty.
fn read_names(directory: &File) -> std::io::Result<Vec<OsString>> {
    // `fdopendir` takes ownership of the descriptor it is given, so hand it a
    // duplicate and keep the caller's.
    // SAFETY: `dup` takes a descriptor this process holds and returns a new one
    // or -1, which is reported.
    let duplicate = unsafe { libc::dup(directory.as_raw_fd()) };
    if duplicate < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `fdopendir` takes ownership of the duplicate descriptor; a null
    // return leaves it ours.
    let stream = unsafe { libc::fdopendir(duplicate) };
    if stream.is_null() {
        let source = std::io::Error::last_os_error();
        // SAFETY: no stream was created, so the duplicate is still ours to close.
        unsafe { libc::close(duplicate) };
        return Err(source);
    }
    let mut names = Vec::new();
    loop {
        // SAFETY: `readdir` reads from a stream `fdopendir` returned and gives
        // either null or a pointer to an entry the stream owns until the next call.
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            break;
        }
        // SAFETY: the entry pointer is non-null and its name is NUL-terminated.
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        let bytes = name.to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        names.push(OsStr::from_bytes(bytes).to_os_string());
    }
    // SAFETY: the stream came from `fdopendir`, is closed once here, and closes
    // the duplicate descriptor with it.
    unsafe { libc::closedir(stream) };
    Ok(names)
}

/// Open one folder, without following a symlink and without waiting on a fifo.
fn open_at(directory: Option<&File>, name: &OsStr) -> std::io::Result<File> {
    let name = c_name(name)?;
    let parent = directory.map_or(libc::AT_FDCWD, AsRawFd::as_raw_fd);
    // SAFETY: `openat` takes a descriptor this process holds (or `AT_FDCWD`) and
    // a NUL-terminated name; a successful call returns a new descriptor.
    let fd = unsafe {
        libc::openat(
            parent,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `openat` returned a descriptor nothing else owns.
    Ok(unsafe { File::from_raw_fd(fd) })
}

/// One entry's status, without following a symlink.
fn stat_at(directory: &File, name: &OsStr) -> std::io::Result<libc::stat> {
    let name = c_name(name)?;
    // SAFETY: `fstatat` writes one `libc::stat` into the buffer we own and reads
    // a descriptor this process holds and a NUL-terminated name.
    let mut status: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers are valid for the call as described above.
    let result = unsafe {
        libc::fstatat(
            directory.as_raw_fd(),
            name.as_ptr(),
            &mut status,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(status)
}

/// Remove one entry of an open folder. A symlink is removed, never followed.
fn unlink_at(directory: &File, name: &OsStr, flags: libc::c_int) -> std::io::Result<()> {
    let name = c_name(name)?;
    // SAFETY: `unlinkat` takes a descriptor this process holds and a
    // NUL-terminated name; it removes the entry named.
    let result = unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), flags) };
    if result != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn c_name(name: &OsStr) -> std::io::Result<CString> {
    CString::new(name.as_bytes()).map_err(|_| std::io::Error::from_raw_os_error(libc::EINVAL))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::root::{RootMarker, ROOT_SCHEMA};

    fn marker(id: &str) -> RootMarker {
        RootMarker {
            schema: ROOT_SCHEMA.to_string(),
            area: "test/area".to_string(),
            id: id.to_string(),
            label: "slot".to_string(),
            created_unix_ms: 1,
            expires_unix_ms: None,
            folder: None,
        }
    }

    #[test]
    fn a_nested_filesystem_is_skipped() {
        let temp = crate::testing::TempDir::new("tree-device").unwrap();
        let root = temp.path().join("slot-0123456789abcdef");
        let nested = root.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        crate::root::write_marker(&root, &marker("0123456789abcdef")).unwrap();
        std::fs::write(nested.join("keep.txt"), b"keep").unwrap();
        std::fs::write(root.join("counted.bin"), vec![b'x'; 8]).unwrap();
        let marker_bytes = std::fs::symlink_metadata(root.join(crate::root::MARKER_NAME))
            .unwrap()
            .len();

        // The fixture cannot mount a second filesystem, so the seam reports one
        // for the folder a mount would appear as.
        let removed = remove_tree_with(&root, &|name, device| {
            if name == "nested" {
                device + 1
            } else {
                device
            }
        })
        .unwrap();

        assert!(
            nested.join("keep.txt").exists(),
            "the nested mount survives"
        );
        assert!(removed.skipped.contains(&nested));
        assert!(removed.skipped.contains(&root));
        assert!(!removed.removed);
        assert_eq!(
            removed.bytes,
            8 + marker_bytes,
            "the counted files were removed"
        );
        assert!(!root.join("counted.bin").exists());
    }

    #[test]
    fn the_walk_stops_at_the_depth_cap() {
        let temp = crate::testing::TempDir::new("tree-depth").unwrap();
        let root = temp.path().join("slot-0123456789abcdef");
        let mut deepest = root.clone();
        std::fs::create_dir_all(&root).unwrap();
        crate::root::write_marker(&root, &marker("0123456789abcdef")).unwrap();
        for _ in 0..MAX_DEPTH + 2 {
            deepest = deepest.join("nested");
            std::fs::create_dir(&deepest).unwrap();
        }
        let removed = remove_tree(&root).unwrap();
        assert!(!removed.skipped.is_empty(), "the deep tree stays in place");
        assert!(!removed.removed);
        assert!(deepest.is_dir());
    }
}
