//! Pool status: every declared pool (or one repository's), its roots, and the
//! lease that reserves each root. Read-only: it declares no area and changes no
//! file, so it works on a read-only account.

use std::path::PathBuf;

use serde::Serialize;

use super::leases::{self, WorktreeLease};
use super::pools::{self, Pool};
use crate::area::Location;
use crate::error::Result;
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
    let filter = repo.map(pools::canonical_repo).transpose()?;
    if let Some(identity) = &filter {
        pools::find_pool(&pools, identity)?;
    }
    let leases = leases::all_leases(state)?;
    let mut report = Vec::new();
    for pool in &pools {
        if filter
            .as_ref()
            .is_none_or(|identity| *identity == pool.repo)
        {
            report.push(pool_status(barge, pool, &leases)?);
        }
    }
    Ok(report)
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
