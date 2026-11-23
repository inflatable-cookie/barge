//! Worktree pools: warm, reusable git worktrees per repository, leased to a caller
//! on a new branch at an exact commit (`docs/knowledge/worktrees.md`).
//!
//! A pool is an area in the `worktrees` class, named by the repository's
//! canonical identity. Each checkout is a root in that area, at `<root>/checkout`,
//! and the pool's bare clone sits in the area's `.barge/` folder. A lease is a
//! durable record that reserves its root, with the root pinned against eviction
//! for as long as the lease lives. Nothing here scans or kills processes.

mod cleanup;
pub(crate) mod git;
mod lease;
mod leases;
mod pools;
mod prune;
mod returns;
mod run;
mod status;
mod warm;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub use leases::{LeaseRequest, WorktreeLease, WorktreeLeaseState, LEASE_SCHEMA};
pub use pools::POOLS_SCHEMA;
pub use prune::WorktreePrune;
pub use returns::{ReturnAction, WorktreeReturn};
pub use run::WorktreeHold;
pub(crate) use status::clone_problems;
pub use status::{WorktreeCloneProblem, WorktreePool, WorktreeRoot};

use crate::area::{Area, Location, BARGE_DIR};
use crate::error::{Error, Result};
use crate::hold::Mode;
use crate::layout::{Class, Placement};
use crate::Barge;
use pools::{Pool, NAMESPACE};

/// The bare clone's folder inside an area's `.barge/`.
const CLONE_NAME: &str = "clone.git";
/// The checkout's folder inside a root.
const TREE_NAME: &str = "checkout";
/// The label of roots a pool makes. The folder is `wt-<id>`.
const ROOT_LABEL: &str = "wt";

impl Barge {
    /// Lease a checkout of `request.base` on a new branch. Waits up to `wait` for
    /// a free root when the pool is at its maximum.
    ///
    /// The same request with the same repository, base and branch returns the
    /// existing lease. Any other payload under a used request is refused.
    pub fn worktree_lease(&self, request: LeaseRequest, wait: Duration) -> Result<WorktreeLease> {
        lease::lease(self, request, wait)
    }

    /// The lease a request ID is bound to.
    pub fn worktree_lease_by_request(&self, request: &str) -> Result<WorktreeLease> {
        let leases = leases::all_leases(self.core.state.root())?;
        leases::find_by_request(&leases, request)
            .cloned()
            .ok_or_else(|| Error::not_found(format!("no lease for request {request}")))
    }

    /// The lease with this ID.
    pub fn worktree_lease_by_id(&self, lease_id: &str) -> Result<WorktreeLease> {
        leases::read_lease(self.core.state.root(), lease_id)?
            .ok_or_else(|| Error::not_found(format!("no lease {lease_id}")))
    }

    /// Return a lease: check the tree, then reset it to its base and free the
    /// root. A refused return changes nothing. Returning a returned lease reports
    /// it and touches nothing.
    pub fn worktree_return(
        &self,
        lease_id: &str,
        request: Option<&str>,
        disposable: bool,
    ) -> Result<WorktreeReturn> {
        returns::give_back(self, lease_id, request, disposable)
    }

    /// Hold a lease's checkout for a command, without waiting. Refused unless the
    /// lease is `leased` and no return holds its root. Dropping the hold releases
    /// it. Pass it on with [`WorktreeHold::spawn`] or [`WorktreeHold::exec`].
    pub fn worktree_hold(&self, lease_id: &str, mode: Mode) -> Result<WorktreeHold> {
        run::hold(self, lease_id, mode)
    }

    /// Every declared pool, or the one for `repo`, with its roots and leases.
    pub fn worktree_status(&self, repo: Option<&str>) -> Result<Vec<WorktreePool>> {
        status::status(self, repo)
    }

    /// Bring each selected pool's free roots to its warm target, within its
    /// total capacity. A dry run reports the proposed changes without writing.
    pub fn worktree_prune(&self, repo: Option<&str>, dry_run: bool) -> Result<Vec<WorktreePrune>> {
        prune::prune(self, repo, dry_run)
    }

    /// The `worktrees` class placement, or `None` when the class has no layout.
    fn worktree_placement(&self) -> Result<Option<Placement>> {
        Ok(self.layout()?.classes.get(&Class::Worktrees).cloned())
    }

    /// The area a pool's roots live in, declared on first use.
    fn pool_area(&self, pool: &Pool) -> Result<Area> {
        self.area(self.pool_area_spec(pool)?)
    }

    fn pool_area_spec(&self, pool: &Pool) -> Result<crate::AreaSpec> {
        let placement = self.worktree_placement()?.ok_or_else(|| {
            Error::refused(
                "resource class worktrees has no layout; place it with `barge layout set worktrees`",
            )
        })?;
        let slug = pool.slug()?;
        Ok(
            crate::AreaSpec::new(NAMESPACE, slug.clone(), placement.volume)
                .folder(format!("{}/{slug}", placement.path))
                .absent(placement.absent)
                .counts_toward_reserve(false),
        )
    }

    /// The area of a pool that has already been declared, without declaring it.
    fn declared_pool_area(&self, pool: &Pool) -> Result<Option<Area>> {
        let key = crate::area::AreaKey::new(NAMESPACE, pool.slug()?)?;
        self.core
            .state
            .read_area(&key)?
            .map(|record| self.area_from_record(record))
            .transpose()
    }
}

/// The folder an area's roots and clone are in right now. A pool whose area is
/// unavailable refuses rather than guessing a place.
fn area_directory(area: &Area) -> Result<PathBuf> {
    match area.location()? {
        Location::Volume(path) | Location::Fallback(path) => Ok(path),
        Location::Unavailable(reason) => Err(Error::refused(format!(
            "worktree pool {} is unavailable: {reason}",
            area.key().label()
        ))),
    }
}

/// The pool's bare clone path inside its area directory.
fn clone_path(directory: &Path) -> PathBuf {
    directory.join(BARGE_DIR).join(CLONE_NAME)
}

/// The bare clone, created on first use from the pool's origin.
fn ensure_clone(directory: &Path, pool: &Pool) -> Result<PathBuf> {
    let clone = clone_path(directory);
    if !clone
        .try_exists()
        .map_err(|source| Error::io("cannot inspect", &clone, &source))?
    {
        let barge = directory.join(BARGE_DIR);
        fs::create_dir_all(&barge).map_err(|source| Error::io("cannot create", &barge, &source))?;
        git::init_bare(&clone, &pool.origin)?;
    }
    git::require_bare(&clone)?;
    Ok(clone)
}

/// The checkout folder of a root.
fn tree_path(root_path: &Path) -> PathBuf {
    root_path.join(TREE_NAME)
}
