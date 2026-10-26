//! macOS: APFS volume facts and the password database, behind the `os` traits.
//!
//! This module is Barge's only FFI seam. Every `unsafe` block in the crate is
//! here, each one with a safety comment naming the invariant it relies on, and
//! the rest of the crate denies `unsafe_code`. Everything platform-specific —
//! syscalls, errnos, `/Volumes` and the state directory's path — is confined to
//! this file, so no module above `os` can name it.

#![allow(unsafe_code)]

use std::ffi::{CStr, CString, OsStr};
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::error::{Error, ErrorKind, Result};
use crate::os::facts::{
    AccountFacts, DeviceId, FileSystem, MountPoint, MountRoot, VolumeFacts, VolumeIdentity,
    VolumeUuid,
};

/// The parent every managed volume mounts directly under.
const VOLUMES_PARENT: &str = "/Volumes";

/// The account-wide state root, relative to the account home (stage 1 design,
/// "Account-wide state directory").
const STATE_ROOT_RELATIVE: &str = "Library/Application Support/barge/state";

/// The largest password-database buffer Barge will grow to before refusing.
const MAX_PASSWD_BUFFER_BYTES: usize = 64 * 1024;

/// A generous fixed buffer for one `fgetattrlist` volume attribute: the kernel
/// writes a 4-byte length followed by the attribute's value.
const VOLUME_ATTRIBUTE_BYTES: usize = 32;

/// The macOS OS layer.
#[derive(Debug, Clone, Copy, Default)]
pub struct MacOs;

impl MacOs {
    /// The platform implementation behind the `os` traits.
    pub const fn new() -> Self {
        Self
    }
}

impl VolumeFacts for MacOs {
    fn identify(&self, path: &Path) -> Result<VolumeIdentity> {
        let descriptor = open_directory(path)?;
        read_identity(&descriptor, path)
    }
}

impl MountRoot for MacOs {
    fn mount_root(&self, path: &Path) -> Result<MountPoint> {
        let descriptor = open_directory(path)?;
        let identity = read_identity(&descriptor, path)?;
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
        if identity.mount_point != path {
            return Err(Error::new(
                ErrorKind::VolumeMismatch,
                format!(
                    "{} is not a mount root; the kernel reports its mount point as {}",
                    path.display(),
                    identity.mount_point.display()
                ),
            )
            .with_detail("path", path.display().to_string().into())
            .with_detail(
                "actual_mount",
                identity.mount_point.display().to_string().into(),
            ));
        }
        if !is_under_volumes_parent(&identity.mount_point) {
            return Err(Error::new(
                ErrorKind::VolumeMismatch,
                format!(
                    "{} is not directly under {}, so it does not pass the mount-parent rule",
                    path.display(),
                    VOLUMES_PARENT
                ),
            )
            .with_detail("path", path.display().to_string().into())
            .with_detail("expected_mount_parent", VOLUMES_PARENT.into()));
        }
        Ok(MountPoint::new(identity.mount_point))
    }
}

impl AccountFacts for MacOs {
    fn account_home(&self) -> Result<PathBuf> {
        account_home()
    }

    fn state_root(&self) -> Result<PathBuf> {
        Ok(account_home()?.join(STATE_ROOT_RELATIVE))
    }
}

/// Open `path` as a directory, never following a symbolic link at the leaf.
///
/// A path whose volume has vanished fails here as `volume_absent`, which is an
/// error: Barge never reads a vanished volume as "absent and safe".
fn open_directory(path: &Path) -> Result<File> {
    let raw_path = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        Error::new(
            ErrorKind::Usage,
            format!("{} contains a NUL byte", path.display()),
        )
    })?;
    // SAFETY: `raw_path` is a NUL-terminated path that outlives the call, and
    // the flags are libc's own constants. `open` returns either -1 or a fresh
    // descriptor that this function then owns.
    let descriptor = unsafe {
        libc::open(
            raw_path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if descriptor < 0 {
        return Err(open_error(path, io::Error::last_os_error()));
    }
    // SAFETY: `descriptor` is a fresh non-negative descriptor from `open` and no
    // other owner exists, so `File` may take ownership of it.
    Ok(unsafe { File::from_raw_fd(descriptor) })
}

/// Map a failed open onto the contract's kinds: a missing or vanished volume is
/// `volume_absent`, a symbolic link at the leaf is a containment refusal, and
/// anything else is I/O.
fn open_error(path: &Path, error: io::Error) -> Error {
    let kind = match error.raw_os_error() {
        Some(libc::ENOENT | libc::ENOTDIR | libc::ENXIO | libc::EIO | libc::ENODEV) => {
            ErrorKind::VolumeAbsent
        }
        Some(libc::ELOOP) => ErrorKind::RefusedContainment,
        _ => ErrorKind::Io,
    };
    let message = match kind {
        ErrorKind::VolumeAbsent => format!(
            "the volume holding {} is not available: {error}",
            path.display()
        ),
        ErrorKind::RefusedContainment => format!(
            "{} is a symbolic link, so it is not opened as a directory",
            path.display()
        ),
        _ => format!("cannot read {}: {error}", path.display()),
    };
    Error::new(kind, message)
        .with_detail("path", path.display().to_string().into())
        .with_detail("os_error", error.to_string().into())
}

/// Read every fact of the volume that holds the opened directory.
///
/// An APFS volume always has a UUID, so a read that yields none on an APFS
/// volume is an unreadable fact and refuses here. Another filesystem carries no
/// APFS UUID, so it reports `None` and the verification policy refuses it as a
/// filesystem mismatch first.
fn read_identity(descriptor: &File, path: &Path) -> Result<VolumeIdentity> {
    let descriptor_fd = descriptor.as_raw_fd();
    let status = read_statfs(descriptor_fd, path)?;
    let mount_point = read_mount_point(&status, path)?;
    let filesystem = read_filesystem(&status, path)?;
    let volume_uuid = read_volume_uuid(descriptor_fd);
    if volume_uuid.is_none() && filesystem == FileSystem::Apfs {
        return Err(unreadable_fact(path, "volume UUID"));
    }
    Ok(VolumeIdentity {
        filesystem,
        device: read_device(descriptor_fd, path)?,
        volume_uuid,
        boot_volume: is_boot_volume(&mount_point),
        mount_point,
    })
}

/// A failed read of a fact on an otherwise open descriptor: a vanished volume
/// is `volume_absent`, and any other unreadable fact is unknown state.
fn fact_error(path: &Path, fact: &str, error: io::Error) -> Error {
    let kind = match error.raw_os_error() {
        Some(libc::ENOENT | libc::ENXIO | libc::EIO | libc::ENODEV) => ErrorKind::VolumeAbsent,
        _ => ErrorKind::UnknownState,
    };
    Error::new(
        kind,
        format!("could not read the {fact} of {}: {error}", path.display()),
    )
    .with_detail("path", path.display().to_string().into())
    .with_detail("fact", fact.into())
    .with_detail("os_error", error.to_string().into())
}

fn read_statfs(descriptor: RawFd, path: &Path) -> Result<libc::statfs> {
    // SAFETY: `statfs` is a plain-old-data C struct, so an all-zero value is a
    // valid initial buffer, and `fstatfs` writes it on success.
    let mut status: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: `descriptor` is open for this call and `status` is a writable
    // `statfs` of exactly the size the call expects.
    if unsafe { libc::fstatfs(descriptor, &mut status) } != 0 {
        return Err(fact_error(
            path,
            "filesystem and mount point",
            io::Error::last_os_error(),
        ));
    }
    Ok(status)
}

fn read_filesystem(status: &libc::statfs, path: &Path) -> Result<FileSystem> {
    let name = c_array(&status.f_fstypename)
        .ok_or_else(|| unreadable_fact(path, "filesystem type"))?
        .to_string_lossy()
        .into_owned();
    if name.eq_ignore_ascii_case("apfs") {
        Ok(FileSystem::Apfs)
    } else {
        Ok(FileSystem::Other(name))
    }
}

fn read_mount_point(status: &libc::statfs, path: &Path) -> Result<PathBuf> {
    let bytes = c_array(&status.f_mntonname)
        .ok_or_else(|| unreadable_fact(path, "mount point"))?
        .to_bytes();
    if bytes.is_empty() {
        return Err(unreadable_fact(path, "mount point"));
    }
    Ok(PathBuf::from(OsStr::from_bytes(bytes)))
}

fn read_device(descriptor: RawFd, path: &Path) -> Result<DeviceId> {
    // SAFETY: `stat` is a plain-old-data C struct, so an all-zero value is a
    // valid initial buffer, and `fstat` writes it on success.
    let mut status: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `descriptor` is open for this call and `status` is a writable
    // `stat` of exactly the size the call expects.
    if unsafe { libc::fstat(descriptor, &mut status) } != 0 {
        return Err(fact_error(path, "device", io::Error::last_os_error()));
    }
    // `dev_t` is signed on macOS; keep its bit pattern so two readings of one
    // volume compare equal without a sign-dependent conversion.
    Ok(DeviceId::new(u64::from(u32::from_ne_bytes(
        status.st_dev.to_ne_bytes(),
    ))))
}

/// The kernel's APFS volume UUID through `fgetattrlist(ATTR_VOL_UUID)`.
///
/// A failed, short or malformed answer yields `None`. The caller must refuse an
/// unreadable UUID rather than invent one, and the verification policy does.
fn read_volume_uuid(descriptor: RawFd) -> Option<VolumeUuid> {
    // SAFETY: `attrlist` is a plain-old-data C struct, so an all-zero value is a
    // valid starting point; the two fields set below are the ones libc defines
    // for a volume-attribute request.
    let mut list: libc::attrlist = unsafe { std::mem::zeroed() };
    list.bitmapcount = libc::ATTR_BIT_MAP_COUNT;
    list.volattr = libc::ATTR_VOL_UUID;
    let mut buffer = [0u8; VOLUME_ATTRIBUTE_BYTES];
    // SAFETY: `descriptor` is open, `list` is initialised and readable, and
    // `buffer` is a writable array whose exact length is passed, so
    // `fgetattrlist` writes only inside it.
    let status = unsafe {
        libc::fgetattrlist(
            descriptor,
            std::ptr::addr_of_mut!(list).cast(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            0,
        )
    };
    if status != 0 {
        return None;
    }
    let length = u32::from_ne_bytes(buffer.get(0..4)?.try_into().ok()?) as usize;
    if length < 4 + 16 || length > buffer.len() {
        return None;
    }
    let bytes: [u8; 16] = buffer.get(4..20)?.try_into().ok()?;
    Some(VolumeUuid::from_bytes(bytes))
}

/// Whether a mount point belongs to the boot volume or its volume group. Barge
/// never manages either.
fn is_boot_volume(mount_point: &Path) -> bool {
    mount_point == Path::new("/") || mount_point.starts_with("/System/Volumes")
}

/// Whether the mount point is a mount root directly under the mount parent, and
/// that parent is a real directory rather than a link.
fn is_under_volumes_parent(mount_point: &Path) -> bool {
    if mount_point.parent() != Some(Path::new(VOLUMES_PARENT)) {
        return false;
    }
    match std::fs::symlink_metadata(VOLUMES_PARENT) {
        Ok(metadata) => metadata.is_dir(),
        Err(_) => false,
    }
}

/// A NUL-terminated C array as a borrowed string. The kernel's `statfs` arrays
/// are fixed-size and NUL-terminated, so an array with no terminator is a fact
/// that could not be read.
fn c_array(array: &[libc::c_char]) -> Option<&CStr> {
    // SAFETY: `c_char` and `u8` have the same size and alignment, and the slice
    // covers exactly the array, so the reinterpretation is sound.
    let bytes = unsafe { std::slice::from_raw_parts(array.as_ptr().cast::<u8>(), array.len()) };
    CStr::from_bytes_until_nul(bytes).ok()
}

/// A fact the platform could not read on a descriptor that is otherwise open.
fn unreadable_fact(path: &Path, fact: &str) -> Error {
    Error::new(
        ErrorKind::UnknownState,
        format!("the {fact} of {} is unreadable", path.display()),
    )
    .with_detail("path", path.display().to_string().into())
    .with_detail("fact", fact.into())
}

/// The account home from the password database for the effective uid, never
/// from the environment (stage 1 design, "Account-wide state directory").
fn account_home() -> Result<PathBuf> {
    // SAFETY: `geteuid` takes no arguments, cannot fail and has no preconditions.
    let uid = unsafe { libc::geteuid() };
    let mut buffer = vec![0u8; 1024];
    loop {
        // SAFETY: `passwd` is a plain-old-data C struct, so an all-zero value is
        // a valid initial buffer that `getpwuid_r` overwrites on success.
        let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        // SAFETY: `entry`, `buffer` and `result` are valid writable memory of the
        // declared sizes for the duration of the call, and `getpwuid_r` writes
        // only within them, returning its own status.
        let status = unsafe {
            libc::getpwuid_r(
                uid,
                &mut entry,
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };
        if status == libc::ERANGE && buffer.len() < MAX_PASSWD_BUFFER_BYTES {
            buffer.resize(buffer.len() * 2, 0);
            continue;
        }
        if status != 0 {
            return Err(passwd_error(uid, io::Error::from_raw_os_error(status)));
        }
        if result.is_null() {
            return Err(passwd_error(uid, io::Error::from(io::ErrorKind::NotFound)));
        }
        // `result` points at `entry` on success, whose `pw_dir` points into
        // `buffer`; both are alive until the bytes are copied below.
        let directory = entry.pw_dir;
        if directory.is_null() {
            return Err(passwd_error(uid, io::Error::from(io::ErrorKind::NotFound)));
        }
        // SAFETY: `pw_dir` is a NUL-terminated string owned by the password
        // entry, which stays valid until the next call using this buffer.
        let bytes = unsafe { CStr::from_ptr(directory) }.to_bytes();
        if bytes.is_empty() {
            return Err(passwd_error(uid, io::Error::from(io::ErrorKind::NotFound)));
        }
        return Ok(PathBuf::from(OsStr::from_bytes(bytes)));
    }
}

/// A password-database answer Barge could not read: unknown, never `$HOME`.
fn passwd_error(uid: libc::uid_t, error: io::Error) -> Error {
    Error::new(
        ErrorKind::UnknownState,
        format!("the account home for the effective uid {uid} could not be read: {error}"),
    )
    .with_detail("uid", u64::from(uid).into())
    .with_detail("os_error", error.to_string().into())
}
