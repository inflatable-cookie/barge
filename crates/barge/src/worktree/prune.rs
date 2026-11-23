//! Maintain a pool's ready checkouts under the same account lock as leasing
//! and returning. Preview reads the same inventory without writing account
//! state, fetching from an origin, or taking a hold.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::Serialize;

use super::leases;
use super::pools::{self, Pool};
use super::returns;
use super::{area_directory, clone_path, ensure_clone, git, tree_path, ROOT_LABEL};
use crate::area::Area;
use crate::error::{Error, ErrorKind, Result};
use crate::hold::Mode;
use crate::root::RootInfo;
use crate::Barge;

/// The changes to one pool, or what a dry run would do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorktreePrune {
    /// The canonical repository identity.
    pub repo: String,
    /// How many detached checkouts were prepared.
    pub created: usize,
    /// IDs of excess free roots evicted oldest first.
    pub evicted: Vec<String>,
    /// IDs of interrupted returns completed.
    pub finished: Vec<String>,
}

pub(super) fn prune(
    barge: &Barge,
    repo: Option<&str>,
    dry_run: bool,
) -> Result<Vec<WorktreePrune>> {
    if !dry_run {
        barge.core.ensure_writable()?;
    }
    let pools = pools::read_pools(barge.core.state.root())?;
    pools::select_pools(&pools, repo)?
        .into_iter()
        .map(|pool| prune_pool(barge, pool, dry_run))
        .collect()
}

fn prune_pool(barge: &Barge, pool: &Pool, dry_run: bool) -> Result<WorktreePrune> {
    let mut report = WorktreePrune {
        repo: pool.repo.clone(),
        created: 0,
        evicted: Vec::new(),
        finished: Vec::new(),
    };
    let area = match barge.declared_pool_area(pool)? {
        Some(area) => area,
        None if pool.warm == 0 => return Ok(report),
        None if dry_run => barge.area_from_record(barge.pool_area_spec(pool)?.into_record())?,
        None => barge.pool_area(pool)?,
    };
    // Declaring the area takes the account lock itself.
    let _lock = if dry_run {
        None
    } else {
        Some(barge.core.state.lock()?)
    };
    let directory = area_directory(&area)?;
    let leases = leases::all_leases(barge.core.state.root())?;
    let slug = pool.slug()?;
    let reserved: BTreeMap<_, _> = leases
        .iter()
        .filter(|lease| lease.pool == slug && lease.state.reserves_root())
        .map(|lease| (lease.root_id.as_str(), lease))
        .collect();

    let listing = area.list_without_bytes()?;
    let mut finished = BTreeSet::new();
    let mut free = Vec::new();
    for root in &listing.roots {
        if root.held {
            continue;
        }
        if let Some(lease) = reserved.get(root.id.as_str()) {
            if !lease.state.resumes_return()
                || (!dry_run
                    && returns::reset_lease(barge, &area, pool, (*lease).clone(), false)?.is_none())
            {
                continue;
            }
            report.finished.push(lease.lease_id.clone());
            finished.insert(root.id.as_str());
        } else if root.pinned {
            continue;
        }

        // Only checkouts of the active placement's clone can serve new leases.
        if root.path.parent() == Some(&directory) {
            free.push(root);
        }
    }
    let now = barge.core.clock.now_unix_ms();
    free.sort_by_key(|root| {
        let used = if finished.contains(root.id.as_str()) {
            now.max(root.last_used_unix_ms)
        } else {
            root.last_used_unix_ms
        };
        (used, root.id.as_str())
    });
    let warm = usize::try_from(pool.warm).unwrap_or(usize::MAX);
    let max = usize::try_from(pool.max).unwrap_or(usize::MAX);
    let remove = free.len().saturating_sub(warm);
    let add = warm
        .saturating_sub(free.len())
        .min(max.saturating_sub(listing.roots.len()));

    for root in &free[..remove] {
        if dry_run || evict(&area, root)? {
            report.evicted.push(root.id.clone());
        }
    }
    if !dry_run && !report.evicted.is_empty() {
        let clone = clone_path(&directory);
        if clone
            .try_exists()
            .map_err(|source| Error::io("cannot inspect", &clone, &source))?
        {
            git::require_bare(&clone)?;
            git::prune_worktrees(&clone)?;
        }
    }

    // A prior failed checkout may have left an empty but owned root.
    let mut empty = Vec::new();
    for root in &free[remove..] {
        let tree = tree_path(&root.path);
        match std::fs::symlink_metadata(&tree) {
            Ok(_) => {}
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => empty.push(*root),
            Err(source) => return Err(Error::io("cannot inspect", &tree, &source)),
        }
    }
    report.created = if dry_run {
        add + empty.len()
    } else {
        prepare(&area, pool, &directory, &empty, add)?
    };
    Ok(report)
}

fn evict(area: &Area, root: &RootInfo) -> Result<bool> {
    match area.evict_listed(root) {
        Ok(_) => Ok(true),
        Err(error) if matches!(error.kind(), ErrorKind::Refused | ErrorKind::NotFound) => Ok(false),
        Err(error) => Err(error),
    }
}

fn prepare(
    area: &Area,
    pool: &Pool,
    directory: &Path,
    empty: &[&RootInfo],
    add: usize,
) -> Result<usize> {
    if empty.is_empty() && add == 0 {
        return Ok(0);
    }
    let clone = ensure_clone(directory, pool)?;
    git::prune_worktrees(&clone)?;
    let base = git::fetch_default(&clone)?;
    let mut created = 0;
    for root in empty {
        if let Some(_hold) = area.try_hold(&root.id, Mode::Exclusive)? {
            git::add_detached(&clone, &tree_path(&root.path), &base)?;
            created += 1;
        }
    }
    for _ in 0..add {
        let root = area.create_root(ROOT_LABEL)?;
        let Some(hold) = area.try_hold(&root.id, Mode::Exclusive)? else {
            continue;
        };
        let result = git::add_detached(&clone, &tree_path(&root.path), &base);
        drop(hold);
        if let Err(error) = result {
            // Use ordinary owned-root eviction for a partial checkout.
            let _ = area.evict(&root.id);
            return Err(error);
        }
        created += 1;
    }
    Ok(created)
}
