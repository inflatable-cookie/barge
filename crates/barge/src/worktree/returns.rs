//! Returning a lease: fence the root against holds, preflight the tree, then reset
//! it to its base and free the root.
//!
//! Only the preflight promises the tree is untouched: a refusal there changes
//! nothing. Once the lease is `returning`, the reset runs; a reset cut off part-way
//! leaves the lease `interrupted`, and the next return finishes it without a second
//! preflight, because the first one already accepted the work. A returned lease
//! reports its own result and touches nothing, even after its root is leased again.

use std::path::{Path, PathBuf};

use super::cleanup::{self, classify};
use super::git;
use super::leases::{self, WorktreeLease, WorktreeLeaseState};
use super::pools::{self, Pool};
use super::{area_directory, clone_path, tree_path};
use crate::area::Area;
use crate::error::{Error, ErrorKind, Result};
use crate::hold::Mode;
use crate::Barge;

/// What a return call did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReturnAction {
    /// The root was reset and freed by this call.
    Returned,
    /// The lease was already returned; nothing was touched.
    AlreadyReturned,
}

impl ReturnAction {
    /// The stable name used in documents and by people.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Returned => "returned",
            Self::AlreadyReturned => "already_returned",
        }
    }
}

/// A return's result: the lease as it now stands, and what the call did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeReturn {
    pub lease: WorktreeLease,
    pub action: ReturnAction,
}

pub(crate) fn give_back(
    barge: &Barge,
    lease_id: &str,
    request: Option<&str>,
    disposable: bool,
) -> Result<WorktreeReturn> {
    barge.core.ensure_writable()?;
    let state = barge.core.state.root();
    let _lock = barge.core.state.lock()?;
    let mut lease = leases::read_lease(state, lease_id)?
        .ok_or_else(|| Error::not_found(format!("no lease {lease_id}")))?;
    if let Some(request) = request {
        if request != lease.request {
            return Err(Error::refused(format!(
                "lease {lease_id} belongs to request {}, not {request}; nothing was returned",
                lease.request
            )));
        }
    }
    if lease.state == WorktreeLeaseState::Returned {
        return Ok(WorktreeReturn {
            lease,
            action: ReturnAction::AlreadyReturned,
        });
    }

    let pools = pools::read_pools(state)?;
    let pool = pool_for_slug(&pools, &lease.pool)?.ok_or_else(|| {
        Error::unknown_state(format!(
            "lease {lease_id} is in pool {}, which is no longer declared; nothing was returned",
            lease.pool
        ))
    })?;
    let area = barge
        .declared_pool_area(pool)?
        .ok_or_else(|| Error::unknown_state(format!("pool {} has no area", lease.pool)))?;
    let directory = area_directory(&area)?;
    // The record names the checkout the git calls will run in and clean, so it
    // must be the checkout of the root it reserves, and nothing else.
    let root = area.resolve(&lease.root_id)?;
    let tree = PathBuf::from(&lease.path);
    if tree != tree_path(&root.path) {
        return Err(Error::unknown_state(format!(
            "lease {lease_id} names {}, which is not the checkout of root {}; nothing was returned",
            lease.path, lease.root_id
        )));
    }
    // Holding the root exclusively fences it: a hold that starts during the
    // return waits for it, and a non-waiting one is refused.
    let Some(_fence) = area.try_hold(&lease.root_id, Mode::Exclusive)? else {
        return Err(Error::refused(format!(
            "root {} is held by a process; nothing was returned",
            lease.root_id
        )));
    };
    let clone = clone_path(&directory);

    if lease.state == WorktreeLeaseState::Leased {
        preflight(&tree, &lease.base, pool, disposable)?;
        lease.disposable = disposable;
        lease.state = WorktreeLeaseState::Returning;
        lease.updated_unix_ms = barge.core.clock.now_unix_ms();
        leases::write_lease(state, &lease)?;
    }

    match reset_and_free(&area, pool, &clone, &tree, &lease) {
        Ok(head) => {
            lease.state = WorktreeLeaseState::Returned;
            lease.head = head;
            lease.updated_unix_ms = barge.core.clock.now_unix_ms();
            leases::write_lease(state, &lease)?;
            Ok(WorktreeReturn {
                lease,
                action: ReturnAction::Returned,
            })
        }
        Err(error) => {
            // A failed write leaves `returning`, which the next return resumes
            // just as it does `interrupted`, so the failure is not hidden.
            lease.state = WorktreeLeaseState::Interrupted;
            lease.updated_unix_ms = barge.core.clock.now_unix_ms();
            let _ = leases::write_lease(state, &lease);
            Err(error)
        }
    }
}

/// The pool whose area name is `slug`.
fn pool_for_slug<'a>(pools: &'a [Pool], slug: &str) -> Result<Option<&'a Pool>> {
    for pool in pools {
        if pool.slug()? == slug {
            return Ok(Some(pool));
        }
    }
    Ok(None)
}

/// Refuse a return whose tree holds work the pool cannot account for. Nothing is
/// changed here: every check reads the tree.
fn preflight(tree: &Path, base: &str, pool: &Pool, disposable: bool) -> Result<()> {
    let dirty = git::dirty_entries(tree)?;
    if !dirty.is_empty() {
        return Err(Error::refused(format!(
            "{} has {} uncommitted or untracked entries; nothing was returned",
            tree.display(),
            dirty.len()
        )));
    }
    let (warm, disposable_paths) = declared_patterns(pool);
    let sweep = classify(&git::ignored_entries(tree)?, &warm, &disposable_paths);
    if !sweep.undeclared.is_empty() {
        return Err(Error::refused(format!(
            "ignored files the pool does not declare disposable: {}; nothing was returned",
            sweep.undeclared.join(", ")
        )));
    }
    let beyond = git::commits_beyond(tree, base)?;
    if beyond > 0 && !disposable {
        return Err(Error::refused(format!(
            "{beyond} commits lie beyond base {base}; pass --disposable if they are published, merged or abandoned; nothing was returned"
        )));
    }
    Ok(())
}

fn declared_patterns(pool: &Pool) -> (Vec<&str>, Vec<&str>) {
    let warm = pool
        .warm_paths
        .iter()
        .map(|warm| warm.path.as_str())
        .collect();
    let disposable = pool.disposable.iter().map(String::as_str).collect();
    (warm, disposable)
}

/// Reset the tree to its target and free the root. Returns the commit the tree
/// now sits at. The pin goes only after the tree verifies.
fn reset_and_free(
    area: &Area,
    pool: &Pool,
    clone: &Path,
    tree: &Path,
    lease: &WorktreeLease,
) -> Result<String> {
    git::fetch(clone)?;
    let target = reset_target(clone, lease)?;
    let (warm, disposable_paths) = declared_patterns(pool);
    let sweep = classify(&git::ignored_entries(tree)?, &warm, &disposable_paths);
    for entry in &sweep.removed {
        cleanup::remove_entry(tree, entry)?;
    }
    git::detach(tree, &target)?;
    if git::branch_exists(clone, &lease.branch)? {
        git::delete_branch(clone, &lease.branch)?;
    }
    let head = git::head(tree)?;
    if head != target || !git::dirty_entries(tree)?.is_empty() {
        return Err(Error::unknown_state(format!(
            "{} did not reset to {target} cleanly; the lease stays out of the pool",
            tree.display()
        )));
    }
    match area.unpin(&lease.root_id) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    Ok(head)
}

/// The commit a return resets to: the base's branch in the clone when it is known,
/// else the lease's own base.
fn reset_target(clone: &Path, lease: &WorktreeLease) -> Result<String> {
    if let Some(name) = &lease.base_name {
        if let Some(sha) = git::commit_of(clone, &format!("refs/remotes/origin/{name}"))? {
            return Ok(sha);
        }
    }
    Ok(lease.base.clone())
}
