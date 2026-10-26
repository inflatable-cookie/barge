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
use std::time::{Duration, Instant};

use crate::error::{Error, ErrorKind, Result};
use crate::os::facts::{
    AccountFacts, ChildEntry, ChildListing, ChildRemoval, DescriptorScan, DescriptorScanRequest,
    DeviceId, DirHandle, DirId, DirOps, EntryFacts, EntryKind, FileSystem, MountPoint, MountRoot,
    OpenDescriptor, ProcessIdentity, ProcessObserver, RemoveKind, RenameOutcome, RetriedUnreadable,
    RetryEvidence, RetryResolution, VolumeFacts, VolumeIdentity, VolumeUuid,
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

/// `PROC_PIDFDVNODEPATHINFO` from `sys/proc_info.h`: one descriptor's vnode path.
const PROC_PIDFDVNODEPATHINFO: libc::c_int = 2;

/// How many descriptors one process may hold before the scan is bounded: the
/// platform's own open-file limit is far below this, so reaching it means the
/// observation cannot be complete.
const SCAN_DESCRIPTORS_PER_PROCESS: usize = 4096;

/// The backoff before each re-observation of one process whose descriptor
/// listing answered `EPERM`. The settle is short on purpose: the answers it
/// settles are intermittent, and a real refusal must still refuse quickly.
const EPERM_BACKOFFS: [Duration; 3] = [
    Duration::from_millis(25),
    Duration::from_millis(75),
    Duration::from_millis(100),
];

/// The most time one process's `EPERM` may be re-observed for, always inside the
/// scan's own overall time bound. It is larger than the backoffs above, so the
/// retry count is what normally stops a persistent answer.
const EPERM_SETTLE_WINDOW: Duration = Duration::from_millis(250);

/// The backoff schedule this build retries a same-uid `EPERM` on. A test that
/// states an empty schedule is the counterfactual: the same answer that the
/// retry settles then refuses at once.
#[cfg(test)]
pub(crate) fn retry_schedule() -> &'static [Duration] {
    &EPERM_BACKOFFS
}

// `proc_pidfdinfo` is the one libproc call the descriptor scan needs; `libc`
// declares it not at all, so the seam declares it itself. The process table is
// read through `sysctl`, which needs no libproc call and no privilege.
extern "C" {
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
        Ok(Some(bsd_start(&info)))
    }

    fn scan_open_descriptors(&self, request: &DescriptorScanRequest<'_>) -> Result<DescriptorScan> {
        // The kernel's own process table is read first, so every candidate is
        // classified by its effective uid before any descriptor is inspected. A
        // table that fills its bound is bounded, because a missing pid could
        // hold the root open.
        let Some(table) = self.process_table()? else {
            return Ok(DescriptorScan::Bounded {
                processes: 0,
                descriptors: 0,
                bound: "process list",
                retry: RetryEvidence::default(),
            });
        };
        let table: Vec<ProcessIdentity> = table.into_iter().map(|row| row.identity).collect();
        self.scan_processes(request, &table, &EPERM_BACKOFFS, &mut [])
    }
}

impl MacOs {
    /// Scan one known process table with the real descriptor enumeration,
    /// classification and self-exclusion.
    ///
    /// The production path always passes the kernel's own table. The unit tests
    /// pass a table they state themselves, which is how an injected fact — a
    /// foreign effective uid, an unreadable uid, or a table that claims a pid
    /// this account may not read — is exercised against the real
    /// `PROC_PIDLISTFDS` and `PROC_PIDFDVNODEPATHINFO` reads for the pids a
    /// fixture does own. Nothing else about the scan changes.
    #[cfg(test)]
    pub(crate) fn scan_open_descriptors_over(
        &self,
        request: &DescriptorScanRequest<'_>,
        table: &[ProcessIdentity],
    ) -> Result<DescriptorScan> {
        self.scan_processes(request, table, &EPERM_BACKOFFS, &mut [])
    }

    /// Scan one known process table with the real descriptor enumeration, the
    /// real identity re-observation, a stated backoff schedule and a test's
    /// stated `EPERM` answers.
    ///
    /// The production path states nothing. A test states an `EPERM` sequence so
    /// the bounded re-observation of a transient answer is exercised without the
    /// kernel having to produce one: the classification, the identity re-read,
    /// the backoff, the retry bound and the descriptor reads are all the
    /// production ones.
    #[cfg(test)]
    pub(crate) fn scan_open_descriptors_with(
        &self,
        request: &DescriptorScanRequest<'_>,
        table: &[ProcessIdentity],
        backoffs: &[Duration],
        faults: &mut [StatedListing],
    ) -> Result<DescriptorScan> {
        self.scan_processes(request, table, backoffs, faults)
    }

    fn scan_processes(
        &self,
        request: &DescriptorScanRequest<'_>,
        table: &[ProcessIdentity],
        backoffs: &[Duration],
        faults: &mut [StatedListing],
    ) -> Result<DescriptorScan> {
        // The only fact the scan cannot begin with: this account's own uid. Every
        // later failure is an answer, so the bounded retry record reaches the
        // refusal that carries it.
        let uid = self.effective_uid()?;
        let mut retry = RetryEvidence::default();
        self.scan_table(request, table, backoffs, faults, uid, &mut retry)
    }

    /// One scan of a known process table, recording its bounded `EPERM`
    /// re-observation in `retry` so every answer carries it.
    #[allow(clippy::too_many_arguments)]
    fn scan_table(
        &self,
        request: &DescriptorScanRequest<'_>,
        table: &[ProcessIdentity],
        backoffs: &[Duration],
        faults: &mut [StatedListing],
        uid: u32,
        retry: &mut RetryEvidence,
    ) -> Result<DescriptorScan> {
        let started = Instant::now();
        // The kernel reports canonical paths, so the root is matched under both
        // the spelling the walk reached and the one the filesystem resolves.
        let mut roots = vec![request.root.to_path_buf()];
        if let Ok(canonical) = std::fs::canonicalize(request.root) {
            if canonical != request.root {
                roots.push(canonical);
            }
        }

        let mut processes = 0usize;
        let mut descriptors = 0usize;
        let mut foreign = 0usize;
        let mut open = Vec::new();
        // Whether the process the request names as the calling one was found and
        // skipped. An exclusion that never matched proves nothing about what it
        // left out, so the scan answers unreadable rather than complete.
        let mut self_excluded = false;
        for identity in table.iter().copied() {
            if started.elapsed() >= request.timeout {
                return Ok(DescriptorScan::Bounded {
                    processes,
                    descriptors,
                    bound: "time",
                    retry: retry.clone(),
                });
            }
            let pid = identity.pid;
            // Classification comes first and needs no privilege: the effective
            // uid is in the kernel's own table. A process of another effective
            // uid is the named blind spot — counted and reported, never
            // inspected — and is never read as unreadable, because this account
            // may not read its facts at all.
            match identity.effective_uid {
                None => {
                    return Ok(DescriptorScan::Unreadable {
                        reason: format!(
                            "pid {pid} has no readable effective uid, so the scan cannot classify it"
                        ),
                        retry: retry.clone(),
                    });
                }
                Some(other) if other != uid => {
                    foreign = foreign.saturating_add(1);
                    continue;
                }
                Some(_) => {}
            }
            if let Some(exclusion) = request.self_exclusion {
                if pid == exclusion.pid {
                    // The calling process's own descriptors are the walk's and
                    // the checks', never evidence about another writer. The
                    // skip is conditional on the identity custody read when it
                    // asked for the scan: a reused pid is a different process.
                    if identity.start != exclusion.start {
                        return Ok(DescriptorScan::Unreadable {
                            reason: format!(
                                "pid {pid} is the calling process, but its start identity is {}, not the recorded {}; the scan cannot exclude itself",
                                identity.start, exclusion.start
                            ),
                            retry: retry.clone(),
                        });
                    }
                    self_excluded = true;
                    continue;
                }
            }
            // A listing the kernel answers `EPERM` for is re-observed by
            // identity before it is decided, never assumed: the count says how
            // many processes needed it and the bounded record says which ones
            // and how each resolved.
            let settled =
                self.settle_descriptors(identity, uid, started, request.timeout, backoffs, faults);
            if let Some(record) = settled.record {
                retry.record(record);
            }
            let fds = match settled.listing {
                SettledListing::Listed(fds) => fds,
                // The kernel reports no such process: it is positively gone, so
                // nothing it held is open.
                SettledListing::Gone => continue,
                // The pid now names another account's process: the named blind
                // spot, counted and reported, never inspected.
                SettledListing::Foreign => {
                    foreign = foreign.saturating_add(1);
                    continue;
                }
                // The scan's own time bound would have been crossed by another
                // retry, so the observation cannot be completed.
                SettledListing::Bounded => {
                    return Ok(DescriptorScan::Bounded {
                        processes,
                        descriptors,
                        bound: "time",
                        retry: retry.clone(),
                    })
                }
                // The observation itself cannot be completed. The record of the
                // re-observation travels with the refusal.
                SettledListing::Unreadable(reason) => {
                    return Ok(DescriptorScan::Unreadable {
                        reason,
                        retry: retry.clone(),
                    })
                }
            };
            processes = processes.saturating_add(1);
            if processes > request.max_processes {
                return Ok(DescriptorScan::Bounded {
                    processes,
                    descriptors,
                    bound: "processes",
                    retry: retry.clone(),
                });
            }
            if fds.len() >= SCAN_DESCRIPTORS_PER_PROCESS {
                return Ok(DescriptorScan::Bounded {
                    processes,
                    descriptors,
                    bound: "descriptors per process",
                    retry: retry.clone(),
                });
            }
            for fd in fds {
                if started.elapsed() >= request.timeout {
                    return Ok(DescriptorScan::Bounded {
                        processes,
                        descriptors,
                        bound: "time",
                        retry: retry.clone(),
                    });
                }
                descriptors = descriptors.saturating_add(1);
                if descriptors > request.max_descriptors {
                    return Ok(DescriptorScan::Bounded {
                        processes,
                        descriptors,
                        bound: "descriptors",
                        retry: retry.clone(),
                    });
                }
                if libc::c_int::try_from(fd.proc_fdtype).ok() != Some(libc::PROX_FDTYPE_VNODE) {
                    continue;
                }
                let path = match descriptor_path(pid, fd.proc_fd) {
                    Ok(Some(path)) => path,
                    Ok(None) => continue,
                    Err(error) => {
                        return Ok(DescriptorScan::Unreadable {
                            reason: error.message().to_owned(),
                            retry: retry.clone(),
                        })
                    }
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
        if let Some(exclusion) = request.self_exclusion {
            if !self_excluded {
                return Ok(DescriptorScan::Unreadable {
                    reason: format!(
                        "the calling process pid {} was not observed as this account's own in the process table, so the scan cannot prove what it excluded",
                        exclusion.pid
                    ),
                    retry: retry.clone(),
                });
            }
        }
        Ok(DescriptorScan::Complete {
            processes,
            descriptors,
            foreign,
            retry: retry.clone(),
            open,
        })
    }

    /// One same-uid process's descriptor listing, settling a transient `EPERM`
    /// by bounded identity re-observation.
    ///
    /// A listing the kernel answers `EPERM` for is never turned into evidence of
    /// absence, and elapsed time is never taken as proof that it cleared (barge#012,
    /// found by barge#007's owner-context diagnostic: the cause of a same-uid
    /// `EPERM` is not established). The scan re-reads the process's kernel
    /// identity — pid, start time and effective uid — and lists again after a
    /// short backoff, once per entry in `backoffs`, inside [`EPERM_SETTLE_WINDOW`]
    /// and inside the scan's own time bound. It settles
    /// only on a positive fact: a later listing, the kernel reporting no such
    /// pid, a changed start identity (a different process, re-evaluated under
    /// this same rule inside the same budget), or a changed effective uid (the
    /// named blind spot, ruling bcaa83d8). The same identity still answering
    /// `EPERM` after its retries is unreadable, which is a refusal.
    fn settle_descriptors(
        &self,
        classified: ProcessIdentity,
        uid: u32,
        started: Instant,
        timeout: Duration,
        backoffs: &[Duration],
        faults: &mut [StatedListing],
    ) -> Settled {
        let pid = classified.pid;
        let settle_started = Instant::now();
        let mut start = classified.start;
        let mut retries = 0usize;
        let mut record = RetriedUnreadable {
            pid,
            command: None,
            start,
            errnos: Vec::new(),
            identity_changes: 0,
            resolution: RetryResolution::Unreadable,
        };
        loop {
            let listing = match stated_errno(faults, pid) {
                Some(errno) => DescriptorListing::from_errno(errno),
                None => self.list_descriptors(pid),
            };
            match listing {
                DescriptorListing::Listed(fds) => {
                    record.resolution = RetryResolution::Listed;
                    return settle(
                        record,
                        SettledListing::Listed(fds),
                        settle_started,
                        started,
                        timeout,
                    );
                }
                DescriptorListing::Gone { errno } => {
                    // A retried listing that answers `ESRCH` is the positive fact
                    // that ended the settle, so the sequence records it. A first
                    // listing that answers it never made a retry record at all.
                    if !record.errnos.is_empty() {
                        if let Some(errno) = errno {
                            record.errnos.push(errno);
                        }
                    }
                    record.resolution = RetryResolution::Gone;
                    return settle(
                        record,
                        SettledListing::Gone,
                        settle_started,
                        started,
                        timeout,
                    );
                }
                DescriptorListing::Unreadable(error) => {
                    // Only `EPERM` is the intermittent answer this rule is about.
                    // Every other unreadable listing is unknown immediately, as
                    // it was before. When it answers a retry, its errno is the
                    // fact that ended the re-observation, so the sequence keeps
                    // it beside the `EPERM`s that were retried.
                    if error.raw_os_error() != Some(libc::EPERM) {
                        if !record.errnos.is_empty() {
                            if let Some(errno) = error.raw_os_error() {
                                record.errnos.push(errno);
                            }
                        }
                        return Settled {
                            listing: SettledListing::Unreadable(
                                unreadable_descriptors(pid, &error).message().to_owned(),
                            ),
                            record: retried(record),
                        };
                    }
                    record.errnos.push(libc::EPERM);
                    // The positive fact: what the kernel's own table reports for
                    // this pid now. Elapsed time decides nothing here.
                    let now = match self.reobserve_identity(pid, faults) {
                        Ok(Some(now)) => now,
                        // The kernel reports no such pid: it is positively gone,
                        // and a gone process holds no descriptors.
                        Ok(None) => {
                            record.resolution = RetryResolution::Gone;
                            return settle(
                                record,
                                SettledListing::Gone,
                                settle_started,
                                started,
                                timeout,
                            );
                        }
                        Err(error) => {
                            return Settled {
                                listing: SettledListing::Unreadable(error.message().to_owned()),
                                record: retried(record),
                            }
                        }
                    };
                    if record.command.is_none() {
                        record.command = Some(now.command);
                    }
                    match now.identity.effective_uid {
                        Some(other) if other != uid => {
                            // The pid now names another account's process: the
                            // named blind spot, counted and reported, never
                            // inspected (Tom, 2026-10-26, ruling bcaa83d8).
                            record.resolution = RetryResolution::Foreign;
                            return settle(
                                record,
                                SettledListing::Foreign,
                                settle_started,
                                started,
                                timeout,
                            );
                        }
                        Some(_) => {}
                        None => {
                            return Settled {
                                listing: SettledListing::Unreadable(format!(
                                    "pid {pid} has no readable effective uid, so its unreadable descriptors cannot be settled"
                                )),
                                record: retried(record),
                            };
                        }
                    }
                    // A changed start identity means a different process now has
                    // the pid: it is re-evaluated under this same rule, inside
                    // this same bounded budget, so a churning pid can never keep
                    // the scan busy.
                    if now.identity.start != start {
                        record.identity_changes = record.identity_changes.saturating_add(1);
                        start = now.identity.start;
                    }
                    // The projected end of the next attempt is checked before the
                    // backoff is taken, and both bounds are checked again after
                    // it: a delayed wake must not buy one more listing past the
                    // settle window or past the scan's own deadline.
                    let Some(backoff) = backoffs.get(retries).copied() else {
                        return still_unreadable(record, &error);
                    };
                    match stop_before_backoff(
                        settle_started.elapsed(),
                        started.elapsed(),
                        backoff,
                        timeout,
                    ) {
                        Some(RetryStop::Unreadable) => return still_unreadable(record, &error),
                        Some(RetryStop::Bounded) => {
                            record.resolution = RetryResolution::Bounded;
                            return Settled {
                                listing: SettledListing::Bounded,
                                record: retried(record),
                            };
                        }
                        None => {}
                    }
                    std::thread::sleep(backoff);
                    retries = retries.saturating_add(1);
                    match stop_after_backoff(settle_started.elapsed(), started.elapsed(), timeout) {
                        Some(RetryStop::Unreadable) => return still_unreadable(record, &error),
                        Some(RetryStop::Bounded) => {
                            record.resolution = RetryResolution::Bounded;
                            return Settled {
                                listing: SettledListing::Bounded,
                                record: retried(record),
                            };
                        }
                        None => {}
                    }
                }
            }
        }
    }

    /// The kernel's own identity for one pid, with any identity a test states.
    ///
    /// `Ok(None)` is the kernel reporting no such process, which is the positive
    /// fact a gone process is settled on.
    fn reobserve_identity(
        &self,
        pid: u32,
        faults: &[StatedListing],
    ) -> Result<Option<KinfoProcess>> {
        let Some(mut now) = kinfo_process(pid)? else {
            return Ok(None);
        };
        if let Some(stated) = faults.iter().find(|stated| stated.pid == pid) {
            if let Some(start) = stated.start {
                now.identity.start = start;
            }
            if let Some(uid) = stated.uid {
                now.identity.effective_uid = Some(uid);
            }
        }
        Ok(Some(now))
    }

    /// One process's descriptors, as `PROC_PIDLISTFDS` answers them.
    fn list_descriptors(&self, pid: u32) -> DescriptorListing {
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
        // `proc_pidinfo` answers zero bytes on failure and leaves the reason in
        // errno, so the error is cleared first and read after: a process that
        // holds no descriptors at all also answers zero bytes. Reading zero
        // bytes as "nothing open" without that distinction would turn an
        // unreadable process into evidence of absence.
        // SAFETY: `__error` returns this thread's errno cell, which is writable
        // for this thread and outlives the call.
        unsafe { *libc::__error() = 0 };
        // SAFETY: `fds` is a writable slice whose exact byte length is passed,
        // so the kernel writes only inside it.
        let written = unsafe {
            libc::proc_pidinfo(
                libc::c_int::try_from(pid).unwrap_or(libc::c_int::MAX),
                libc::PROC_PIDLISTFDS,
                0,
                fds.as_mut_ptr().cast(),
                libc::c_int::try_from(fds_bytes).unwrap_or(libc::c_int::MAX),
            )
        };
        if written <= 0 {
            let error = io::Error::last_os_error();
            return match error.raw_os_error() {
                // The kernel reports no such process: it is positively gone, so
                // nothing it held is open.
                Some(libc::ESRCH) => DescriptorListing::Gone {
                    errno: Some(libc::ESRCH),
                },
                // No error at all: the process genuinely holds no descriptors.
                Some(0) | None => DescriptorListing::Listed(Vec::new()),
                _ => DescriptorListing::Unreadable(error),
            };
        }
        let written = written as usize;
        if written >= fds_bytes {
            // The buffer filled, so the listing cannot be complete. The caller
            // answers the per-process bound.
            return DescriptorListing::Listed(fds);
        }
        fds.truncate(written / std::mem::size_of::<libc::proc_fdinfo>());
        DescriptorListing::Listed(fds)
    }
}

/// One descriptor listing's answer for one process.
#[derive(Debug)]
enum DescriptorListing {
    /// The process's descriptors, listed inside the per-process buffer.
    Listed(Vec<libc::proc_fdinfo>),
    /// The kernel reports no such process, with the errno that said so when the
    /// listing itself answered it.
    Gone {
        /// `ESRCH`, when the listing answered it, so a retried sequence records
        /// the fact that ended it. `None` is a pid the kernel's own table no
        /// longer reports.
        errno: Option<i32>,
    },
    /// The listing could not be read, with the kernel's own reason. `EPERM` is
    /// the intermittent answer the scan re-observes; every other reason is
    /// unknown at once.
    Unreadable(io::Error),
}

impl DescriptorListing {
    /// The listing one errno stands for, classified the way `PROC_PIDLISTFDS` is
    /// classified, so a stated answer means exactly what the kernel's own would.
    fn from_errno(errno: i32) -> Self {
        match errno {
            // The kernel reports no such process: positively gone.
            libc::ESRCH => Self::Gone {
                errno: Some(libc::ESRCH),
            },
            // No error at all: the process holds no descriptors.
            0 => Self::Listed(Vec::new()),
            _ => Self::Unreadable(io::Error::from_raw_os_error(errno)),
        }
    }
}

/// What one same-uid process's descriptor listing settled to, and the bounded
/// record of the re-observation it took to get there.
#[derive(Debug)]
struct Settled {
    listing: SettledListing,
    record: Option<RetriedUnreadable>,
}

/// A settled descriptor listing. Only [`Listed`](Self::Listed) carries
/// descriptors; every other answer is a resolved fact, never an assumption.
#[derive(Debug)]
enum SettledListing {
    Listed(Vec<libc::proc_fdinfo>),
    Gone,
    Foreign,
    Bounded,
    /// The observation cannot be completed, for this reason. The bounded retry
    /// record travels beside it.
    Unreadable(String),
}

/// What a test states about one process's descriptor listing.
///
/// The production scan states nothing: [`MacOs::scan_open_descriptors`] passes an
/// empty list, so every answer a real scan acts on comes from the kernel. A test
/// states the errno sequence the listing gives and, when it needs one, a
/// different start identity or effective uid for the re-observation, which is how
/// the intermittent, persistent and reused-pid rules are exercised without the
/// kernel having to produce them.
#[derive(Debug, Clone, Default)]
pub(crate) struct StatedListing {
    /// The pid the statement is about.
    pub(crate) pid: u32,
    /// The errno of each descriptor listing, in order, before the kernel's own
    /// answer. `EPERM` is the answer the scan re-observes; any other entry is
    /// refused at once, exactly as a real one would be.
    pub(crate) errnos: Vec<i32>,
    /// A start identity the re-observation reports instead of the kernel's.
    pub(crate) start: Option<u64>,
    /// An effective uid the re-observation reports instead of the kernel's.
    pub(crate) uid: Option<u32>,
}

/// The errno a test states for this process's next listing, consuming it. A
/// production scan states nothing, so it always answers `None` and every listing
/// comes from the kernel.
fn stated_errno(faults: &mut [StatedListing], pid: u32) -> Option<i32> {
    let stated = faults.iter_mut().find(|stated| stated.pid == pid)?;
    if stated.errnos.is_empty() {
        return None;
    }
    Some(stated.errnos.remove(0))
}

/// Why one process's bounded re-observation has to stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetryStop {
    /// The same identity is still unreadable after the retries it was given.
    Unreadable,
    /// One of the two deadlines would be crossed: the observation is bounded.
    Bounded,
}

/// Whether the backoff before the next listing may be taken.
///
/// Both deadlines are projected forward, not only the retry count: the settle
/// window must still hold when the backoff ends, and so must the scan's own time
/// bound.
fn stop_before_backoff(
    settle_elapsed: Duration,
    scan_elapsed: Duration,
    backoff: Duration,
    timeout: Duration,
) -> Option<RetryStop> {
    if settle_elapsed.saturating_add(backoff) >= EPERM_SETTLE_WINDOW {
        return Some(RetryStop::Unreadable);
    }
    if scan_elapsed.saturating_add(backoff) >= timeout {
        return Some(RetryStop::Bounded);
    }
    None
}

/// Whether the next listing may run after the backoff.
///
/// A wake that arrives late is checked against both deadlines again, so a
/// delayed sleep never buys one more listing past the settle window or past the
/// scan's own time bound.
fn stop_after_backoff(
    settle_elapsed: Duration,
    scan_elapsed: Duration,
    timeout: Duration,
) -> Option<RetryStop> {
    if settle_elapsed >= EPERM_SETTLE_WINDOW {
        return Some(RetryStop::Unreadable);
    }
    if scan_elapsed >= timeout {
        return Some(RetryStop::Bounded);
    }
    None
}

/// Whether one process's terminal answer may be accepted.
///
/// The read that produced it may itself have been slow, so both deadlines are
/// checked again here: a re-observation that finishes after its 250 ms settle
/// window, or after the scan's own time bound, is not an answer. The settle
/// window is the per-process bound, so it refuses; the scan's bound makes the
/// whole scan bounded.
fn stop_when_settled(
    settle_elapsed: Duration,
    scan_elapsed: Duration,
    timeout: Duration,
) -> Option<RetryStop> {
    if settle_elapsed >= EPERM_SETTLE_WINDOW {
        return Some(RetryStop::Unreadable);
    }
    if scan_elapsed >= timeout {
        return Some(RetryStop::Bounded);
    }
    None
}

/// One terminal answer from [`MacOs::settle_descriptors`], accepted only while
/// both deadlines still hold.
///
/// An answer read after the scan's own deadline makes the scan bounded; one read
/// after the process's settle window closes is a refusal, because the bounded
/// re-observation did not finish inside its bound. Either way the scan never
/// accepts an observation from outside the bounds it states, and the record of
/// what was tried travels with the refusal.
fn settle(
    mut record: RetriedUnreadable,
    listing: SettledListing,
    settle_started: Instant,
    started: Instant,
    timeout: Duration,
) -> Settled {
    match stop_when_settled(settle_started.elapsed(), started.elapsed(), timeout) {
        Some(RetryStop::Unreadable) => {
            record.resolution = RetryResolution::Bounded;
            return Settled {
                listing: SettledListing::Unreadable(window_closed(&record)),
                record: retried(record),
            };
        }
        Some(RetryStop::Bounded) => {
            record.resolution = RetryResolution::Bounded;
            return Settled {
                listing: SettledListing::Bounded,
                record: retried(record),
            };
        }
        None => {}
    }
    Settled {
        listing,
        record: retried(record),
    }
}

/// The refusal for a re-observation that produced an answer after its settle
/// window had closed: the answer is not evidence, because the bounded
/// re-observation did not finish inside its bound.
fn window_closed(record: &RetriedUnreadable) -> String {
    let described = match record.command.as_deref() {
        Some(command) => format!("`{command}`"),
        None => "a process".to_owned(),
    };
    format!(
        "the re-observation of {described} (pid {}, start {}) produced its answer after its {} ms settle window closed, so it is not evidence",
        record.pid,
        record.start,
        EPERM_SETTLE_WINDOW.as_millis()
    )
}

/// The bounded record of one process's re-observation, or `None` when its
/// listing never answered `EPERM`.
fn retried(record: RetriedUnreadable) -> Option<RetriedUnreadable> {
    (!record.errnos.is_empty()).then_some(record)
}

/// The refusal for a same-uid process whose descriptor listing still answers
/// `EPERM` after its bounded re-observation.
///
/// The errno sequence it was given, the identity it belongs to and the command
/// are in the message and in the record beside it, because unknown is a refusal
/// and its evidence is what lets the next native diagnostic tell an intermittent
/// answer from a persistent one.
fn still_unreadable(record: RetriedUnreadable, error: &io::Error) -> Settled {
    let described = match record.command.as_deref() {
        Some(command) => format!("`{command}`"),
        None => "an unnamed process".to_owned(),
    };
    let retries = record.errnos.len().saturating_sub(1);
    let reason = format!(
        "the open descriptors of {described} (pid {}, start {}) are still unreadable: {error} on {} attempts (retries: {retries})",
        record.pid,
        record.start,
        record.errnos.len()
    );
    Settled {
        listing: SettledListing::Unreadable(reason),
        record: retried(record),
    }
}

/// How many processes the scan reads from the kernel's table before it answers
/// bounded rather than complete.
const SCAN_PID_CAP: usize = 8192;

/// `sizeof(struct kinfo_proc)` from `<sys/sysctl.h>` on this platform, and the
/// offsets of the five fields this layer reads. The struct between them is left
/// opaque: this build reads the start time (a `timeval` at the head of the
/// exported union), the pid, the command name, the effective uid and nothing
/// else.
///
/// A hard-coded table of offsets is only safe because the read checks itself:
/// the entry for this very process must report the pid, effective uid and start
/// identity that `getpid`, `geteuid` and `PROC_PIDTBSDINFO` independently
/// report. A table that disagrees is refused as unknown state, never parsed
/// further into a wrong uid.
const KINFO_PROC_SIZE: usize = 648;
const KINFO_START_SEC_OFFSET: usize = 0;
const KINFO_START_USEC_OFFSET: usize = 8;
const KINFO_PID_OFFSET: usize = 40;
const KINFO_COMMAND_OFFSET: usize = 243;
const KINFO_COMMAND_BYTES: usize = 17;
const KINFO_UID_OFFSET: usize = 420;

/// One process's identity and command name from the kernel's own table.
///
/// The identity is what a re-observation decides on; the command name is recorded
/// only for a process whose descriptor listing answered `EPERM`, which is why it
/// is read for that one pid rather than for every process in a scan.
#[derive(Debug)]
struct KinfoProcess {
    identity: ProcessIdentity,
    command: String,
}

/// One process's kernel identity through `KERN_PROC_PID`, or `Ok(None)` when the
/// kernel reports no such process.
///
/// This is the positive fact an intermittent `EPERM` is settled against. The
/// table reports every process's pid, start time and effective uid without
/// inspecting the process, so a pid that now names another account's process is
/// classified rather than refused (ruling bcaa83d8).
fn kinfo_process(pid: u32) -> Result<Option<KinfoProcess>> {
    let mut mib = [
        libc::CTL_KERN,
        libc::KERN_PROC,
        libc::KERN_PROC_PID,
        libc::c_int::try_from(pid).unwrap_or(libc::c_int::MAX),
    ];
    let mut size = 0usize;
    // SAFETY: the size query passes a null buffer with a writable size, which is
    // the documented first half of a `sysctl` read.
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            u32::try_from(mib.len()).unwrap_or(u32::MAX),
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return Err(process_table_error("for one pid could not be read"));
    }
    // The size query's answer is an estimate: this platform returns room for a
    // few entries even for one pid, and no bytes for a pid it does not have. The
    // read below is what says how many entries the kernel actually wrote, so the
    // buffer is that estimate with room for the one entry that is asked for.
    let mut buffer = vec![0u8; size.max(KINFO_PROC_SIZE)];
    let mut written = buffer.len();
    // SAFETY: `buffer` is a writable allocation of exactly `written` bytes and
    // `mib` describes the one-pid request, so the kernel writes only inside it.
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            u32::try_from(mib.len()).unwrap_or(u32::MAX),
            buffer.as_mut_ptr().cast(),
            &mut written,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return Err(process_table_error("for one pid could not be read"));
    }
    // The process can exit between the size query and the read, and a pid the
    // kernel does not have answers no bytes at all.
    if written == 0 {
        return Ok(None);
    }
    if written != KINFO_PROC_SIZE {
        return Err(Error::new(
            ErrorKind::UnknownState,
            format!(
                "the kernel returned {written} bytes for pid {pid}, not one {KINFO_PROC_SIZE}-byte entry"
            ),
        )
        .with_detail("pid", u64::from(pid).into()));
    }
    let identity = parse_identity(&buffer)?;
    if identity.pid != pid {
        return Err(Error::new(
            ErrorKind::UnknownState,
            format!(
                "the kernel answered pid {pid} with the entry of pid {}",
                identity.pid
            ),
        )
        .with_detail("pid", u64::from(pid).into())
        .with_detail("answered_pid", u64::from(identity.pid).into()));
    }
    Ok(Some(KinfoProcess {
        identity,
        command: entry_command(&buffer),
    }))
}

impl MacOs {
    /// The kernel's own process table, read through `sysctl KERN_PROC_ALL`.
    ///
    /// This is the unprivileged fact custody classifies with: the effective uid
    /// and start identity of every process, obtained without inspecting any
    /// process. It is what makes another account's process a *named* blind spot
    /// instead of an unreadable fact, because `PROC_PIDTBSDINFO` and
    /// `PROC_PIDLISTFDS` answer `EPERM` for those processes.
    ///
    /// `Ok(None)` is a table past its bound. An unreadable table, a table that is
    /// not a whole number of entries, and a table that does not describe this
    /// process as itself are errors, never an empty table.
    fn process_table(&self) -> Result<Option<Vec<TableRow>>> {
        let mut mib = [libc::CTL_KERN, libc::KERN_PROC, libc::KERN_PROC_ALL];
        let mut size = 0usize;
        // SAFETY: the size query passes a null buffer with a writable size, which
        // is the documented first half of a `sysctl` read.
        if unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                u32::try_from(mib.len()).unwrap_or(u32::MAX),
                std::ptr::null_mut(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        } != 0
        {
            return Err(process_table_error("its size could not be read"));
        }
        if size == 0 || !size.is_multiple_of(KINFO_PROC_SIZE) {
            return Err(Error::new(
                ErrorKind::UnknownState,
                format!(
                    "the process table is {size} bytes, which is not a whole number of {KINFO_PROC_SIZE}-byte entries"
                ),
            )
            .with_detail("fact", "process table".into()));
        }
        // Slack for the processes that appear between the size query and the
        // read, so a busy machine does not make every scan fail.
        let want = size.saturating_add(KINFO_PROC_SIZE.saturating_mul(64));
        let mut buffer = vec![0u8; want];
        let mut written = want;
        // SAFETY: `buffer` is a writable allocation of exactly `written` bytes
        // and `mib` describes the table request, so the kernel writes only
        // inside it.
        if unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                u32::try_from(mib.len()).unwrap_or(u32::MAX),
                buffer.as_mut_ptr().cast(),
                &mut written,
                std::ptr::null_mut(),
                0,
            )
        } != 0
        {
            return Err(process_table_error("could not be read"));
        }
        if written > want || !written.is_multiple_of(KINFO_PROC_SIZE) {
            return Err(Error::new(
                ErrorKind::UnknownState,
                format!(
                    "the process table returned {written} bytes, which is not a whole number of {KINFO_PROC_SIZE}-byte entries"
                ),
            )
            .with_detail("fact", "process table".into()));
        }
        let count = written / KINFO_PROC_SIZE;
        if count > SCAN_PID_CAP {
            return Ok(None);
        }
        let mut rows = Vec::with_capacity(count);
        for index in 0..count {
            let start = index.saturating_mul(KINFO_PROC_SIZE);
            let entry = buffer
                .get(start..start.saturating_add(KINFO_PROC_SIZE))
                .ok_or_else(|| {
                    Error::new(ErrorKind::UnknownState, "the process table entry is short")
                })?;
            let row = TableRow::parse(entry)?;
            if row.identity.pid == 0 {
                continue;
            }
            rows.push(row);
        }
        verify_own_row(&rows, self.effective_uid()?)?;
        Ok(Some(rows))
    }

    /// The kernel's table with each process's command name, for the tests that
    /// name a real process such as `/usr/bin/login` without inspecting it.
    #[cfg(test)]
    pub(crate) fn observed_processes(&self) -> Result<Option<Vec<(ProcessIdentity, String)>>> {
        Ok(self.process_table()?.map(|rows| {
            rows.into_iter()
                .map(|row| (row.identity, row.command))
                .collect()
        }))
    }
}

/// One process in the kernel's table: the identity the scan classifies with, and
/// the command name, which only the tests read.
struct TableRow {
    identity: ProcessIdentity,
    /// The process's command name. It exists so a test can name a real process
    /// such as `/usr/bin/login` without inspecting it, so it is parsed only for
    /// tests: a production scan allocates no name it would never read.
    #[cfg(test)]
    command: String,
}

impl TableRow {
    /// One entry, at the offsets this platform's `<sys/sysctl.h>` gives them.
    fn parse(entry: &[u8]) -> Result<Self> {
        Ok(Self {
            identity: parse_identity(entry)?,
            #[cfg(test)]
            command: entry_command(entry),
        })
    }
}

/// The identity fields of one table entry, at the offsets above.
fn parse_identity(entry: &[u8]) -> Result<ProcessIdentity> {
    let start_sec = entry_i64(entry, KINFO_START_SEC_OFFSET)?;
    let start_usec = entry_i32(entry, KINFO_START_USEC_OFFSET)?;
    let pid = entry_i32(entry, KINFO_PID_OFFSET)?;
    let uid = entry_u32(entry, KINFO_UID_OFFSET)?;
    Ok(ProcessIdentity {
        pid: u32::try_from(pid).unwrap_or(0),
        effective_uid: Some(uid),
        start: u64::try_from(start_sec)
            .unwrap_or(0)
            .saturating_mul(1_000_000)
            .saturating_add(u64::try_from(start_usec).unwrap_or(0)),
    })
}

/// One entry's command name, read for a test that names a process and for the
/// bounded record of a retried `EPERM`.
fn entry_command(entry: &[u8]) -> String {
    entry
        .get(KINFO_COMMAND_OFFSET..KINFO_COMMAND_OFFSET.saturating_add(KINFO_COMMAND_BYTES))
        .map(|bytes| {
            let end = bytes
                .iter()
                .position(|byte| *byte == 0)
                .unwrap_or(bytes.len());
            String::from_utf8_lossy(&bytes[..end]).into_owned()
        })
        .unwrap_or_default()
}

fn entry_i32(entry: &[u8], offset: usize) -> Result<i32> {
    let bytes: [u8; 4] = entry
        .get(offset..offset.saturating_add(4))
        .and_then(|bytes| <[u8; 4]>::try_from(bytes).ok())
        .ok_or_else(|| short_entry(offset))?;
    Ok(i32::from_ne_bytes(bytes))
}

fn entry_u32(entry: &[u8], offset: usize) -> Result<u32> {
    let bytes: [u8; 4] = entry
        .get(offset..offset.saturating_add(4))
        .and_then(|bytes| <[u8; 4]>::try_from(bytes).ok())
        .ok_or_else(|| short_entry(offset))?;
    Ok(u32::from_ne_bytes(bytes))
}

fn entry_i64(entry: &[u8], offset: usize) -> Result<i64> {
    let bytes: [u8; 8] = entry
        .get(offset..offset.saturating_add(8))
        .and_then(|bytes| <[u8; 8]>::try_from(bytes).ok())
        .ok_or_else(|| short_entry(offset))?;
    Ok(i64::from_ne_bytes(bytes))
}

fn short_entry(offset: usize) -> Error {
    Error::new(
        ErrorKind::UnknownState,
        format!("the process table entry ends before the field at offset {offset}"),
    )
    .with_detail("fact", "process table".into())
}

fn process_table_error(reason: &str) -> Error {
    Error::new(
        ErrorKind::UnknownState,
        format!("the process table {reason}: {}", io::Error::last_os_error()),
    )
    .with_detail("fact", "process table".into())
}

/// Refuse a table whose own view of this process is not this process.
///
/// The offsets above are the SDK's, and this is what keeps a wrong one from
/// becoming a wrong uid: the entry for this pid must report this account's
/// effective uid and the start identity the kernel reports for it through
/// `PROC_PIDTBSDINFO`, which is readable for oneself on every platform this
/// crate builds for.
fn verify_own_row(rows: &[TableRow], uid: u32) -> Result<()> {
    let pid = std::process::id();
    let expected_start = bsd_info(pid)?.map(|info| bsd_start(&info));
    let Some(row) = rows.iter().find(|row| row.identity.pid == pid) else {
        return Err(Error::new(
            ErrorKind::UnknownState,
            format!("the process table does not report pid {pid} at all"),
        )
        .with_detail("pid", u64::from(pid).into()));
    };
    if row.identity.effective_uid != Some(uid) || Some(row.identity.start) != expected_start {
        return Err(Error::new(
            ErrorKind::UnknownState,
            format!(
                "the process table reports pid {pid} as uid {:?} start {}, not this process's uid {uid} start {}",
                row.identity.effective_uid,
                row.identity.start,
                expected_start.unwrap_or(0)
            ),
        )
        .with_detail("pid", u64::from(pid).into()));
    }
    Ok(())
}

/// One process's start identity, from the facts already read for it.
fn bsd_start(info: &libc::proc_bsdinfo) -> u64 {
    info.pbi_start_tvsec
        .saturating_mul(1_000_000)
        .saturating_add(info.pbi_start_tvusec)
}

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
        // `dup` shares the directory offset with the caller's handle, and a
        // directory stream reads from that offset, so a second listing through the
        // same handle would otherwise start where the first ended and answer empty.
        // A listing is always the directory's names from the first one.
        // SAFETY: `stream` is the directory stream created above.
        unsafe {
            libc::rewinddir(stream);
        }
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

    fn rename_child(&self, parent: &DirHandle, from: &str, to: &str) -> Result<RenameOutcome> {
        let directory = parent_descriptor(parent, from)?;
        let source = name_cstring(from)?;
        let target = name_cstring(to)?;
        // SAFETY: both names are NUL-terminated components inside the same open
        // directory, which is what makes the rename atomic, and `RENAME_EXCL` is
        // libc's own flag for the platform's exclusive rename. The exclusive
        // form never replaces an existing target name.
        if unsafe {
            libc::renameatx_np(
                directory,
                source.as_ptr(),
                directory,
                target.as_ptr(),
                libc::RENAME_EXCL,
            )
        } == 0
        {
            return Ok(RenameOutcome::Renamed);
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EEXIST) {
            // The target name is taken. Nothing was renamed, so the caller
            // decides: a capture retries with a fresh private name, and a
            // put-back reports both names and stops.
            return Ok(RenameOutcome::TargetExists);
        }
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

    fn set_dir_mode(&self, handle: &DirHandle, mode: u32) -> Result<()> {
        let descriptor = handle.descriptor().ok_or_else(|| {
            Error::new(
                ErrorKind::Usage,
                "the handle has no open descriptor to chmod".to_owned(),
            )
        })?;
        // SAFETY: the descriptor is open for this call and `mode` is one of the
        // permission bits `fchmod` takes. It acts on the opened directory itself,
        // so no link is followed and no name is resolved again.
        if unsafe { libc::fchmod(descriptor.as_raw_fd(), mode as libc::mode_t) } != 0 {
            return Err(Error::new(
                ErrorKind::Io,
                format!("cannot set the directory mode to {mode:#o}"),
            )
            .with_detail("mode", mode.to_string().into())
            .with_detail("os_error", io::Error::last_os_error().to_string().into()));
        }
        Ok(())
    }

    fn recheck_dir(&self, handle: &DirHandle) -> Result<DirId> {
        let descriptor = handle.descriptor().ok_or_else(|| {
            Error::new(
                ErrorKind::Usage,
                "the handle has no open descriptor to re-read".to_owned(),
            )
        })?;
        read_dir_identity(descriptor, Path::new("recheck"))
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::time::Duration;

    use super::{
        stop_after_backoff, stop_before_backoff, stop_when_settled, RetryStop, EPERM_SETTLE_WINDOW,
    };

    /// A scan deadline well clear of the settle window, so the two bounds are
    /// told apart.
    const SCAN_TIMEOUT: Duration = Duration::from_secs(10);

    #[test]
    fn the_first_retry_is_taken_inside_both_bounds() {
        assert_eq!(
            stop_before_backoff(
                Duration::ZERO,
                Duration::from_millis(3),
                Duration::from_millis(25),
                SCAN_TIMEOUT
            ),
            None
        );
        assert_eq!(
            stop_after_backoff(
                Duration::from_millis(25),
                Duration::from_millis(28),
                SCAN_TIMEOUT
            ),
            None
        );
    }

    #[test]
    fn a_backoff_that_would_end_past_the_settle_window_is_not_taken() {
        // The projection is checked before the sleep: a retry that cannot finish
        // inside the window is not started.
        assert_eq!(
            stop_before_backoff(
                EPERM_SETTLE_WINDOW.saturating_sub(Duration::from_millis(10)),
                Duration::ZERO,
                Duration::from_millis(25),
                SCAN_TIMEOUT
            ),
            Some(RetryStop::Unreadable)
        );
    }

    #[test]
    fn a_backoff_that_would_end_past_the_scan_deadline_is_not_taken() {
        assert_eq!(
            stop_before_backoff(
                Duration::ZERO,
                Duration::from_millis(9_990),
                Duration::from_millis(25),
                SCAN_TIMEOUT
            ),
            Some(RetryStop::Bounded)
        );
    }

    #[test]
    fn a_delayed_wake_past_the_settle_window_stops_the_retry() {
        // The wake is checked again: a sleep that returned after the window had
        // closed must not buy one more listing, even though the projection
        // before it passed.
        assert_eq!(
            stop_after_backoff(EPERM_SETTLE_WINDOW, Duration::from_millis(30), SCAN_TIMEOUT),
            Some(RetryStop::Unreadable)
        );
    }

    #[test]
    fn a_delayed_wake_past_the_scan_deadline_is_bounded() {
        assert_eq!(
            stop_after_backoff(Duration::from_millis(100), SCAN_TIMEOUT, SCAN_TIMEOUT),
            Some(RetryStop::Bounded)
        );
    }

    #[test]
    fn a_terminal_answer_inside_both_deadlines_is_accepted() {
        assert_eq!(
            stop_when_settled(
                Duration::from_millis(200),
                Duration::from_millis(3),
                SCAN_TIMEOUT
            ),
            None
        );
    }

    #[test]
    fn a_terminal_answer_read_after_the_settle_window_is_not_accepted() {
        // The read that produced the answer may itself have been slow: the
        // per-process window is checked again when the answer arrives, so a
        // retry or a read begun inside the window cannot resolve after it.
        assert_eq!(
            stop_when_settled(EPERM_SETTLE_WINDOW, Duration::from_millis(30), SCAN_TIMEOUT),
            Some(RetryStop::Unreadable)
        );
    }

    #[test]
    fn a_terminal_answer_read_after_the_scan_deadline_is_bounded() {
        assert_eq!(
            stop_when_settled(Duration::from_millis(20), SCAN_TIMEOUT, SCAN_TIMEOUT),
            Some(RetryStop::Bounded)
        );
    }
}
