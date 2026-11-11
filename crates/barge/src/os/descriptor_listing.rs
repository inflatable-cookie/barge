//! The shared bounded listing algorithm. The macOS adapter supplies kernel
//! identity/read operations; tests replace those operations, never this policy.

use std::io;
use std::time::{Duration, Instant};

use super::facts::ProcessIdentity;

pub(super) const MAX_PROCESSES: usize = 4096;
pub(super) const MAX_ROWS: usize = 65_536;
pub(super) const MAX_TIME: Duration = Duration::from_secs(10);
const INITIAL_ROWS: usize = 4096;

#[derive(Debug)]
pub(super) struct ScanBudget {
    pub(super) started: Instant,
    pub(super) timeout: Duration,
    deadline: Instant,
    pub(super) remaining: usize,
    #[cfg(any(test, feature = "proof-descriptor-listing"))]
    pub(super) attempts: Vec<Attempt>,
}

#[cfg(any(test, feature = "proof-descriptor-listing"))]
#[derive(Debug)]
pub(super) struct Attempt {
    pub(super) capacity: usize,
    pub(super) rows: Option<usize>,
    pub(super) outcome: &'static str,
    pub(super) before: ProcessIdentity,
    pub(super) after: Option<ProcessIdentity>,
}

impl ScanBudget {
    pub(super) fn new(started: Instant, rows: usize, timeout: Duration) -> Option<Self> {
        let timeout = timeout.min(MAX_TIME);
        Some(Self {
            started,
            timeout,
            deadline: started.checked_add(timeout)?,
            remaining: rows.min(MAX_ROWS),
            #[cfg(any(test, feature = "proof-descriptor-listing"))]
            attempts: Vec::new(),
        })
    }

    pub(super) fn expired(&self) -> bool {
        Instant::now() >= self.deadline
    }

    fn stop(&self, now: Instant, local: Option<Instant>) -> Option<Listing> {
        // Preserve the existing local-window refusal precedence.
        if local.is_some_and(|deadline| now >= deadline) {
            return Some(unreadable("the descriptor listing settle window closed"));
        }
        (now >= self.deadline).then_some(Listing::Bounded("time"))
    }
}

#[derive(Debug)]
pub(super) enum Listing {
    Complete {
        identity: ProcessIdentity,
        fds: Vec<libc::proc_fdinfo>,
        #[cfg_attr(not(feature = "proof-descriptor-listing"), allow(dead_code))]
        capacity: usize,
    },
    Bounded(&'static str),
    Gone {
        errno: Option<i32>,
    },
    Unreadable(io::Error),
}

/// Only platform operations are substitutable. All allocation, validation,
/// charging, growth, identity and deadline decisions stay in `list`.
pub(super) trait Source {
    fn identity(&mut self, pid: u32) -> io::Result<Option<ProcessIdentity>>;
    fn read(&mut self, pid: libc::c_int, fds: &mut [libc::proc_fdinfo]) -> (i32, i32);
    fn now(&mut self) -> Instant {
        Instant::now()
    }
    /// A test can fail or delay this operation; allocation itself stays shared.
    fn before_allocate(&mut self, _capacity: usize) -> io::Result<()> {
        Ok(())
    }
}

fn allocate(capacity: usize) -> io::Result<Vec<libc::proc_fdinfo>> {
    let mut fds = Vec::new();
    fds.try_reserve_exact(capacity).map_err(io::Error::other)?;
    fds.resize(
        capacity,
        libc::proc_fdinfo {
            proc_fd: 0,
            proc_fdtype: 0,
        },
    );
    Ok(fds)
}

fn unreadable(reason: &'static str) -> Listing {
    Listing::Unreadable(io::Error::other(reason))
}

/// Exact conversions are checked before allocation or an external read.
pub(super) fn arguments(pid: u32, capacity: usize) -> Option<(libc::c_int, libc::c_int)> {
    if pid == 0 || capacity == 0 {
        return None;
    }
    Some((
        libc::c_int::try_from(pid).ok()?,
        libc::c_int::try_from(capacity.checked_mul(std::mem::size_of::<libc::proc_fdinfo>())?)
            .ok()?,
    ))
}

pub(super) fn list(
    source: &mut impl Source,
    classified: ProcessIdentity,
    budget: &mut ScanBudget,
    local_deadline: Option<Instant>,
) -> Listing {
    let mut capacity = INITIAL_ROWS.min(budget.remaining);
    loop {
        if let Some(stop) = budget.stop(source.now(), local_deadline) {
            return stop;
        }
        if capacity == 0 {
            return Listing::Bounded("descriptor work");
        }
        let Some((pid, requested_bytes)) = arguments(classified.pid, capacity) else {
            return unreadable(
                "descriptor listing PID, capacity or byte count is not representable",
            );
        };
        let before =
            match source.identity(classified.pid) {
                Ok(None) => return Listing::Gone { errno: None },
                Ok(Some(identity)) if same_identity(identity, classified) => identity,
                Ok(Some(_)) => return unreadable(
                    "descriptor listing process identity or effective uid changed before the read",
                ),
                Err(error) => return Listing::Unreadable(error),
            };
        if let Some(stop) = budget.stop(source.now(), local_deadline) {
            return stop;
        }
        let mut fds = match source
            .before_allocate(capacity)
            .and_then(|()| allocate(capacity))
        {
            Ok(fds) => fds,
            Err(error) => return Listing::Unreadable(error),
        };
        if let Some(stop) = budget.stop(source.now(), local_deadline) {
            return stop;
        }
        #[cfg(test)]
        crate::tape::scalar(
            "MacOs::list_descriptors",
            "listing_capacity",
            crate::tape::Identity::none(),
            &capacity.to_string(),
        );
        let (written, errno) = source.read(pid, &mut fds);
        let rows = usize::try_from(written)
            .ok()
            .filter(|bytes| {
                *bytes <= requested_bytes as usize
                    && *bytes % std::mem::size_of::<libc::proc_fdinfo>() == 0
            })
            .map(|bytes| bytes / std::mem::size_of::<libc::proc_fdinfo>());
        // Charge every structurally returned row, even when the answer has an
        // ambiguous errno or malformed entries. Failed/invalid byte counts
        // refuse immediately and cannot buy another attempt.
        if let Some(rows) = rows {
            budget.remaining -= rows;
        }
        let valid_bytes = written >= 0 && rows.is_some();
        let valid_errno = errno == 0;
        #[cfg(test)]
        {
            let outcome = if written <= 0 && errno != 0 {
                if errno == libc::ESRCH {
                    crate::tape::Outcome::answer("gone")
                } else {
                    crate::tape::Outcome::refused(crate::tape::errno_class(
                        &io::Error::from_raw_os_error(errno),
                    ))
                }
            } else if valid_bytes && valid_errno {
                let rows = rows.unwrap_or(0);
                for fd in fds.iter().take(rows) {
                    crate::tape::fd_entry(
                        "MacOs::list_descriptors",
                        classified.pid,
                        fd.proc_fd,
                        fd.proc_fdtype,
                    );
                }
                crate::tape::Outcome::listed(rows, rows, rows == capacity)
            } else {
                crate::tape::Outcome::refused("invalid_listing_bytes_or_errno")
            };
            crate::tape::fd_list("MacOs::list_descriptors", classified.pid, outcome);
        }
        // Failure precedence: neither a later gone identity nor a deadline
        // turns a failed or malformed answer into absence.
        let failure = if written <= 0 && errno != 0 {
            Some(if errno == libc::ESRCH {
                Listing::Gone { errno: Some(errno) }
            } else {
                Listing::Unreadable(io::Error::from_raw_os_error(errno))
            })
        } else if !valid_bytes || !valid_errno {
            Some(unreadable(
                "descriptor listing returned an invalid byte count or ambiguous errno",
            ))
        } else {
            fds.truncate(rows.unwrap_or(0));
            fds.sort_unstable_by_key(|fd| fd.proc_fd);
            if fds.iter().any(|fd| fd.proc_fd < 0)
                || fds
                    .windows(2)
                    .any(|pair| pair[0].proc_fd == pair[1].proc_fd)
            {
                Some(unreadable(
                    "descriptor listing returned a negative or duplicate descriptor",
                ))
            } else {
                None
            }
        };
        #[cfg(any(test, feature = "proof-descriptor-listing"))]
        let attempt_index = {
            let index = budget.attempts.len();
            budget.attempts.push(Attempt {
                capacity,
                rows,
                before,
                after: None,
                outcome: if failure.is_some() {
                    "refused"
                } else if rows == Some(capacity) {
                    "full"
                } else {
                    "short"
                },
            });
            index
        };
        // Bracket every attempted read while time remains, including errors.
        // A later gone/failed identity can never erase the original failure.
        let stop_after_read = budget.stop(source.now(), local_deadline);
        let after = if stop_after_read.is_none() {
            source.identity(classified.pid)
        } else {
            Ok(None)
        };
        #[cfg(any(test, feature = "proof-descriptor-listing"))]
        if let Ok(Some(after)) = &after {
            budget.attempts[attempt_index].after = Some(*after);
        }
        if let Some(failure) = failure {
            return failure;
        }
        if let Some(stop) = stop_after_read {
            return stop;
        }
        // Blocking calls cannot be preempted. Check again after final identity,
        // before accepting even an empty or last-process answer.
        if let Some(stop) = budget.stop(source.now(), local_deadline) {
            return stop;
        }
        match after {
            Ok(None) => return Listing::Gone { errno: None },
            Ok(Some(after)) if same_identity(after, before) => {}
            Ok(Some(_)) => {
                return unreadable(
                    "descriptor listing process identity or effective uid changed around the read",
                )
            }
            Err(error) => return Listing::Unreadable(error),
        }
        if fds.len() < capacity {
            return Listing::Complete {
                identity: before,
                fds,
                capacity,
            };
        }
        // No copy and no union of observations. The previous full prefix is
        // charged and dropped before its successor can allocate.
        drop(fds);
        let Some(next) = capacity
            .checked_mul(2)
            .map(|next| next.min(budget.remaining))
        else {
            return unreadable("descriptor listing growth overflowed");
        };
        if next <= capacity {
            return Listing::Bounded("descriptor work");
        }
        capacity = next;
    }
}

fn same_identity(a: ProcessIdentity, b: ProcessIdentity) -> bool {
    a.pid == b.pid
        && a.start == b.start
        && a.effective_uid.is_some()
        && a.effective_uid == b.effective_uid
}

#[cfg(test)]
#[path = "descriptor_listing_tests.rs"]
mod tests;
