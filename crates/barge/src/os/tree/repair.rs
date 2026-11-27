//! Permission recovery for the trash removal walk, never measurement or entry removal.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::os::fd::AsRawFd;
#[cfg(target_os = "linux")]
use std::os::fd::FromRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use super::{c_name, open_at, open_directory, read_names, stat_at, unlink_at};

#[derive(Clone, Copy)]
pub(super) enum Recovery {
    None,
    Trash,
}

impl Recovery {
    fn allowed(self, source: &std::io::Error) -> bool {
        matches!(self, Self::Trash)
            && matches!(source.raw_os_error(), Some(libc::EACCES | libc::EPERM))
    }

    pub(super) fn read_names(
        self,
        directory: &File,
        path: &Path,
    ) -> std::io::Result<Vec<OsString>> {
        read_names(directory).or_else(|source| {
            if self.allowed(&source) && repair_opened(directory, path).is_ok() {
                read_names(directory)
            } else {
                Err(source)
            }
        })
    }

    pub(super) fn unlink(
        self,
        directory: &File,
        name: &OsStr,
        flags: libc::c_int,
        path: &Path,
    ) -> std::io::Result<()> {
        unlink_at(directory, name, flags).or_else(|source| {
            if self.allowed(&source)
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

    /// Open a directory, retrying once after repair. None means it was replaced.
    pub(super) fn open_child(
        self,
        parent: &File,
        name: &OsStr,
        expected: &libc::stat,
    ) -> std::io::Result<Option<File>> {
        let nested = match open_at(Some(parent), name) {
            Ok(nested) => nested,
            Err(source)
                if self.allowed(&source) && repair_unopened(parent, name, expected).is_ok() =>
            {
                match open_at(Some(parent), name) {
                    Ok(nested) => nested,
                    Err(source) => {
                        if stat_at(parent, name)
                            .is_ok_and(|current| !same_entry(&current, expected))
                        {
                            return Ok(None);
                        }
                        return Err(source);
                    }
                }
            }
            Err(source) => return Err(source),
        };
        let opened = nested.metadata()?;
        let expected_dev: u64 = expected.st_dev as _;
        let expected_ino: u64 = expected.st_ino as _;
        if opened.dev() != expected_dev || opened.ino() != expected_ino {
            return Ok(None);
        }
        Ok(Some(nested))
    }
}

fn same_entry(current: &libc::stat, expected: &libc::stat) -> bool {
    current.st_dev == expected.st_dev
        && current.st_ino == expected.st_ino
        && current.st_mode & libc::S_IFMT == libc::S_IFDIR
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
    let status_dev: u64 = status.st_dev as _;
    let status_ino: u64 = status.st_ino as _;
    if status.st_mode & libc::S_IFMT != libc::S_IFDIR
        || status.st_uid != effective_uid
        || opened.dev() != status_dev
        || opened.ino() != status_ino
    {
        return Err(std::io::Error::from_raw_os_error(libc::EPERM));
    }
    // SAFETY: the descriptor refers to the checked directory.
    if unsafe { libc::fchmod(directory.as_raw_fd(), (opened.mode() | 0o700) as _) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// macOS cannot open a mode-000 directory for fchmod. A same-user swap between
/// the no-follow stat and fchmodat is a residual window; the subsequent open
/// verifies identity before descending.
#[cfg(target_os = "macos")]
fn repair_unopened(parent: &File, name: &OsStr, expected: &libc::stat) -> std::io::Result<()> {
    // SAFETY: geteuid has no preconditions.
    if expected.st_uid != unsafe { libc::geteuid() } {
        return Err(std::io::Error::from_raw_os_error(libc::EPERM));
    }
    let name = c_name(name)?;
    // SAFETY: parent and NUL-terminated name are valid; no links are followed.
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
fn repair_unopened(parent: &File, name: &OsStr, expected: &libc::stat) -> std::io::Result<()> {
    let name = c_name(name)?;
    // SAFETY: the parent and name are valid; no symlink is followed.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
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
        || actual.dev() != expected.st_dev
        || actual.ino() != expected.st_ino
    {
        return Err(std::io::Error::from_raw_os_error(libc::EPERM));
    }
    // O_PATH cannot be fchmod'ed. procfs resolves this held descriptor, not
    // the mutable entry name; if procfs is unavailable the repair fails safely.
    let proc_path = c_name(OsStr::new(&format!("/proc/self/fd/{fd}")))?;
    // SAFETY: proc_path is NUL-terminated and fd remains open.
    if unsafe { libc::chmod(proc_path.as_ptr(), actual.mode() | 0o700) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}
