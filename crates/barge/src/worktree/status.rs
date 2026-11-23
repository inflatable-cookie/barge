//! Pool status: every declared pool (or one repository's), its roots, and the
//! lease that reserves each root. Read-only: it declares no area and changes no
//! file, so it works on a read-only account.

use std::path::PathBuf;

use serde::Serialize;

use super::leases::{self, WorktreeLease};
use super::pools::{self, Pool};
use super::{area_directory, clone_path, git};
use crate::area::Location;
use crate::error::{Error, Result};
use crate::Barge;

/// One pool's line in the status report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorktreePool {
    /// The canonical repository identity.
    pub repo: String,
    /// The pool's area name.
    pub pool: String,
    pub warm: u32,
    pub max: u32,
    /// Where the pool's roots live now, when the area is available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<PathBuf>,
    /// Why the pool cannot be used now, when it cannot.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unavailable: Option<String>,
    pub roots: Vec<WorktreeRoot>,
}

/// One root of a pool: free, or reserved by the lease it carries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorktreeRoot {
    pub id: String,
    pub path: PathBuf,
    /// Whether a process holds the root right now.
    pub held: bool,
    /// `free`, or the state of the lease reserving the root.
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lease_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

pub(crate) fn status(barge: &Barge, repo: Option<&str>) -> Result<Vec<WorktreePool>> {
    let state = barge.core.state.root();
    let pools = pools::read_pools(state)?;
    let selected = pools::select_pools(&pools, repo)?;
    let leases = leases::all_leases(state)?;
    selected
        .into_iter()
        .map(|pool| pool_status(barge, pool, &leases))
        .collect()
}

fn pool_status(barge: &Barge, pool: &Pool, leases: &[WorktreeLease]) -> Result<WorktreePool> {
    let slug = pool.slug()?;
    let mut report = WorktreePool {
        repo: pool.repo.clone(),
        pool: slug.clone(),
        warm: pool.warm,
        max: pool.max,
        location: None,
        unavailable: None,
        roots: Vec::new(),
    };
    let Some(area) = barge.declared_pool_area(pool)? else {
        report.unavailable = Some("nothing has been leased from this pool yet".to_string());
        return Ok(report);
    };
    match area.location()? {
        Location::Volume(path) | Location::Fallback(path) => report.location = Some(path),
        Location::Unavailable(reason) => {
            report.unavailable = Some(reason);
            return Ok(report);
        }
    }
    for root in area.list_without_bytes()?.roots {
        let lease = leases.iter().find(|lease| {
            lease.pool == slug && lease.root_id == root.id && lease.state.reserves_root()
        });
        report.roots.push(WorktreeRoot {
            state: lease
                .map_or("free", |lease| lease.state.as_str())
                .to_string(),
            lease_id: lease.map(|lease| lease.lease_id.clone()),
            request: lease.map(|lease| lease.request.clone()),
            branch: lease.map(|lease| lease.branch.clone()),
            id: root.id,
            path: root.path,
            held: root.held,
        });
    }
    Ok(report)
}

/// A bare clone that doctor could not find or verify.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorktreeCloneProblem {
    pub repo: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    pub reason: String,
}

pub(crate) fn clone_problems(barge: &Barge) -> Result<Vec<WorktreeCloneProblem>> {
    let mut problems = Vec::new();
    for pool in pools::read_pools(barge.core.state.root())? {
        let mut path = None;
        let check = (|| {
            let area = barge.declared_pool_area(&pool)?.ok_or_else(|| {
                Error::not_found("bare clone is missing; the pool area has not been declared")
            })?;
            let clone = clone_path(&area_directory(&area)?);
            let checked = git::check_bare(&clone);
            path = Some(clone);
            checked
        })();
        if let Err(error) = check {
            problems.push(WorktreeCloneProblem {
                repo: pool.repo,
                path,
                reason: error.to_string(),
            });
        }
    }
    Ok(problems)
}
