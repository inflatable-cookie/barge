//! Barge's own scratch: one area per project with an expiry per root, so test
//! runs never scatter folders across a drive root or a home folder.
//!
//! Layout: `<volume base>/scratch/<project>/<label>-<id>/`.

use std::time::Duration;

use crate::area::{Absent, Area, AreaSpec, Location};
use crate::error::{Error, Result};
use crate::root::{Listing, Root};
use crate::space::SweepReport;
use crate::state::validate_name;
use crate::Core;

/// The namespace every scratch area uses.
pub const NAMESPACE: &str = "scratch";

/// One project's scratch area on one volume.
#[derive(Debug, Clone)]
pub struct Scratch {
    area: Area,
}

impl Scratch {
    pub(crate) fn open(core: Core, volume_name: &str, project: &str) -> Result<Self> {
        validate_name("scratch project", project)?;
        let proposed = AreaSpec::new(NAMESPACE, project, volume_name)
            .absent(Absent::Fail)
            .counts_toward_reserve(true)
            .into_record();
        Ok(Self {
            area: core.declare_area(proposed)?,
        })
    }

    /// The scratch area behind this project.
    pub fn area(&self) -> &Area {
        &self.area
    }

    /// Where new scratch roots go, or why there is nowhere.
    pub fn location(&self) -> Result<Location> {
        self.area.location()
    }

    /// Create a scratch root that expires after `ttl`.
    pub fn create(&self, label: &str, ttl: Duration) -> Result<Root> {
        let now = self.area.now_unix_ms();
        let millis = u64::try_from(ttl.as_millis())
            .map_err(|_| Error::usage(format!("scratch lifetime {ttl:?} is too long")))?;
        let expires = now
            .checked_add(millis)
            .ok_or_else(|| Error::usage(format!("scratch lifetime {ttl:?} overflows the clock")))?;
        self.area.create_root_with_expiry(label, Some(expires))
    }

    /// Every root in this project's scratch area.
    pub fn roots(&self) -> Result<Listing> {
        self.area.list_without_bytes()
    }

    /// Evict expired, idle, unpinned scratch roots.
    pub fn sweep(&self) -> Result<SweepReport> {
        self.area.sweep_expired()
    }
}
