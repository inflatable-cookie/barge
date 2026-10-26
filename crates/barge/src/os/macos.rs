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
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{Error, ErrorKind, Result};
use crate::os::facts::{
    AccountFacts, ChildEntry, ChildListing, ChildRemoval, DescriptorScan, DescriptorScanRequest,
    DeviceId, DirHandle, DirId, DirOps, EntryFacts, EntryKind, FileSystem, MountPoint, MountRoot,
    OpenDescriptor, ProcessObserver, RemoveKind, VolumeFacts, VolumeIdentity, VolumeUuid,
};

/// The parent every managed volume mounts directly under.
const VOLUMES_PARENT: &str = "/Volumes";

/// The account-wide state root, relative to the account home (stage 1 design,
/// "Account-wide state directory").
const STATE_ROOT_RELATIVE: &str = "Library/Application Support/barge/state";

/// Where this platform keeps an account's application state, relative to its
/// home. A foreign tool's own leaf names are added by the module that reads it.
const APPLICATION_SUPPORT_RELATIVE: &str = "Library/Application Support";

/// Where this platform keeps an account's caches, relative to its home.
const CACHES_RELATIVE: &str = "Library/Caches";

/// The largest password-database buffer Barge will grow to before refusing.
const MAX_PASSWD_BUFFER_BYTES: usize = 64 * 1024;

/// The most names one listing examines, even when the caller asks for more.
/// It is the design's per-directory name bound ("Directory names per listing").
const LIST_ENTRY_LIMIT: usize = 200_000;

/// Distinguishes temporary names issued in one process.
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// A generous fixed buffer for one `fgetattrlist` volume attribute: the kernel
/// writes a 4-byte length followed by the attribute's value.
const VOLUME_ATTRIBUTE_BYTES: usize = 32;

/// `PROC_ALL_PIDS` from `sys/proc_info.h`: every process the kernel reports.
const PROC_ALL_PIDS: u32 = 1;

/// `PROC_PIDFDVNODEPATHINFO` from `sys/proc_info.h`: one descriptor's vnode path.
const PROC_PIDFDVNODEPATHINFO: libc::c_int = 2;

/// How many descriptors one process may hold before the scan is bounded: the
/// platform's own open-file limit is far below this, so reaching it means the
/// observation cannot be complete.
const SCAN_DESCRIPTORS_PER_PROCESS: usize = 4096;

// `proc_listpids` and `proc_pidfdinfo` are the two libproc calls the descriptor
// scan needs; `libc` declares neither, so the seam declares them itself.
extern "C" {
    fn proc_listpids(
        kind: u32,
        typeinfo: u32,
        buffer: *mut libc::c_void,
        buffer_size: libc::c_int,
    ) -> libc::c_int;
    fn proc_pidfdinfo(
        pid: libc::c_int,
        descriptor: libc::c_int,
        flavor: libc::c_int,
        buffer: *mut libc::c_void,
        buffer_size: libc::c_int,
    ) -> libc::c_int;
}

/// `struct proc_fileinfo` from `sys/proc_info.h`, which `libc` does not declare.
#[repr(C)]
struct ProcFileInfo {
    fi_openflags: u32,
    fi_status: u32,
    fi_offset: i64,
    fi_type: i32,
    fi_guardflags: u32,
}

/// `struct vnode_fdinfowithpath` from `sys/proc_info.h`.
#[repr(C)]
struct VnodeFdInfoWithPath {
    pfi: ProcFileInfo,
    pvip: libc::vnode_info_path,
}

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

    fn tool_state_dir(&self, tool: &str) -> Result<PathBuf> {
        Ok(account_home()?
            .join(APPLICATION_SUPPORT_RELATIVE)
            .join(tool)
            .join("state"))
    }

    fn tool_cache_dir(&self, tool: &str) -> Result<PathBuf> {
        Ok(account_home()?.join(CACHES_RELATIVE).join(tool))
    }
}

impl ProcessObserver for MacOs {
    fn effective_uid(&self) -> Result<u32> {
        // SAFETY: `geteuid` takes no arguments, cannot fail and has no
        // preconditions.
        Ok(unsafe { libc::geteuid() })
    }

    fn boot_identity(&self) -> Result<String> {
        // SAFETY: `timeval` is a plain-old-data C struct, so an all-zero value
        // is a valid initial buffer that `sysctl` overwrites on success.
        let mut value: libc::timeval = unsafe { std::mem::zeroed() };
        let mut size = std::mem::size_of::<libc::timeval>();
        let mut mib = [libc::CTL_KERN, libc::KERN_BOOTTIME];
        // SAFETY: `mib` and `size` describe the request, and `value` is a
        // writable `timeval` whose exact size is passed, so the kernel writes
        // only inside it.
        let status = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                2,
                std::ptr::addr_of_mut!(value).cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if status != 0 || size != std::mem::size_of::<libc::timeval>() {
            return Err(Error::new(
                ErrorKind::UnknownState,
                format!(
                    "the boot identity could not be read: {}",
                    io::Error::last_os_error()
                ),
            )
            .with_detail("fact", "boot identity".into()));
        }
        Ok(format!("{}.{}", value.tv_sec, value.tv_usec))
    }

    fn process_start(&self, pid: u32) -> Result<Option<u64>> {
        if pid == 0 {
            return Err(Error::new(
                ErrorKind::UnknownState,
                "pid 0 has no process start identity",
            )
            .with_detail("pid", u64::from(pid).into()));
        }
        let Some(info) = bsd_info(pid)? else {
            return Ok(None);
        };
        Ok(Some(
            info.pbi_start_tvsec
                .saturating_mul(1_000_000)
                .saturating_add(info.pbi_start_tvusec),
        ))
    }

    fn scan_open_descriptors(&self, request: &DescriptorScanRequest<'_>) -> Result<DescriptorScan> {
        let uid = self.effective_uid()?;
        let started = std::time::Instant::now();
        // The kernel reports canonical paths, so the root is matched under both
        // the spelling the walk reached and the one the filesystem resolves.
        let mut roots = vec![request.root.to_path_buf()];
        if let Ok(canonical) = std::fs::canonicalize(request.root) {
            if canonical != request.root {
                roots.push(canonical);
            }
        }

        // The whole system's pid list is read first; a list that fills the
        // buffer is bounded, because a missing pid could hold the root open.
        let mut pids = vec![0i32; SCAN_PID_CAP];
        let pids_bytes = pids.len().saturating_mul(std::mem::size_of::<i32>());
        // SAFETY: `pids` is a writable slice whose exact byte length is passed,
        // so the kernel writes only inside it and returns how many bytes it wrote.
        let written = unsafe {
            proc_listpids(
                PROC_ALL_PIDS,
                0,
                pids.as_mut_ptr().cast(),
                libc::c_int::try_from(pids_bytes).unwrap_or(libc::c_int::MAX),
            )
        };
        if written <= 0 {
            return Err(Error::new(
                ErrorKind::UnknownState,
                format!(
                    "the process list could not be read: {}",
                    io::Error::last_os_error()
                ),
            )
            .with_detail("fact", "process list".into()));
        }
        let written = written as usize;
        if written >= pids_bytes {
            return Ok(DescriptorScan::Bounded {
                processes: 0,
                descriptors: 0,
                bound: "process list",
            });
        }
        let count = written / std::mem::size_of::<i32>();

        let mut processes = 0usize;
        let mut descriptors = 0usize;
        let mut open = Vec::new();
        for pid in pids.into_iter().take(count) {
            if started.elapsed() >= request.timeout {
                return Ok(DescriptorScan::Bounded {
                    processes,
                    descriptors,
                    bound: "time",
                });
            }
            if pid <= 0 {
                continue;
            }
            let pid = u32::try_from(pid).unwrap_or(0);
            let Some(info) = bsd_info(pid)? else {
                // The process exited while it was observed: nothing can be held
                // open by a process that is positively gone.
                continue;
            };
            if info.pbi_uid != uid {
                // A process of another uid is the named limit of this evidence,
                // never a fact Barge claims.
                continue;
            }
            processes = processes.saturating_add(1);
            if processes > request.max_processes {
                return Ok(DescriptorScan::Bounded {
                    processes,
                    descriptors,
                    bound: "processes",
                });
            }

            let mut fds = vec![
                libc::proc_fdinfo {
                    proc_fd: 0,
                    proc_fdtype: 0,
                };
                SCAN_DESCRIPTORS_PER_PROCESS
            ];
            let fds_bytes = fds
                .len()
                .saturating_mul(std::mem::size_of::<libc::proc_fdinfo>());
            // SAFETY: `fds` is a writable slice whose exact byte length is
            // passed, so the kernel writes only inside it.
            let written = unsafe {
                libc::proc_pidinfo(
                    libc::c_int::try_from(pid).unwrap_or(libc::c_int::MAX),
                    libc::PROC_PIDLISTFDS,
                    0,
                    fds.as_mut_ptr().cast(),
                    libc::c_int::try_from(fds_bytes).unwrap_or(libc::c_int::MAX),
                )
            };
            if written < 0 {
                let error = io::Error::last_os_error();
                match error.raw_os_error() {
                    Some(libc::ESRCH) => continue,
                    _ => return Err(unreadable_descriptors(pid, &error)),
                }
            }
            let written = written as usize;
            if written >= fds_bytes {
                return Ok(DescriptorScan::Bounded {
                    processes,
                    descriptors,
                    bound: "descriptors per process",
                });
            }
            let fd_count = written / std::mem::size_of::<libc::proc_fdinfo>();
            for fd in fds.into_iter().take(fd_count) {
                if started.elapsed() >= request.timeout {
                    return Ok(DescriptorScan::Bounded {
                        processes,
                        descriptors,
                        bound: "time",
                    });
                }
                descriptors = descriptors.saturating_add(1);
                if descriptors > request.max_descriptors {
                    return Ok(DescriptorScan::Bounded {
                        processes,
                        descriptors,
                        bound: "descriptors",
                    });
                }
                if libc::c_int::try_from(fd.proc_fdtype).ok() != Some(libc::PROX_FDTYPE_VNODE) {
                    continue;
                }
                let Some(path) = descriptor_path(pid, fd.proc_fd)? else {
                    continue;
                };
                if roots.iter().any(|root| path_is_at_or_under(&path, root)) {
                    open.push(OpenDescriptor {
                        pid,
                        descriptor: fd.proc_fd,
                        path,
                    });
                }
            }
        }
        Ok(DescriptorScan::Complete {
            processes,
            descriptors,
            open,
        })
    }
}

/// How many pids the scan reads from the kernel in one call. A system with more
/// processes than this makes the scan bounded rather than incomplete.
const SCAN_PID_CAP: usize = 8192;

/// One process's BSD info, or `Ok(None)` when the kernel reports no such
/// process. Any other failure is an unreadable fact.
fn bsd_info(pid: u32) -> Result<Option<libc::proc_bsdinfo>> {
    // SAFETY: `proc_bsdinfo` is a plain-old-data C struct, so an all-zero value
    // is a valid initial buffer that `proc_pidinfo` overwrites.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>();
    // SAFETY: `info` is a writable buffer of exactly the size passed, so the
    // kernel writes only inside it and returns how much it wrote.
    let written = unsafe {
        libc::proc_pidinfo(
            libc::c_int::try_from(pid).unwrap_or(libc::c_int::MAX),
            libc::PROC_PIDTBSDINFO,
            0,
            std::ptr::addr_of_mut!(info).cast(),
            libc::c_int::try_from(size).unwrap_or(libc::c_int::MAX),
        )
    };
    if written as usize == size {
        return Ok(Some(info));
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        // The kernel reports no such process: it is positively gone.
        Some(libc::ESRCH) => Ok(None),
        _ => Err(Error::new(
            ErrorKind::UnknownState,
            format!("the facts of pid {pid} are unreadable: {error}"),
        )
        .with_detail("pid", u64::from(pid).into())
        .with_detail("os_error", error.to_string().into())),
    }
}

/// One descriptor's path, or `Ok(None)` when the descriptor closed while it was
/// observed. Any other failure is an unreadable fact.
fn descriptor_path(pid: u32, descriptor: i32) -> Result<Option<PathBuf>> {
    // SAFETY: `VnodeFdInfoWithPath` is a plain-old-data C struct, so an
    // all-zero value is a valid initial buffer.
    let mut info: VnodeFdInfoWithPath = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<VnodeFdInfoWithPath>();
    // SAFETY: `info` is a writable buffer of exactly the size passed, so the
    // kernel writes only inside it and returns how much it wrote.
    let written = unsafe {
        proc_pidfdinfo(
            libc::c_int::try_from(pid).unwrap_or(libc::c_int::MAX),
            descriptor,
            PROC_PIDFDVNODEPATHINFO,
            std::ptr::addr_of_mut!(info).cast(),
            libc::c_int::try_from(size).unwrap_or(libc::c_int::MAX),
        )
    };
    if written as usize != size {
        let error = io::Error::last_os_error();
        return match error.raw_os_error() {
            Some(libc::ESRCH | libc::EBADF) => Ok(None),
            _ => Err(unreadable_descriptors(pid, &error)),
        };
    }
    // SAFETY: `vip_path` is a fixed array of `c_char` inside `info`, which is
    // alive for this borrow, and only bytes are read from it.
    let bytes = unsafe {
        std::slice::from_raw_parts(
            info.pvip.vip_path.as_ptr().cast::<u8>(),
            std::mem::size_of_val(&info.pvip.vip_path),
        )
    };
    let Ok(path) = CStr::from_bytes_until_nul(bytes) else {
        return Err(unreadable_descriptors(
            pid,
            &io::Error::from_raw_os_error(libc::ENAMETOOLONG),
        ));
    };
    if path.to_bytes().is_empty() {
        return Ok(None);
    }
    Ok(Some(PathBuf::from(OsStr::from_bytes(path.to_bytes()))))
}

fn unreadable_descriptors(pid: u32, error: &io::Error) -> Error {
    Error::new(
        ErrorKind::UnknownState,
        format!("the open descriptors of pid {pid} are unreadable: {error}"),
    )
    .with_detail("pid", u64::from(pid).into())
    .with_detail("os_error", error.to_string().into())
}

/// Whether one reported path is the root itself or lies under it, compared by
/// path components so `/a/bc` never matches the root `/a/b`.
fn path_is_at_or_under(path: &Path, root: &Path) -> bool {
    path == root || path.starts_with(root)
}

impl DirOps for MacOs {
    fn open_path(&self, path: &Path) -> Result<DirHandle> {
        let descriptor = open_directory(path)?;
        let identity = read_dir_identity(&descriptor, path)?;
        Ok(DirHandle::from_descriptor(descriptor, identity))
    }

    fn path_facts(&self, path: &Path) -> Result<Option<EntryFacts>> {
        let raw = path_cstring(path)?;
        // SAFETY: `stat` is a plain-old-data C struct, so an all-zero value is a
        // valid initial buffer, and `lstat` writes it on success.
        let mut status: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: `raw` is a NUL-terminated path that outlives the call and
        // `status` is a writable `stat` of exactly the size the call expects.
        if unsafe { libc::lstat(raw.as_ptr(), &mut status) } != 0 {
            return entry_result(path, "entry", io::Error::last_os_error());
        }
        Ok(Some(entry_facts(&status)))
    }

    fn open_child_dir(&self, parent: &DirHandle, name: &str) -> Result<DirHandle> {
        let directory = parent_descriptor(parent, name)?;
        let raw = name_cstring(name)?;
        // SAFETY: `directory` is open for this call, `raw` is one NUL-terminated
        // component, and the flags are libc's own constants. `openat` returns
        // either -1 or a fresh descriptor this function then owns.
        let descriptor = unsafe {
            libc::openat(
                directory,
                raw.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if descriptor < 0 {
            return Err(child_error(
                name,
                "cannot open the child directory",
                io::Error::last_os_error(),
            ));
        }
        // SAFETY: `descriptor` is a fresh non-negative descriptor from `openat`
        // and no other owner exists, so `File` may take ownership of it.
        let file = unsafe { File::from_raw_fd(descriptor) };
        let identity = read_dir_identity(&file, Path::new(name))?;
        Ok(DirHandle::from_descriptor(file, identity))
    }

    fn duplicate_dir(&self, handle: &DirHandle) -> Result<DirHandle> {
        let directory = parent_descriptor(handle, "duplicate")?;
        // SAFETY: `directory` is open for this call; `dup` returns either -1 or
        // a fresh descriptor this function then owns.
        let duplicate = unsafe { libc::dup(directory) };
        if duplicate < 0 {
            return Err(child_error(
                "duplicate",
                "cannot duplicate the directory",
                io::Error::last_os_error(),
            ));
        }
        // SAFETY: `duplicate` is a fresh non-negative descriptor from `dup` and
        // no other owner exists, so `File` may take ownership of it.
        let file = unsafe { File::from_raw_fd(duplicate) };
        Ok(DirHandle::from_descriptor(file, handle.identity()))
    }

    fn child_facts(&self, parent: &DirHandle, name: &str) -> Result<Option<EntryFacts>> {
        let directory = parent_descriptor(parent, name)?;
        let raw = name_cstring(name)?;
        // SAFETY: `stat` is a plain-old-data C struct, so an all-zero value is a
        // valid initial buffer, and `fstatat` writes it on success.
        let mut status: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: `directory` is open for this call, `raw` is one NUL-terminated
        // component and `status` is a writable `stat` of the size the call
        // expects. `AT_SYMLINK_NOFOLLOW` means the link itself is reported.
        if unsafe {
            libc::fstatat(
                directory,
                raw.as_ptr(),
                &mut status,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            return child_entry_result(name, io::Error::last_os_error());
        }
        Ok(Some(entry_facts(&status)))
    }

    fn create_child_dir(&self, parent: &DirHandle, name: &str, mode: u32) -> Result<DirId> {
        let directory = parent_descriptor(parent, name)?;
        let raw = name_cstring(name)?;
        // SAFETY: `directory` is open for this call, `raw` is one NUL-terminated
        // component, and `mode` is passed with the default argument promotion a
        // variadic C function expects.
        if unsafe { libc::mkdirat(directory, raw.as_ptr(), mode as libc::mode_t) } != 0 {
            return Err(child_error(
                name,
                "cannot create the child directory",
                io::Error::last_os_error(),
            ));
        }
        Ok(self.open_child_dir(parent, name)?.identity())
    }

    fn publish_child_file(
        &self,
        parent: &DirHandle,
        name: &str,
        bytes: &[u8],
        mode: u32,
    ) -> Result<()> {
        let directory = parent_descriptor(parent, name)?;
        let target = name_cstring(name)?;
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temporary = format!(".barge-tmp-{}-{sequence}", std::process::id());
        let temporary_raw = name_cstring(&temporary)?;
        // SAFETY: `directory` is open, `temporary_raw` is one NUL-terminated
        // component, the flags are libc's own constants and the variadic mode is
        // passed with the promotion C expects. The result is either -1 or a
        // fresh descriptor this function owns.
        let descriptor = unsafe {
            libc::openat(
                directory,
                temporary_raw.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                mode as libc::c_int,
            )
        };
        if descriptor < 0 {
            return Err(child_error(
                name,
                "cannot create a temporary file",
                io::Error::last_os_error(),
            ));
        }
        // SAFETY: `descriptor` is a fresh non-negative descriptor from `openat`
        // and no other owner exists, so `File` may take ownership of it.
        let mut file = unsafe { File::from_raw_fd(descriptor) };
        let published = (|| -> Result<()> {
            file.write_all(bytes)
                .map_err(|error| child_error(name, "cannot write the temporary file", error))?;
            file.sync_all()
                .map_err(|error| child_error(name, "cannot sync the temporary file", error))?;
            // SAFETY: both names are NUL-terminated components inside the same
            // open directory, which is what makes the rename atomic.
            if unsafe {
                libc::renameat(
                    directory,
                    temporary_raw.as_ptr(),
                    directory,
                    target.as_ptr(),
                )
            } != 0
            {
                return Err(child_error(
                    name,
                    "cannot publish the file",
                    io::Error::last_os_error(),
                ));
            }
            // SAFETY: `directory` is the open directory the rename happened in.
            if unsafe { libc::fsync(directory) } != 0 {
                return Err(child_error(
                    name,
                    "cannot sync the directory",
                    io::Error::last_os_error(),
                ));
            }
            Ok(())
        })();
        if published.is_err() {
            // Best effort: a leftover temporary file is never read as the
            // target, and reporting the original failure matters more.
            // SAFETY: `temporary_raw` names the file this call created.
            unsafe {
                libc::unlinkat(directory, temporary_raw.as_ptr(), 0);
            }
        }
        published
    }

    fn read_child_file(
        &self,
        parent: &DirHandle,
        name: &str,
        cap: usize,
    ) -> Result<Option<Vec<u8>>> {
        let directory = parent_descriptor(parent, name)?;
        let raw = name_cstring(name)?;
        // SAFETY: `directory` is open, `raw` is one NUL-terminated component and
        // the flags are libc's own constants. The result is either -1 or a fresh
        // descriptor this function owns.
        let descriptor = unsafe {
            libc::openat(
                directory,
                raw.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if descriptor < 0 {
            return child_read_result(name, io::Error::last_os_error());
        }
        // SAFETY: `descriptor` is a fresh non-negative descriptor from `openat`
        // and no other owner exists, so `File` may take ownership of it.
        let mut file = unsafe { File::from_raw_fd(descriptor) };
        let facts = entry_facts(&fstat(&file, Path::new(name))?);
        if facts.kind != EntryKind::File {
            return Err(Error::new(
                ErrorKind::UnknownState,
                format!("`{name}` is a {}, not a regular file", facts.kind.as_str()),
            )
            .with_detail("name", name.into()));
        }
        if facts.size > cap as u64 {
            return Err(child_bound_error(name, facts.size, cap));
        }
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            let read = file
                .read(&mut chunk)
                .map_err(|error| child_error(name, "cannot read the file", error))?;
            if read == 0 {
                return Ok(Some(buffer));
            }
            if buffer.len() + read > cap {
                return Err(child_bound_error(name, (buffer.len() + read) as u64, cap));
            }
            buffer.extend_from_slice(&chunk[..read]);
        }
    }

    fn list_children(&self, parent: &DirHandle, max_names: usize) -> Result<ChildListing> {
        let directory = parent_descriptor(parent, "listing")?;
        // `fdopendir` takes ownership of the descriptor it is given, so the
        // caller's handle is duplicated rather than closed under it.
        // SAFETY: `directory` is open for this call.
        let duplicate = unsafe { libc::dup(directory) };
        if duplicate < 0 {
            return Err(listing_error(io::Error::last_os_error()));
        }
        // SAFETY: `duplicate` is a fresh descriptor this function owns and
        // `fdopendir` returns either null or a directory stream that owns it.
        let stream = unsafe { libc::fdopendir(duplicate) };
        if stream.is_null() {
            // SAFETY: the stream was not created, so the descriptor is still
            // this function's to close.
            unsafe {
                libc::close(duplicate);
            }
            return Err(listing_error(io::Error::last_os_error()));
        }
        let mut entries = Vec::new();
        let mut total = 0usize;
        let mut truncated = false;
        loop {
            // SAFETY: `__error` returns this thread's errno location.
            unsafe {
                *libc::__error() = 0;
            }
            // SAFETY: `stream` is the directory stream created above.
            let entry = unsafe { libc::readdir(stream) };
            if entry.is_null() {
                // SAFETY: `__error` returns this thread's errno location.
                let errno = unsafe { *libc::__error() };
                if errno != 0 {
                    // SAFETY: `stream` is owned by this function.
                    unsafe {
                        libc::closedir(stream);
                    }
                    return Err(listing_error(io::Error::from_raw_os_error(errno)));
                }
                break;
            }
            // SAFETY: `entry` points at the live directory entry `readdir`
            // returned, and is read before the next call changes it.
            let name = entry_name(entry);
            let Some(name) = name else {
                continue;
            };
            if name == "." || name == ".." {
                continue;
            }
            total = total.saturating_add(1);
            if entries.len() < max_names && total <= LIST_ENTRY_LIMIT {
                if let Some(facts) = self.child_facts(parent, &name)? {
                    entries.push(ChildEntry { name, facts });
                }
            } else {
                truncated = true;
            }
            if total >= LIST_ENTRY_LIMIT {
                truncated = true;
                break;
            }
        }
        // SAFETY: `stream` is owned by this function and is not used again.
        unsafe {
            libc::closedir(stream);
        }
        Ok(ChildListing {
            entries,
            total,
            truncated,
        })
    }

    fn random_bytes(&self, length: usize) -> Result<Vec<u8>> {
        let mut source = File::open("/dev/urandom").map_err(|error| {
            Error::new(
                ErrorKind::Io,
                format!("cannot open the platform entropy source: {error}"),
            )
            .with_detail("os_error", error.to_string().into())
        })?;
        let mut buffer = vec![0u8; length];
        source.read_exact(&mut buffer).map_err(|error| {
            Error::new(
                ErrorKind::Io,
                format!("cannot read the platform entropy source: {error}"),
            )
            .with_detail("os_error", error.to_string().into())
        })?;
        Ok(buffer)
    }

    fn remove_child(
        &self,
        parent: &DirHandle,
        name: &str,
        kind: RemoveKind,
    ) -> Result<ChildRemoval> {
        let directory = parent_descriptor(parent, name)?;
        let raw = name_cstring(name)?;
        let flags = match kind {
            RemoveKind::File => 0,
            RemoveKind::Directory => libc::AT_REMOVEDIR,
        };
        // SAFETY: `directory` is open for this call, `raw` is one NUL-terminated
        // component and `flags` is either zero or libc's own directory flag, so
        // no symbolic link at the leaf is ever followed.
        if unsafe { libc::unlinkat(directory, raw.as_ptr(), flags) } == 0 {
            return Ok(ChildRemoval::Removed);
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::ENOENT) => Ok(ChildRemoval::Absent),
            Some(libc::ENOTEMPTY | libc::EEXIST) => Ok(ChildRemoval::NotEmpty),
            _ => Err(removal_error(name, kind, error)),
        }
    }

    fn rename_child(&self, parent: &DirHandle, from: &str, to: &str) -> Result<()> {
        let directory = parent_descriptor(parent, from)?;
        let source = name_cstring(from)?;
        let target = name_cstring(to)?;
        // SAFETY: both names are NUL-terminated components inside the same open
        // directory, which is exactly what makes the rename atomic.
        if unsafe { libc::renameat(directory, source.as_ptr(), directory, target.as_ptr()) } == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        let kind = match error.raw_os_error() {
            Some(libc::EXDEV) => ErrorKind::VolumeMismatch,
            Some(libc::ENOENT | libc::ENXIO | libc::EIO | libc::ENODEV) => ErrorKind::VolumeAbsent,
            _ => ErrorKind::Io,
        };
        Err(
            Error::new(kind, format!("cannot rename `{from}` to `{to}`: {error}"))
                .with_detail("from", from.into())
                .with_detail("to", to.into())
                .with_detail("os_error", error.to_string().into()),
        )
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
    Ok(device_of(status.st_dev))
}

/// A device number as the platform reports it, kept as its bit pattern.
fn device_of(device: libc::dev_t) -> DeviceId {
    DeviceId::new(u64::from(u32::from_ne_bytes(device.to_ne_bytes())))
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

/// One absolute path as a NUL-terminated C string. A path with a NUL byte is a
/// usage failure, never a truncated path.
fn path_cstring(path: &Path) -> Result<CString> {
    CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        Error::new(
            ErrorKind::Usage,
            format!("{} contains a NUL byte", path.display()),
        )
        .with_detail("path", path.display().to_string().into())
    })
}

/// One child name as a NUL-terminated C string. A name that is not exactly one
/// path component is refused, so the platform layer cannot be asked to walk a
/// path by name.
fn name_cstring(name: &str) -> Result<CString> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') {
        return Err(Error::new(
            ErrorKind::Usage,
            format!("`{name}` is not one path component"),
        )
        .with_detail("name", name.into()));
    }
    CString::new(name.as_bytes()).map_err(|_| {
        Error::new(ErrorKind::Usage, "a child name contains a NUL byte")
            .with_detail("name", name.into())
    })
}

/// The descriptor behind a parent handle.
fn parent_descriptor(parent: &DirHandle, name: &str) -> Result<RawFd> {
    parent.descriptor().map(AsRawFd::as_raw_fd).ok_or_else(|| {
        Error::new(
            ErrorKind::Usage,
            format!("the handle for `{name}` has no open descriptor"),
        )
        .with_detail("name", name.into())
    })
}

/// The identity of an opened directory.
fn read_dir_identity(descriptor: &File, path: &Path) -> Result<DirId> {
    let status = fstat(descriptor, path)?;
    Ok(DirId {
        device: device_of(status.st_dev),
        inode: status.st_ino,
    })
}

fn fstat(descriptor: &File, path: &Path) -> Result<libc::stat> {
    // SAFETY: `stat` is a plain-old-data C struct, so an all-zero value is a
    // valid initial buffer, and `fstat` writes it on success.
    let mut status: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: the descriptor is open for this call and `status` is a writable
    // `stat` of exactly the size the call expects.
    if unsafe { libc::fstat(descriptor.as_raw_fd(), &mut status) } != 0 {
        return Err(fact_error(
            path,
            "device and inode",
            io::Error::last_os_error(),
        ));
    }
    Ok(status)
}

/// One `stat` as the facts a module above `os` may use. Nothing is followed and
/// nothing is inferred: the kind, device, inode, owner and mode are the entry's
/// own.
fn entry_facts(status: &libc::stat) -> EntryFacts {
    let kind = match status.st_mode & libc::S_IFMT {
        libc::S_IFDIR => EntryKind::Directory,
        libc::S_IFREG => EntryKind::File,
        libc::S_IFLNK => EntryKind::Symlink,
        _ => EntryKind::Other,
    };
    EntryFacts {
        kind,
        device: device_of(status.st_dev),
        inode: status.st_ino,
        uid: status.st_uid,
        mode: u32::from(status.st_mode) & 0o7777,
        size: u64::try_from(status.st_size).unwrap_or(0),
        modified_ms: modified_ms(status),
    }
}

/// The modification time in milliseconds since the Unix epoch, when the
/// platform's two fields convert cleanly.
fn modified_ms(status: &libc::stat) -> Option<u64> {
    let seconds = u64::try_from(status.st_mtime).ok()?;
    let nanos = u64::try_from(status.st_mtime_nsec).ok()?;
    seconds.checked_mul(1000)?.checked_add(nanos / 1_000_000)
}

/// The name of one directory entry, lossily decoded: a listing reports names,
/// and a name that is not UTF-8 is still counted.
fn entry_name(entry: *const libc::dirent) -> Option<String> {
    // SAFETY: `entry` points at a live directory entry, whose `d_name` is a
    // NUL-terminated array inside it.
    let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
    let bytes = name.to_bytes();
    if bytes.is_empty() {
        return None;
    }
    Some(OsStr::from_bytes(bytes).to_string_lossy().into_owned())
}

/// An absent entry is `Ok(None)`; anything else is the platform's own failure,
/// mapped to the contract's kinds.
fn entry_result(path: &Path, action: &str, error: io::Error) -> Result<Option<EntryFacts>> {
    match error.raw_os_error() {
        Some(libc::ENOENT) => Ok(None),
        Some(libc::ELOOP) => Err(Error::new(
            ErrorKind::UnknownState,
            format!(
                "{} is a symbolic link, which is never followed",
                path.display()
            ),
        )
        .with_detail("path", path.display().to_string().into())),
        Some(libc::ENOTDIR) => Err(Error::new(
            ErrorKind::RefusedContainment,
            format!("{} is not inside a directory", path.display()),
        )
        .with_detail("path", path.display().to_string().into())),
        _ => Err(Error::new(
            ErrorKind::Io,
            format!("{action} {}: {error}", path.display()),
        )
        .with_detail("path", path.display().to_string().into())
        .with_detail("os_error", error.to_string().into())),
    }
}

/// The result of opening a child for reading: absent is `Ok(None)`, a symlink is
/// unknown state and anything else is the platform's own failure.
fn child_read_result(name: &str, error: io::Error) -> Result<Option<Vec<u8>>> {
    match error.raw_os_error() {
        Some(libc::ENOENT) => Ok(None),
        Some(libc::ELOOP) => Err(Error::new(
            ErrorKind::UnknownState,
            format!("`{name}` is a symbolic link, which is never followed"),
        )
        .with_detail("name", name.into())),
        _ => Err(Error::new(
            ErrorKind::Io,
            format!("cannot open the child `{name}`: {error}"),
        )
        .with_detail("name", name.into())
        .with_detail("os_error", error.to_string().into())),
    }
}

fn child_entry_result(name: &str, error: io::Error) -> Result<Option<EntryFacts>> {
    match error.raw_os_error() {
        Some(libc::ENOENT) => Ok(None),
        Some(libc::ELOOP) => Err(Error::new(
            ErrorKind::UnknownState,
            format!("`{name}` is a symbolic link, which is never followed"),
        )
        .with_detail("name", name.into())),
        _ => Err(Error::new(
            ErrorKind::Io,
            format!("cannot read the child `{name}`: {error}"),
        )
        .with_detail("name", name.into())
        .with_detail("os_error", error.to_string().into())),
    }
}

fn child_error(name: &str, action: &str, error: io::Error) -> Error {
    let kind = match error.raw_os_error() {
        Some(libc::ENOENT | libc::ENXIO | libc::EIO | libc::ENODEV) => ErrorKind::VolumeAbsent,
        Some(libc::ELOOP) => ErrorKind::RefusedContainment,
        _ => ErrorKind::Io,
    };
    Error::new(kind, format!("{action} `{name}`: {error}"))
        .with_detail("name", name.into())
        .with_detail("os_error", error.to_string().into())
}

fn child_bound_error(name: &str, bytes: u64, cap: usize) -> Error {
    Error::new(
        ErrorKind::ResourceBound,
        format!("`{name}` is {bytes} bytes, over the {cap}-byte cap"),
    )
    .with_detail("name", name.into())
    .with_detail("bytes", bytes.into())
    .with_detail("cap", cap.into())
}

fn listing_error(error: io::Error) -> Error {
    Error::new(ErrorKind::Io, format!("cannot list the directory: {error}"))
        .with_detail("os_error", error.to_string().into())
}

/// A removal the platform refused for a reason other than an entry that came
/// back: a vanished volume stays `volume_absent`, and a leaf that turned into a
/// directory when a file was expected is a containment refusal, because the
/// whole-root delete never widens what it removes.
fn removal_error(name: &str, kind: RemoveKind, error: io::Error) -> Error {
    let (error_kind, message) = match error.raw_os_error() {
        Some(libc::ENOENT | libc::ENXIO | libc::EIO | libc::ENODEV) => (
            ErrorKind::VolumeAbsent,
            format!("the volume holding `{name}` is not available: {error}"),
        ),
        Some(libc::ELOOP | libc::ENOTDIR) => (
            ErrorKind::RefusedContainment,
            format!("`{name}` changed into something this delete never removes: {error}"),
        ),
        Some(libc::EISDIR) if kind == RemoveKind::File => (
            ErrorKind::RefusedContainment,
            format!("`{name}` is a directory where a file was observed: {error}"),
        ),
        _ => (ErrorKind::Io, format!("cannot remove `{name}`: {error}")),
    };
    Error::new(error_kind, message)
        .with_detail("name", name.into())
        .with_detail("os_error", error.to_string().into())
}
