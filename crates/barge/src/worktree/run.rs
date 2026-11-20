//! Holding a leased checkout for a command: the seam a non-Rust host uses
//! (`barge worktree run`). The hold is taken without waiting, then its descriptor
//! is kept inheritable and the process is replaced by the command, in the
//! checkout. The hold lives as long as the command and any child that keeps the
//! descriptor. Barge scans no process and writes no record for it.
//!
//! A return holds its root exclusively while it runs, so a hold that finds the
//! root held is refused rather than queued behind the return.

use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;

use super::leases::{self, WorktreeLease, WorktreeLeaseState};
use super::pools;
use super::returns::pool_for_slug;
use super::tree_path;
use crate::error::{Error, Result};
use crate::hold::{Hold, Mode};
use crate::Barge;

/// A leased checkout held for a command. Holding it proves the lease was
/// `leased` when the hold was taken, and that no return held the root then.
#[derive(Debug)]
pub struct WorktreeHold {
    lease: WorktreeLease,
    hold: Hold,
}

impl WorktreeHold {
    /// The lease the hold is for.
    pub fn lease(&self) -> &WorktreeLease {
        &self.lease
    }

    /// Replace this process with `program` in the lease's checkout, keeping the
    /// hold on the descriptor a child inherits. Returns only when the command
    /// could not start, with the reason; nothing was run then.
    pub fn exec(self, program: &str, arguments: &[String]) -> Error {
        let Self { lease, hold } = self;
        let hold = match hold.into_inheritable() {
            Ok(hold) => hold,
            Err(error) => return error,
        };
        let source = Command::new(program)
            .args(arguments)
            .current_dir(&lease.path)
            .exec();
        // Only a failed exec gets here: the hold is released as it drops.
        drop(hold);
        if source.kind() == std::io::ErrorKind::NotFound {
            return Error::usage(format!("cannot run {program}: {source}"));
        }
        Error::io("cannot run", Path::new(program), &source)
    }
}

/// Take a hold for a lease's root, if its lease is `leased` and no return holds
/// the root. `mode` is shared by default; exclusive refuses every other holder.
pub(crate) fn hold(barge: &Barge, lease_id: &str, mode: Mode) -> Result<WorktreeHold> {
    let state = barge.core.state.root();
    // A lease's root never changes, so this first read only finds the root. The
    // state is read again under the hold, which is the read that counts.
    let first = leases::read_lease(state, lease_id)?
        .ok_or_else(|| Error::not_found(format!("no lease {lease_id}")))?;
    require_leased(&first)?;
    let pools = pools::read_pools(state)?;
    let pool = pool_for_slug(&pools, &first.pool)?.ok_or_else(|| {
        Error::unknown_state(format!(
            "lease {lease_id} is in pool {}, which is no longer declared; nothing was run",
            first.pool
        ))
    })?;
    let area = barge
        .declared_pool_area(pool)?
        .ok_or_else(|| Error::unknown_state(format!("pool {} has no area", first.pool)))?;
    let Some(hold) = area.try_hold(&first.root_id, mode)? else {
        return Err(Error::refused(format!(
            "root {} is held by another holder, most likely a return in progress; run never waits for a return, so nothing was run",
            first.root_id
        )));
    };
    let lease = leases::read_lease(state, lease_id)?
        .ok_or_else(|| Error::not_found(format!("no lease {lease_id}")))?;
    require_leased(&lease)?;
    let root = area.resolve(&lease.root_id)?;
    if Path::new(&lease.path) != tree_path(&root.path) {
        return Err(Error::unknown_state(format!(
            "lease {lease_id} names {}, which is not the checkout of root {}; nothing was run",
            lease.path, lease.root_id
        )));
    }
    if !Path::new(&lease.path).is_dir() {
        return Err(Error::refused(format!(
            "checkout {} is missing; nothing was run",
            lease.path
        )));
    }
    Ok(WorktreeHold { lease, hold })
}

/// Refuse any lease that is not `leased`. A lease never returns to `leased`, so
/// the first check can refuse early and the second one, under the hold, decides.
fn require_leased(lease: &WorktreeLease) -> Result<()> {
    if lease.state == WorktreeLeaseState::Leased {
        return Ok(());
    }
    Err(Error::refused(format!(
        "lease {} is {}, not leased; nothing was run",
        lease.lease_id,
        lease.state.as_str()
    )))
}
