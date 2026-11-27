//! Walking a tree without following a symlink: removal and measurement.
//!
//! Every step is descriptor-relative: the folder is opened with `O_NOFOLLOW`,
//! each entry is classified with `fstatat(.., AT_SYMLINK_NOFOLLOW)`, each child
//! folder is opened from its parent's descriptor, and both the folder's
//! ownership and the filesystem it lives on are read from the descriptor that
//! was opened — never from a path or an entry seen a moment earlier. A process
//! racing inside the tree therefore cannot swap a folder for a symlink, or a
//! mount for a plain folder, between a check and the step that acts on it.
//!
//! A root's marker is removed last, after every other entry and only once
//! nothing else is left, so a walk that stops before that leaves a folder that is
//! still owned trash the next pass can finish. The marker has to go before the
//! folder can be removed, so a walk stopped at that last step leaves an empty
//! folder with no marker: [`Tree::remove_if_empty`] reaps exactly that residue,
//! and never a folder holding an entry — a writer that refills it leaves
//! markerless trash, which the sweep refuses and reports.

#![allow(unsafe_code)]

use std::ffi::{CString, OsStr, OsString};
use std::fs::File;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use crate::error::{Error, Result};
use crate::root::MARKER_NAME;

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

/// What reaping an unmarked, possibly empty folder did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reaped {
    /// The folder is gone: this call removed it, or another sweep already did.
    Removed,
    /// The path no longer names the folder that was opened.
    Replaced,
    /// The folder holds an entry, so it is not the residue a deletion leaves.
    HoldsEntry,
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

    /// Whether the folder holds nothing but entries named in `allowed`, read
    /// through the descriptor it was opened on.
    pub(crate) fn holds_only(&self, allowed: &[&OsStr]) -> Result<bool> {
        let names = read_names(&self.directory)
            .map_err(|source| Error::io("cannot list", &self.path, &source))?;
        Ok(names.iter().all(|name| allowed.contains(&name.as_os_str())))
    }

    /// The path the folder was opened at, for the reports that name it.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Record the name a rename gave this folder, so the removal below removes
    /// it only while that name still means the folder this walk was opened on.
    pub(crate) fn rebased(mut self, path: PathBuf) -> Self {
        self.path = path;
        self
    }

    /// Reap this folder if it holds nothing, and only while the path still names
    /// the folder that was opened.
    ///
    /// `rmdir` is the emptiness test, so the decision and the removal are one
    /// atomic step: an entry a writer adds, before this call or between it and
    /// the removal, makes it refuse rather than be deleted.
    pub(crate) fn remove_if_empty(self) -> Result<Reaped> {
        let Tree { directory, path } = self;
        if !names_opened(&directory, &path) {
            return Ok(Reaped::Replaced);
        }
        match std::fs::remove_dir(&path) {
            Ok(()) => Ok(Reaped::Removed),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(Reaped::Removed),
            // An entry is inside it, so it is not the empty residue a deletion
            // leaves: it is refused, never emptied.
            Err(source) if source.kind() == std::io::ErrorKind::DirectoryNotEmpty => {
                Ok(Reaped::HoldsEntry)
            }
            Err(source) => Err(Error::io("cannot remove", &path, &source)),
        }
    }

    /// Remove this folder and its tree, with the root marker last. An
    /// interruption before its last step leaves a folder that still carries the
    /// marker; stopped at its last step it leaves an empty folder, for the trash
    /// sweep to reap. Only trash deletion calls this: it may restore owner
    /// permissions on directories inside the owned trash tree.
    pub(crate) fn remove(self) -> Result<Removed> {
        remove_opened(self, &same_device)
    }

    /// Remove one relative entry using the same walk and filesystem boundary.
    pub(crate) fn remove_entry(&self, relative: &Path) -> Result<Removed> {
        let root_device = device_of(&self.directory)
            .map_err(|source| Error::io("cannot inspect", &self.path, &source))?;
        let path = self.path.join(relative);
        let mut removed = Removed::default();
        if let Some((directory, name)) = self.entry_parent(relative, root_device)? {
            removed.removed = remove_child(
                &directory,
                name,
                &path,
                (root_device, &same_device),
                0,
                false,
                &mut removed,
            )?;
        } else {
            removed.removed = true; // An absent entry needs no removal.
        }
        Ok(removed)
    }

    /// Open a relative regular file, without traversing symlinks or mounts.
    pub(crate) fn open_file(&self, relative: &Path) -> Result<Option<File>> {
        let root_device = device_of(&self.directory)
            .map_err(|source| Error::io("cannot inspect", &self.path, &source))?;
        let Some((parent, name)) = self.entry_parent(relative, root_device)? else {
            return Ok(None);
        };
        let path = self.path.join(relative);
        let name = name
            .to_str()
            .ok_or_else(|| Error::usage("file name must be UTF-8"))?;
        let file = match super::open_marker_at(&parent, name) {
            Ok(file) => file,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(Error::io("cannot open", &path, &source)),
        };
        let metadata = file
            .metadata()
            .map_err(|source| Error::io("cannot inspect", &path, &source))?;
        if !metadata.is_file() || metadata.dev() != root_device {
            return Err(Error::refused(format!(
                "{} is not a regular file on the checkout filesystem",
                path.display()
            )));
        }
        Ok(Some(file))
    }

    fn entry_parent<'a>(
        &self,
        relative: &'a Path,
        device: u64,
    ) -> Result<Option<(File, &'a OsStr)>> {
        let mut parts = relative.components().peekable();
        let mut path = self.path.clone();
        let mut directory = self
            .directory
            .try_clone()
            .map_err(|source| Error::io("cannot open", &path, &source))?;
        while let Some(Component::Normal(name)) = parts.next() {
            if parts.peek().is_none() {
                return Ok(Some((directory, name)));
            }
            path.push(name);
            directory = match open_at(Some(&directory), name) {
                Ok(directory) => directory,
                Err(source)
                    if matches!(
                        source.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) =>
                {
                    return Ok(None)
                }
                Err(source) => return Err(Error::io("cannot open", &path, &source)),
            };
            if device_of(&directory)
                .map_err(|source| Error::io("cannot inspect", &path, &source))?
                != device
            {
                return Err(Error::refused(format!(
                    "{} is on another filesystem",
                    path.display()
                )));
            }
        }
        Err(Error::usage(format!(
            "{} is not a relative entry path",
            relative.display()
        )))
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
    let marker = OsStr::new(MARKER_NAME);
    let mut removed = Removed::default();
    // Every entry but the root's marker goes first: a nested mount, an over-deep
    // tree or a failed step then leaves the marker in place, so the folder is
    // still owned trash rather than a markerless straggler Barge refuses. The
    // marker goes last; only the empty folder is left after it.
    let entries = read_names_for_removal(&directory, &path, true)
        .map_err(|source| Error::io("cannot list", &path, &source))?;
    let mut empty = true;
    for name in entries.iter().filter(|name| name.as_os_str() != marker) {
        empty &= remove_child(
            &directory,
            name,
            &path.join(name),
            (root_device, device),
            0,
            true,
            &mut removed,
        )?;
    }
    // Something stayed, so the marker must stay too: the folder is still owned
    // trash that the next pass can finish.
    if !empty {
        removed.skipped.push(path);
        return Ok(removed);
    }
    // The folder holds nothing but its marker, and the marker has to go before
    // the folder can: what is left if this step stops is an empty folder, which
    // the trash sweep reaps.
    if !remove_child(
        &directory,
        marker,
        &path.join(marker),
        (root_device, device),
        0,
        true,
        &mut removed,
    )? {
        removed.skipped.push(path);
        return Ok(removed);
    }
    // Remove the empty folder only while the path still names the folder this
    // walk was opened on, checked immediately before the removal.
    if !names_opened(&directory, &path) {
        removed.skipped.push(path);
        return Ok(removed);
    }
    std::fs::remove_dir(&path).map_err(|source| Error::io("cannot remove", &path, &source))?;
    removed.removed = true;
    Ok(removed)
}

/// Whether the path still names the folder this walk was opened on.
fn names_opened(directory: &File, path: &Path) -> bool {
    super::file_identity(directory)
        .ok()
        .zip(super::path_identity(path).ok().flatten())
        .is_some_and(|(opened, named)| opened == named)
}

fn remove_contents(
    directory: &File,
    path: &Path,
    root_device: u64,
    depth: usize,
    device: DeviceFn<'_>,
    repair: bool,
    removed: &mut Removed,
) -> Result<bool> {
    if depth >= MAX_DEPTH {
        removed.skipped.push(path.to_path_buf());
        return Ok(false);
    }
    let entries = read_names_for_removal(directory, path, repair)
        .map_err(|source| Error::io("cannot list", path, &source))?;
    let mut empty = true;
    for name in entries {
        empty &= remove_child(
            directory,
            &name,
            &path.join(&name),
            (root_device, device),
            depth,
            repair,
            removed,
        )?;
    }
    Ok(empty)
}

fn remove_child(
    directory: &File,
    name: &OsStr,
    path: &Path,
    boundary: (u64, DeviceFn<'_>),
    depth: usize,
    repair: bool,
    removed: &mut Removed,
) -> Result<bool> {
    let (root_device, device) = boundary;
    let metadata = match stat_at(directory, name) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(source) => return Err(Error::io("cannot inspect", path, &source)),
    };
    let kind = metadata.st_mode & libc::S_IFMT;
    if kind != libc::S_IFDIR {
        if kind != libc::S_IFLNK {
            removed.bytes = removed.bytes.saturating_add(allocated_bytes(&metadata));
        }
        unlink_for_removal(directory, name, 0, path, repair)
            .map_err(|source| Error::io("cannot remove", path, &source))?;
        return Ok(true);
    }
    if device(path, metadata.st_dev as u64) != root_device {
        removed.skipped.push(path.to_path_buf());
        return Ok(false);
    }
    let mut repaired = false;
    let opened = open_at(Some(directory), name).or_else(|source| {
        if repair && permission_error(&source) {
            if repair_unopened(directory, name, &metadata, root_device, path, device).is_ok() {
                repaired = true;
                open_at(Some(directory), name)
            } else {
                Err(source)
            }
        } else {
            Err(source)
        }
    });
    let nested = match opened {
        Ok(nested) => nested,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(source) => {
            if repaired
                && stat_at(directory, name).is_ok_and(|current| {
                    current.st_dev != metadata.st_dev
                        || current.st_ino != metadata.st_ino
                        || current.st_mode & libc::S_IFMT != libc::S_IFDIR
                })
            {
                removed.skipped.push(path.to_path_buf());
                return Ok(false);
            }
            return Err(Error::io("cannot open", path, &source));
        }
    };
    // A name repaired before opening must still identify the same directory.
    let opened = nested
        .metadata()
        .map_err(|source| Error::io("cannot inspect", path, &source))?;
    if opened.dev() != metadata.st_dev as u64 || opened.ino() != metadata.st_ino as u64 {
        removed.skipped.push(path.to_path_buf());
        return Ok(false);
    }
    // Decide from the opened descriptor, so a replacement mount is left alone.
    let nested_device =
        device_of(&nested).map_err(|source| Error::io("cannot inspect", path, &source))?;
    if device(path, nested_device) != root_device {
        removed.skipped.push(path.to_path_buf());
        return Ok(false);
    }
    if !remove_contents(
        &nested,
        path,
        root_device,
        depth + 1,
        device,
        repair,
        removed,
    )? {
        return Ok(false);
    }
    // Remove this folder only while its name still identifies the opened one.
    let current = super::file_identity(&nested)
        .ok()
        .zip(super::path_identity(path).ok().flatten())
        .is_some_and(|(opened, named)| opened == named);
    if !current {
        removed.skipped.push(path.to_path_buf());
        return Ok(false);
    }
    unlink_for_removal(directory, name, libc::AT_REMOVEDIR, path, repair)
        .map_err(|source| Error::io("cannot remove", path, &source))?;
    Ok(true)
}

// Permission recovery is used only by the trash removal walk. Each opened
// directory is checked against its no-follow entry before its mode is changed.
fn permission_error(error: &std::io::Error) -> bool {
    matches!(error.raw_os_error(), Some(libc::EACCES | libc::EPERM))
}

fn repair_opened(directory: &File, path: &Path) -> std::io::Result<()> {
    let parent = open_directory(
        path.parent()
            .ok_or_else(|| std::io::Error::from_raw_os_error(libc::EINVAL))?,
    )?;
    let status = stat_at(
        &parent,
        path.file_name()
            .ok_or_else(|| std::io::Error::from_raw_os_error(libc::EINVAL))?,
    )?;
    let opened = directory.metadata()?;
    // SAFETY: geteuid has no preconditions.
    let effective_uid = unsafe { libc::geteuid() };
    if status.st_mode & libc::S_IFMT != libc::S_IFDIR
        || status.st_uid != effective_uid
        || opened.dev() != status.st_dev as u64
        || opened.ino() != status.st_ino as u64
    {
        return Err(std::io::Error::from_raw_os_error(libc::EPERM));
    }
    // SAFETY: the descriptor refers to the checked directory.
    if unsafe { libc::fchmod(directory.as_raw_fd(), status.st_mode | 0o700) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn read_names_for_removal(
    directory: &File,
    path: &Path,
    repair: bool,
) -> std::io::Result<Vec<OsString>> {
    read_names(directory).or_else(|source| {
        if repair && permission_error(&source) && repair_opened(directory, path).is_ok() {
            read_names(directory)
        } else {
            Err(source)
        }
    })
}

fn unlink_for_removal(
    directory: &File,
    name: &OsStr,
    flags: libc::c_int,
    path: &Path,
    repair: bool,
) -> std::io::Result<()> {
    unlink_at(directory, name, flags).or_else(|source| {
        if repair
            && permission_error(&source)
            && path
                .parent()
                .is_some_and(|parent| repair_opened(directory, parent).is_ok())
        {
            unlink_at(directory, name, flags)
        } else {
            Err(source)
        }
    })
}

// Open without read permission and pin the inode before changing its mode.
fn repair_unopened(
    parent: &File,
    name: &OsStr,
    expected: &libc::stat,
    root_device: u64,
    path: &Path,
    device: DeviceFn<'_>,
) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    let flags = libc::O_PATH;
    #[cfg(target_os = "macos")]
    {
        let _ = (root_device, path, device);
        // SAFETY: geteuid has no preconditions; fchmodat does not follow links.
        if expected.st_uid != unsafe { libc::geteuid() } {
            return Err(std::io::Error::from_raw_os_error(libc::EPERM));
        }
        let name = c_name(name)?;
        // SAFETY: parent and NUL-terminated name are valid.
        if unsafe {
            libc::fchmodat(
                parent.as_raw_fd(),
                name.as_ptr(),
                expected.st_mode | 0o700,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(target_os = "linux")]
    {
        let name = c_name(name)?;
        // SAFETY: the parent and name are valid; no symlink is followed.
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                flags | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: openat returned a new descriptor.
        let pinned = unsafe { File::from_raw_fd(fd) };
        let actual = pinned.metadata()?;
        // SAFETY: geteuid has no preconditions.
        if expected.st_uid != unsafe { libc::geteuid() }
            || actual.uid() != expected.st_uid
            || actual.dev() != expected.st_dev as u64
            || actual.ino() != expected.st_ino as u64
            || device(path, actual.dev()) != root_device
        {
            return Err(std::io::Error::from_raw_os_error(libc::EPERM));
        }
        // O_PATH cannot be fchmod'ed. procfs resolves this held descriptor, not
        // the mutable entry name, and thus cannot redirect to a replacement.
        let proc_path = CString::new(format!("/proc/self/fd/{fd}")).unwrap();
        // SAFETY: proc_path is NUL-terminated and fd remains open.
        if unsafe { libc::chmod(proc_path.as_ptr(), actual.mode() | 0o700) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
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
                    measured.bytes = metadata.blocks().saturating_mul(512);
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
            measured.bytes = measured.bytes.saturating_add(allocated_bytes(&metadata));
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

/// The bytes an entry occupies: its allocated 512-byte blocks, so a sparse or
/// copy-on-write cloned file is counted at what it costs, not at its apparent
/// length. `st_blocks` is always in 512-byte units, whatever the filesystem's
/// block size, so a VM disk image does not overstate a soft cap or freed bytes.
fn allocated_bytes(status: &libc::stat) -> u64 {
    u64::try_from(status.st_blocks)
        .unwrap_or(0)
        .saturating_mul(512)
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
    // A retry after a partial read must start at the beginning; dup shares offsets.
    // SAFETY: stream is a valid directory stream.
    unsafe { libc::rewinddir(stream) };
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
    use std::os::unix::fs::MetadataExt;

    /// The bytes a file is charged for, as the walk counts them.
    fn charged(path: &Path) -> u64 {
        std::fs::symlink_metadata(path).unwrap().blocks() * 512
    }

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
    fn trash_removal_repairs_owned_directories_without_following_links() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let temp = crate::testing::TempDir::new("tree-permissions").unwrap();
        let root = temp.path().join("trash-entry");
        let outside = temp.path().join("outside");
        for name in ["locked", "readonly", "unreadable"] {
            std::fs::create_dir_all(root.join(name)).unwrap();
            std::fs::write(root.join(name).join("file"), b"data").unwrap();
        }
        std::fs::create_dir(&outside).unwrap();
        std::fs::set_permissions(outside.as_path(), std::fs::Permissions::from_mode(0o000))
            .unwrap();
        symlink(&outside, root.join("link")).unwrap();
        std::fs::set_permissions(
            root.join("readonly/file"),
            std::fs::Permissions::from_mode(0o444),
        )
        .unwrap();
        for (name, mode) in [("locked", 0), ("readonly", 0o555), ("unreadable", 0o300)] {
            std::fs::set_permissions(root.join(name), std::fs::Permissions::from_mode(mode))
                .unwrap();
        }
        let result = Tree::open(&root).unwrap().remove();
        let outside_mode = std::fs::symlink_metadata(&outside).unwrap().mode() & 0o777;
        // Restore surviving fixtures even if removal failed, before asserting.
        std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o700)).unwrap();
        for name in ["locked", "readonly", "unreadable"] {
            let path = root.join(name);
            if path.exists() {
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
            }
        }
        assert!(result.unwrap().removed);
        assert!(!root.exists());
        assert_eq!(outside_mode, 0);
    }

    #[test]
    fn entry_removal_does_not_repair_permissions() {
        use std::os::unix::fs::PermissionsExt;
        if crate::os::running_as_root() {
            return;
        }
        let temp = crate::testing::TempDir::new("tree-entry-permissions").unwrap();
        let root = temp.folder("root").unwrap();
        let nested = root.join("readonly");
        std::fs::create_dir(&nested).unwrap();
        std::fs::write(nested.join("file"), b"data").unwrap();
        std::fs::set_permissions(&nested, std::fs::Permissions::from_mode(0o555)).unwrap();
        let result = Tree::open(&root)
            .unwrap()
            .remove_entry(Path::new("readonly"));
        let mode = std::fs::symlink_metadata(&nested).unwrap().mode() & 0o777;
        std::fs::set_permissions(&nested, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(result.is_err());
        assert_eq!(mode, 0o555);
        assert!(nested.join("file").exists());
    }

    #[test]
    fn a_marker_named_file_below_the_root_is_removed_in_ordinary_order() {
        let temp = crate::testing::TempDir::new("tree-nested-marker").unwrap();
        let root = temp.path().join("slot-0123456789abcdef");
        std::fs::create_dir_all(root.join("build/deep")).unwrap();
        std::fs::create_dir_all(root.join("mount")).unwrap();
        crate::root::write_marker(&root, &marker("0123456789abcdef")).unwrap();
        // A consumer's own files that carry the marker's name are not the root's
        // marker: only the top-level entry is held back.
        std::fs::write(root.join("build").join(MARKER_NAME), b"not ours").unwrap();
        std::fs::write(root.join("build/deep").join(MARKER_NAME), b"nor this one").unwrap();

        // The seam reports a top-level folder as another filesystem, so the walk
        // stops with the root's marker in place and the rest of the tree walked.
        let removed = remove_tree_with(&root, &|path, device| {
            if path.ends_with("mount") {
                device + 1
            } else {
                device
            }
        })
        .unwrap();

        assert!(!removed.removed);
        assert!(
            root.join(MARKER_NAME).exists(),
            "the root's marker is held back"
        );
        assert!(
            !root.join("build").join(MARKER_NAME).exists(),
            "a deeper file of the same name goes in listing order"
        );
        assert!(!root.join("build/deep").join(MARKER_NAME).exists());
        assert!(!root.join("build").exists());
        assert!(root.join("mount").is_dir(), "the skipped folder stays");
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
        let counted_bytes = charged(&root.join("counted.bin"));

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
            removed.bytes, counted_bytes,
            "only the file that was removed is counted"
        );
        assert!(!root.join("counted.bin").exists());
        assert!(
            root.join(crate::root::MARKER_NAME).exists(),
            "the marker goes last, so a walk that stopped keeps it"
        );
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
        assert!(
            held.is_dir() && std::fs::read_dir(&held).unwrap().next().is_none(),
            "the folder that moved is left empty and is reaped by the trash sweep"
        );
    }

    /// Deny a folder's own removal, which is decided by the mode of its parent,
    /// while leaving its entries removable.
    fn deny_parent_writes(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o555)).unwrap();
    }

    fn allow_writes(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn a_final_step_that_cannot_remove_the_folder_leaves_an_empty_folder() {
        if crate::os::running_as_root() {
            // Root ignores the mode that makes the last step fail.
            return;
        }
        let temp = crate::testing::TempDir::new("tree-final-failure").unwrap();
        let trash = temp.folder("trash").unwrap();
        let root = trash.join("slot-0123456789abcdef");
        std::fs::create_dir_all(root.join("build")).unwrap();
        crate::root::write_marker(&root, &marker("0123456789abcdef")).unwrap();
        std::fs::write(root.join("build/artifact.bin"), b"artifact").unwrap();
        // The walk can empty the folder, but its parent refuses the folder's own
        // removal, so it fails at its very last step.
        deny_parent_writes(&trash);

        let error = Tree::open(&root).unwrap().remove().unwrap_err();

        allow_writes(&trash);
        assert_eq!(error.kind(), crate::error::ErrorKind::Io, "{error}");
        assert!(!root.join("build").exists(), "the tree was walked");
        assert!(
            !root.join(crate::root::MARKER_NAME).exists(),
            "the marker went before the folder, so the last step leaves no marker"
        );
        assert!(
            std::fs::read_dir(&root).unwrap().next().is_none(),
            "and nothing else: the residue is only ever an empty folder"
        );

        // That residue is exactly what the trash sweep reaps.
        assert_eq!(
            Tree::open(&root).unwrap().remove_if_empty().unwrap(),
            Reaped::Removed
        );
        assert!(!root.exists());
    }

    #[test]
    fn an_empty_folder_is_reaped_but_a_filled_one_is_never_emptied() {
        let temp = crate::testing::TempDir::new("tree-reap").unwrap();
        let empty = temp.folder("empty-leftover").unwrap();
        assert_eq!(
            Tree::open(&empty).unwrap().remove_if_empty().unwrap(),
            Reaped::Removed
        );
        assert!(!empty.exists());

        let filled = temp.folder("filled-leftover").unwrap();
        std::fs::write(filled.join("keep.txt"), b"keep").unwrap();
        assert_eq!(
            Tree::open(&filled).unwrap().remove_if_empty().unwrap(),
            Reaped::HoldsEntry,
            "a folder holding an entry is not the residue"
        );
        assert!(filled.join("keep.txt").exists());
    }

    #[test]
    fn a_reaped_folder_is_the_one_that_was_opened() {
        let temp = crate::testing::TempDir::new("tree-reap-moved").unwrap();
        let path = temp.folder("leftover").unwrap();
        let tree = Tree::open(&path).unwrap();
        // A replacement at the path after the open, as a racing writer does.
        let held = temp.path().join("held");
        std::fs::rename(&path, &held).unwrap();
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("replacement.bin"), b"keep me").unwrap();

        assert_eq!(
            tree.remove_if_empty().unwrap(),
            Reaped::Replaced,
            "another folder is at the path now"
        );
        assert!(path.join("replacement.bin").exists());
        assert!(
            held.is_dir(),
            "the folder that moved was not removed by path"
        );
    }

    #[test]
    fn a_failed_enumeration_is_not_an_end_of_stream() {
        let temp = crate::testing::TempDir::new("tree-list").unwrap();
        let root = temp.path().join("slot-0123456789abcdef");
        std::fs::create_dir_all(root.join("nested")).unwrap();
        crate::root::write_marker(&root, &marker("0123456789abcdef")).unwrap();
        std::fs::write(root.join("readable.bin"), vec![b'x'; 6]).unwrap();
        std::fs::write(root.join("nested/other.bin"), vec![b'x'; 32]).unwrap();
        let marker_bytes = charged(&root.join(crate::root::MARKER_NAME));
        let readable_bytes = charged(&root.join("readable.bin"));

        // The root lists, its child folder does not: the sum is a lower bound.
        push_read_failure(false);
        push_read_failure(true);
        let measured = measure_tree(&root);
        assert_eq!(
            measured.bytes,
            readable_bytes + marker_bytes,
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
        assert!(
            root.join(crate::root::MARKER_NAME).exists(),
            "a walk that failed keeps the marker"
        );
    }

    #[test]
    fn the_walk_stops_at_the_depth_cap() {
        let temp = crate::testing::TempDir::new("tree-depth").unwrap();
        let root = temp.path().join("slot-0123456789abcdef");
        let mut deepest = root.clone();
        std::fs::create_dir_all(&root).unwrap();
        crate::root::write_marker(&root, &marker("0123456789abcdef")).unwrap();
        for _ in 0..MAX_DEPTH + 2 {
            deepest = deepest.join("n");
            std::fs::create_dir(&deepest).unwrap();
        }
        let removed = Tree::open(&root).unwrap().remove().unwrap();
        assert!(!removed.skipped.is_empty(), "the deep tree stays in place");
        assert!(!removed.removed);
        assert!(deepest.is_dir());
        assert!(
            root.join(crate::root::MARKER_NAME).exists(),
            "the folder past the cap keeps the marker that still owns it"
        );

        // Measuring stops at the same cap, so a size never counts a tree the
        // deletion would refuse to reach: only the marker above the cap, which
        // the walk kept, is charged.
        let measured = measure_tree(&root);
        assert!(measured.approximate, "an uncounted tree is never exact");
        assert_eq!(
            measured.bytes,
            charged(&root.join(crate::root::MARKER_NAME)),
            "the tree past the cap is past the cap for measuring too"
        );
    }
}
