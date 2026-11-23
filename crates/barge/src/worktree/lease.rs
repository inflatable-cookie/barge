//! Leasing a root: take a free root (or make one below the pool's maximum), check
//! out the request's branch at its exact base, pin the root and record the lease
//! before the call replies.
//!
//! Every decision that reads or writes the pool happens under the account state
//! lock, so two leases never take one root. A wait for a root releases that lock
//! between attempts, so a return can free a root while a lease waits.

use std::collections::BTreeSet;
use std::path::Path;
use std::time::{Duration, Instant};

use super::git;
use super::leases::{self, LeaseRequest, WorktreeLease, WorktreeLeaseState, LEASE_SCHEMA};
use super::pools::{self, Pool};
use super::warm::{self, WarmReport};
use super::{area_directory, ensure_clone, tree_path, ROOT_LABEL};
use crate::area::Area;
use crate::error::{Error, Result};
use crate::hold::Mode;
use crate::root::{mint_id, RootInfo};
use crate::Barge;

/// How often a waiting lease tries again.
const POLL: Duration = Duration::from_millis(20);

pub(crate) fn lease(barge: &Barge, request: LeaseRequest, wait: Duration) -> Result<WorktreeLease> {
    barge.core.ensure_writable()?;
    request.validate()?;
    let repo = pools::canonical_repo(&request.repo)?;
    let pool = pools::find_pool(&pools::read_pools(barge.core.state.root())?, &repo)?.clone();
    // Declaring the area takes the state lock itself, so it happens here, before
    // any attempt holds that lock.
    let area = barge.pool_area(&pool)?;
    let started = Instant::now();
    let mut fetched = false;
    loop {
        match attempt(barge, &pool, &area, &request, &repo, &mut fetched)? {
            Some(lease) => return Ok(lease),
            None if started.elapsed() >= wait => {
                return Err(Error::refused(format!(
                    "worktree pool {repo} is at its maximum of {} and no root is free; nothing was leased",
                    pool.max
                )))
            }
            None => std::thread::sleep(POLL),
        }
    }
}

/// One pass under the state lock: the request's own lease if it has one, else a
/// new lease on a free or new root.
fn attempt(
    barge: &Barge,
    pool: &Pool,
    area: &Area,
    request: &LeaseRequest,
    repo: &str,
    fetched: &mut bool,
) -> Result<Option<WorktreeLease>> {
    let state = barge.core.state.root();
    let _lock = barge.core.state.lock()?;
    let leases = leases::all_leases(state)?;
    if let Some(existing) = leases::find_by_request(&leases, &request.request) {
        if request.same_payload(existing, repo) {
            return Ok(Some(existing.clone()));
        }
        return Err(Error::refused(format!(
            "request {} is already bound to {} at {} on {}; a changed payload is refused and nothing was leased",
            request.request, existing.repo, existing.base, existing.branch
        )));
    }
    let directory = area_directory(area)?;
    let clone = ensure_clone(&directory, pool)?;
    if git::branch_exists(&clone, &request.branch)? {
        return Err(Error::refused(format!(
            "branch {} already exists in pool {repo}; nothing was leased",
            request.branch
        )));
    }
    if !*fetched {
        git::prune_worktrees(&clone)?;
        git::fetch(&clone)?;
        *fetched = true;
    }
    if git::commit_of(&clone, &request.base)?.is_none() {
        return Err(Error::refused(format!(
            "base {} is not a commit of {repo}; nothing was leased",
            request.base
        )));
    }
    take_root(barge, pool, area, &clone, &leases, request, repo)
}

/// Choose a root and make the lease on it. `None` means the pool is full and
/// every root is reserved or held.
fn take_root(
    barge: &Barge,
    pool: &Pool,
    area: &Area,
    clone: &Path,
    leases: &[WorktreeLease],
    request: &LeaseRequest,
    repo: &str,
) -> Result<Option<WorktreeLease>> {
    let slug = pool.slug()?;
    let reserved: BTreeSet<&str> = leases
        .iter()
        .filter(|lease| lease.pool == slug && lease.state.reserves_root())
        .map(|lease| lease.root_id.as_str())
        .collect();
    let listing = area.list_without_bytes()?;
    let free: Vec<&RootInfo> = listing
        .roots
        .iter()
        .filter(|root| !root.held && !root.pinned && !reserved.contains(root.id.as_str()))
        .collect();
    // A recorded root is likeliest to still have valid dependency folders,
    // so it wins over a cold one even when the cold one has sat idle longer.
    // Among recorded roots the most recently used wins, since its folders
    // are likeliest to match the current lockfiles. With no record anywhere
    // — including a pool that declares no warm paths — the choice keeps the
    // LRU order below, and eviction and prune keep theirs regardless.
    let ordered: Vec<&RootInfo> = if pool.warm_paths.is_empty() {
        let mut lru = free;
        lru.sort_by(|left, right| {
            (left.last_used_unix_ms, left.id.as_str())
                .cmp(&(right.last_used_unix_ms, right.id.as_str()))
        });
        lru
    } else {
        let (mut warm, mut cold): (Vec<&RootInfo>, Vec<&RootInfo>) = free
            .into_iter()
            .partition(|root| warm::has_record(&root.path));
        warm.sort_by(|left, right| {
            (right.last_used_unix_ms, left.id.as_str())
                .cmp(&(left.last_used_unix_ms, right.id.as_str()))
        });
        cold.sort_by(|left, right| {
            (left.last_used_unix_ms, left.id.as_str())
                .cmp(&(right.last_used_unix_ms, right.id.as_str()))
        });
        warm.extend(cold);
        warm
    };
    let (root_id, root_path) = match ordered.first() {
        Some(root) => (root.id.clone(), root.path.clone()),
        None if listing.roots.len() < usize::try_from(pool.max).unwrap_or(usize::MAX) => {
            let root = area.create_root(ROOT_LABEL)?;
            (root.id, root.path)
        }
        None => return Ok(None),
    };
    let Some(_fence) = area.try_hold(&root_id, Mode::Exclusive)? else {
        return Ok(None);
    };
    let tree = tree_path(&root_path);
    let warm = check_out(pool, clone, &root_path, &request.branch, &request.base)?;

    let lease_id = mint_id()?;
    let now = barge.core.clock.now_unix_ms();
    let lease = WorktreeLease {
        schema: LEASE_SCHEMA.to_string(),
        lease_id: lease_id.clone(),
        request: request.request.clone(),
        repo: repo.to_string(),
        base: request.base.clone(),
        base_name: request.base_name.clone(),
        branch: request.branch.clone(),
        label: request.label.clone(),
        pool: slug,
        root_id: root_id.clone(),
        path: tree.to_string_lossy().into_owned(),
        head: request.base.clone(),
        warm_kept: warm.kept,
        warm_removed: warm.removed,
        state: WorktreeLeaseState::Leased,
        disposable: false,
        created_unix_ms: now,
        updated_unix_ms: now,
    };
    let state = barge.core.state.root();
    let recorded = area
        .pin(&root_id, &format!("worktree lease {lease_id}"))
        .and_then(|()| leases::write_lease(state, &lease));
    if let Err(error) = recorded {
        let _ = area.unpin(&root_id);
        abandon(clone, &tree, &request.branch, &request.base);
        return Err(error);
    }
    Ok(Some(lease))
}

/// Check out `branch` at `base` in the root's tree, creating the tree when the
/// root has none. A tree that exists must be clean: it is refused untouched
/// otherwise. A checkout that does not verify is undone.
fn check_out(
    pool: &Pool,
    clone: &Path,
    root: &Path,
    branch: &str,
    base: &str,
) -> Result<WarmReport> {
    let tree = tree_path(root);
    let existing = tree
        .try_exists()
        .map_err(|source| Error::io("cannot inspect", &tree, &source))?;
    let (made, previous) = if existing {
        require_clean(&tree)?;
        let previous = warm::candidates(&tree, pool)?;
        (git::checkout_branch(&tree, branch, base), previous)
    } else {
        (
            git::add_worktree(clone, &tree, branch, base),
            BTreeSet::new(),
        )
    };
    let checked = made.and_then(|()| {
        let report = warm::validate(root, &tree, pool, previous)?;
        verify(&tree, base)?;
        Ok(report)
    });
    if checked.is_err() {
        abandon(clone, &tree, branch, base);
    }
    checked
}

fn require_clean(tree: &Path) -> Result<()> {
    if git::dirty_entries(tree)?.is_empty() {
        return Ok(());
    }
    Err(Error::unknown_state(format!(
        "{} has uncommitted or untracked entries; nothing was leased from it",
        tree.display()
    )))
}

fn verify(tree: &Path, base: &str) -> Result<()> {
    let head = git::head(tree)?;
    if head != base || !git::dirty_entries(tree)?.is_empty() {
        return Err(Error::unknown_state(format!(
            "{} is not clean at {base}; nothing was leased from it",
            tree.display()
        )));
    }
    Ok(())
}

/// Undo a checkout that did not finish: detach the tree at base and delete the
/// branch this call created. Every step is best effort, because the caller is
/// already reporting the failure that caused it.
fn abandon(clone: &Path, tree: &Path, branch: &str, base: &str) {
    if tree.exists() {
        let _ = git::detach(tree, base);
    }
    if git::branch_exists(clone, branch).unwrap_or(false) {
        let _ = git::delete_branch(clone, branch);
    }
}
