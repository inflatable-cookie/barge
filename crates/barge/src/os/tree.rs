//! Walking a tree without following a symlink: removal and measurement.
//!
//! Every step is descriptor-relative: the folder is opened with `O_NOFOLLOW`,
//! each entry is classified with `fstatat(.., AT_SYMLINK_NOFOLLOW)`, each child
//! folder is opened from its parent's descriptor, and both the folder's
//! ownership and the filesystem it lives on are read from the descriptor that
//! was opened — never from a path or an entry seen a moment earlier. A process
//! racing inside the tree therefore cannot swap a folder for a symlink, or a
//! mount for a plain folder, between a check and the step that acts on it.

#![allow(unsafe_code)]

use std::ffi::{CString, OsStr, OsString};
use std::fs::File;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

use super::open_directory;

/// The deepest tree a walk descends before it gives up on an entry. Measuring a
/// root and deleting it use the same cap, so a size never counts a tree the
/// deletion would refuse to reach.
pub const MAX_DEPTH: usize = 128;

/// What one no-follow tree removal did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Removed {
    /// The bytes of the regular files it removed.
    pub bytes: u64,
    /// Entries left in place: another filesystem, a tree that is too deep, or a
    /// folder whose path stopped naming it.
    pub skipped: Vec<PathBuf>,
    /// True when the folder this walk was opened on was removed.
    pub removed: bool,
}

/// A measured tree size, and whether the walk saw all of it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct TreeSize {
    pub bytes: u64,
    /// True when part of the tree was not counted, so `bytes` is a lower bound.
    pub approximate: bool,
}

/// A folder opened once, so its ownership, its filesystem and its tree are the
/// same folder. Removal and measurement both walk from this descriptor.
#[derive(Debug)]
pub(crate) struct Tree {
    directory: File,
    path: PathBuf,
}

impl Tree {
    /// Open a folder without following a symlink at it.
    pub(crate) fn open(path: &Path) -> Result<Self> {
        let directory =
            open_directory(path).map_err(|source| Error::io("cannot open", path, &source))?;
        Ok(Self {
            directory,
            path: path.to_path_buf(),
        })
    }

    /// The descriptor the marker is read through and the walk descends.
    pub(crate) fn directory(&self) -> &File {
        &self.directory
    }

    /// The path the folder was opened at, for the reports that name it.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Remove this folder and its tree.
    pub(crate) fn remove(self) -> Result<Removed> {
        remove_opened(self, &same_device)
    }

    /// The testable core of [`Tree::remove`].
    #[cfg(test)]
    pub(crate) fn remove_with(self, device: DeviceFn<'_>) -> Result<Removed> {
        remove_opened(self, device)
    }
}

/// The testable core: `device` answers which filesystem a folder is on, so a
/// test can claim a nested folder is another mount. It is asked with the path
/// and the device of the descriptor that was opened.
type DeviceFn<'a> = &'a dyn Fn(&Path, u64) -> u64;

fn same_device(_path: &Path, device: u64) -> u64 {
    device
}

/// Remove a tree without following a symlink and without crossing into another
/// filesystem.
#[cfg(test)]
pub(crate) fn remove_tree_with(path: &Path, device: DeviceFn<'_>) -> Result<Removed> {
    Tree::open(path)?.remove_with(device)
}

fn remove_opened(tree: Tree, device: DeviceFn<'_>) -> Result<Removed> {
    let Tree { directory, path } = tree;
    let root_device =
        device_of(&directory).map_err(|source| Error::io("cannot inspect", &path, &source))?;
    let mut removed = Removed::default();
    if !remove_contents(&directory, &path, root_device, 0, device, &mut removed)? {
        // A nested mount or an over-deep tree stays: Barge never reaches across.
        removed.skipped.push(path);
        return Ok(removed);
    }
    // The folder is empty. Remove it only while the path still names the folder
    // this walk was opened on.
    let current = super::file_identity(&directory)
        .ok()
        .zip(super::path_identity(&path).ok().flatten())
        .is_some_and(|(opened, named)| opened == named);
    if current {
        std::fs::remove_dir(&path).map_err(|source| Error::io("cannot remove", &path, &source))?;
        removed.removed = true;
    } else {
        removed.skipped.push(path);
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
        let nested = match open_at(Some(directory), &name) {
            Ok(nested) => nested,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => continue,
            Err(source) => return Err(Error::io("cannot open", &child, &source)),
        };
        // The filesystem is read from the descriptor, not from the entry seen a
        // moment ago: a replacement mount cannot be descended.
        let nested_device =
            device_of(&nested).map_err(|source| Error::io("cannot inspect", &child, &source))?;
        if device(&child, nested_device) != root_device {
            removed.skipped.push(child);
            empty = false;
            continue;
        }
        if remove_contents(&nested, &child, root_device, depth + 1, device, removed)? {
            // Remove the folder this walk emptied, and only while the name still
            // means it: a replacement must not lose an empty folder.
            let current = super::file_identity(&nested)
                .ok()
                .zip(super::path_identity(&child).ok().flatten())
                .is_some_and(|(opened, named)| opened == named);
            if current {
                unlink_at(directory, &name, libc::AT_REMOVEDIR)
                    .map_err(|source| Error::io("cannot remove", &child, &source))?;
            } else {
                removed.skipped.push(child);
                empty = false;
            }
        } else {
            empty = false;
        }
    }
    Ok(empty)
}

/// Measure a tree without following a symlink, without crossing into another
/// filesystem and without descending past [`MAX_DEPTH`]. Anything the walk could
/// not see is reported as an approximate size rather than counted as nothing.
pub(crate) fn measure_tree(path: &Path) -> TreeSize {
    measure_opened(path, &same_device)
}

fn measure_opened(path: &Path, device: DeviceFn<'_>) -> TreeSize {
    let mut measured = TreeSize::default();
    let directory = match open_directory(path) {
        Ok(directory) => directory,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            measured.approximate = true;
            return measured;
        }
        Err(_) => {
            // Not a folder: a plain file is counted, anything else is unknown.
            match std::fs::symlink_metadata(path) {
                Ok(metadata) if !metadata.is_dir() && !metadata.file_type().is_symlink() => {
                    measured.bytes = metadata.len();
                }
                _ => measured.approximate = true,
            }
            return measured;
        }
    };
    let root_device = match device_of(&directory) {
        Ok(device) => device,
        Err(_) => {
            measured.approximate = true;
            return measured;
        }
    };
    measure_contents(&directory, path, root_device, 0, device, &mut measured);
    measured
}

fn measure_contents(
    directory: &File,
    path: &Path,
    root_device: u64,
    depth: usize,
    device: DeviceFn<'_>,
    measured: &mut TreeSize,
) {
    if depth >= MAX_DEPTH {
        // Deeper than deletion goes, so the rest is neither counted nor claimed.
        measured.approximate = true;
        return;
    }
    let entries = match read_names(directory) {
        Ok(entries) => entries,
        Err(_) => {
            measured.approximate = true;
            return;
        }
    };
    for name in entries {
        let child = path.join(&name);
        let metadata = match stat_at(directory, &name) {
            Ok(metadata) => metadata,
            Err(_) => {
                measured.approximate = true;
                continue;
            }
        };
        let kind = metadata.st_mode & libc::S_IFMT;
        if kind == libc::S_IFLNK {
            // A symlink is never followed, so its target is not counted.
            continue;
        }
        if kind != libc::S_IFDIR {
            measured.bytes = measured
                .bytes
                .saturating_add(u64::try_from(metadata.st_size).unwrap_or(0));
            continue;
        }
        let nested = match open_at(Some(directory), &name) {
            Ok(nested) => nested,
            Err(_) => {
                measured.approximate = true;
                continue;
            }
        };
        let nested_device = match device_of(&nested) {
            Ok(device) => device,
            Err(_) => {
                measured.approximate = true;
                continue;
            }
        };
        if device(&child, nested_device) != root_device {
            // Another filesystem's data is not Barge's to count.
            continue;
        }
        measure_contents(&nested, &child, root_device, depth + 1, device, measured);
    }
}

fn device_of(directory: &File) -> std::io::Result<u64> {
    Ok(directory.metadata()?.dev())
}

/// The names in an open folder.
///
/// A null `readdir` is end-of-stream only when the thread's `errno` is clear;
/// anything else is a failed enumeration and is returned as an error, so a
/// removal fails loudly and a measurement marks the size approximate rather
/// than reporting a partial sum as exact.
fn read_names(directory: &File) -> std::io::Result<Vec<OsString>> {
    #[cfg(test)]
    if READ_HOOK
        .with(|hooks| hooks.borrow_mut().pop_front())
        .unwrap_or(false)
    {
        return Err(std::io::Error::from_raw_os_error(libc::EIO));
    }
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
        clear_errno();
        // SAFETY: `readdir` reads from a stream `fdopendir` returned and gives
        // either null or a pointer to an entry the stream owns until the next call.
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            let failure = errno();
            // SAFETY: the stream came from `fdopendir` and is closed once here;
            // it closes the duplicate descriptor with it.
            unsafe { libc::closedir(stream) };
            return match failure {
                0 => Ok(names),
                code => Err(std::io::Error::from_raw_os_error(code)),
            };
        }
        // SAFETY: the entry pointer is non-null and its name is NUL-terminated.
        let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) };
        let bytes = name.to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        names.push(OsStr::from_bytes(bytes).to_os_string());
    }
}

/// Clear this thread's `errno`, so a `readdir` that returns null says whether it
/// failed. Platforms without a named cell keep the previous behaviour: a failed
/// enumeration is then caught by the removal that finds a folder still non-empty
/// and by nothing else.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn clear_errno() {
    // SAFETY: `errno_cell` returns a pointer to this thread's errno cell.
    unsafe { *errno_cell() = 0 };
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn clear_errno() {}

/// This thread's `errno`, or zero on a platform that does not name it.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn errno() -> libc::c_int {
    // SAFETY: `errno_cell` returns a pointer to this thread's errno cell.
    unsafe { *errno_cell() }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn errno() -> libc::c_int {
    0
}

#[cfg(target_os = "macos")]
fn errno_cell() -> *mut libc::c_int {
    // SAFETY: `__error` returns this thread's errno cell.
    unsafe { libc::__error() }
}

#[cfg(target_os = "linux")]
fn errno_cell() -> *mut libc::c_int {
    // SAFETY: `__errno_location` returns this thread's errno cell.
    unsafe { libc::__errno_location() }
}

/// Open one folder, without following a symlink at it.
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
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::NotFound {
            return Err(error);
        }
    }
    Ok(())
}

fn c_name(name: &OsStr) -> std::io::Result<CString> {
    CString::new(name.as_bytes()).map_err(|_| std::io::Error::from_raw_os_error(libc::EINVAL))
}

#[cfg(test)]
thread_local! {
    /// A seam for the enumeration failures a temporary folder cannot produce:
    /// each entry says whether that enumeration fails.
    static READ_HOOK: std::cell::RefCell<std::collections::VecDeque<bool>> =
        const { std::cell::RefCell::new(std::collections::VecDeque::new()) };
}

#[cfg(test)]
fn push_read_failure(fail: bool) {
    READ_HOOK.with(|hooks| hooks.borrow_mut().push_back(fail));
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
    fn a_nested_filesystem_is_skipped_by_the_opened_descriptor() {
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
        // for the folder a mount would appear as. The decision is taken on the
        // descriptor that was opened, so a swap between the entry and the open
        // cannot change it.
        let removed = remove_tree_with(&root, &|path, device| {
            if path.ends_with("nested") {
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
    fn an_opened_tree_keeps_the_folder_it_was_opened_on() {
        let temp = crate::testing::TempDir::new("tree-replaced").unwrap();
        let root = temp.path().join("slot-0123456789abcdef");
        std::fs::create_dir(&root).unwrap();
        crate::root::write_marker(&root, &marker("0123456789abcdef")).unwrap();
        std::fs::write(root.join("owned.bin"), b"owned").unwrap();

        let tree = Tree::open(&root).unwrap();
        // A replacement at the path after the open, as a racing writer does.
        let held = temp.path().join("held");
        std::fs::rename(&root, &held).unwrap();
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("replacement.bin"), b"keep me").unwrap();

        assert_eq!(tree.path(), root.as_path(), "the tree names what it opened");
        let removed = tree.remove().unwrap();

        assert!(removed.skipped.contains(&root), "the path changed under it");
        assert!(!removed.removed);
        assert!(
            root.join("replacement.bin").exists(),
            "the replacement was never walked"
        );
        assert!(!held.join("owned.bin").exists(), "the opened tree was");
        assert!(held.is_dir(), "the folder that moved is left empty");
    }

    #[test]
    fn a_failed_enumeration_is_not_an_end_of_stream() {
        let temp = crate::testing::TempDir::new("tree-list").unwrap();
        let root = temp.path().join("slot-0123456789abcdef");
        std::fs::create_dir_all(root.join("nested")).unwrap();
        crate::root::write_marker(&root, &marker("0123456789abcdef")).unwrap();
        std::fs::write(root.join("readable.bin"), vec![b'x'; 6]).unwrap();
        std::fs::write(root.join("nested/other.bin"), vec![b'x'; 32]).unwrap();
        let marker_bytes = std::fs::symlink_metadata(root.join(crate::root::MARKER_NAME))
            .unwrap()
            .len();

        // The root lists, its child folder does not: the sum is a lower bound.
        push_read_failure(false);
        push_read_failure(true);
        let measured = measure_tree(&root);
        assert_eq!(
            measured.bytes,
            6 + marker_bytes,
            "what was listed is counted"
        );
        assert!(
            measured.approximate,
            "a failed enumeration is never called exact"
        );

        // The same failure makes a removal fail loudly instead of claiming a
        // folder it did not empty.
        push_read_failure(false);
        push_read_failure(true);
        let tree = Tree::open(&root).unwrap();
        let error = tree.remove().unwrap_err();
        assert_eq!(error.kind(), crate::error::ErrorKind::Io, "{error}");
        assert!(root.join("nested/other.bin").exists());
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
        let removed = Tree::open(&root).unwrap().remove().unwrap();
        assert!(!removed.skipped.is_empty(), "the deep tree stays in place");
        assert!(!removed.removed);
        assert!(deepest.is_dir());

        // Measuring stops at the same cap, so a size never counts a tree the
        // deletion would refuse to reach.
        let measured = measure_tree(&root);
        assert!(measured.approximate, "an uncounted tree is never exact");
        assert!(
            measured.bytes < 32,
            "the buried file is past the cap: {}",
            measured.bytes
        );
    }
}
