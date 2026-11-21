//! Barge's storage library: every mechanic, no argv.
//!
//! Volume identity and free space are the only platform-specific facts; they sit
//! behind [`os::Os`], which tests replace with a fake. State lives in small files
//! under the account state directory, written under one lock and published by
//! atomic rename. There is no daemon, no journal and no process scanning:
//! cooperative `flock` holds and pins protect a root, eviction renames a root into
//! the area's `.trash/` before deleting it, and a later root never reuses the
//! evicted path.
//!
//! `docs/knowledge/architecture.md` is the design this crate implements.

#![deny(unsafe_code)]

pub mod area;
pub mod area_spec;
pub mod bindings;
pub mod clock;
pub mod doctor;
pub mod error;
pub mod hold;
pub mod known_links;
pub mod layout;
pub mod move_plan;
pub mod os;
pub mod pin;
pub mod root;
pub mod scratch;
pub mod space;
pub mod state;
mod tool_steps;
mod trash;
pub mod volume;
pub mod workflow;
pub mod worktree;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use state::StateDir;

pub use area::{Absent, Area, AreaRecord, AreaSpec, AreaStatus, Location};
pub use bindings::{
    Applied, ApplyOptions, Binding, BindingKind, BindingRecord, BindingTarget, Bindings,
    EntryIdentity, EntryKind, LinkPlan, Observed, Prior,
};
pub use clock::{Clock, ManualClock, SystemClock};
pub use doctor::{
    BindingReport, BindingState, ClassFolder, DoctorReport, LayoutReport, VolumeReport,
};
pub use error::{Error, ErrorKind, Result};
pub use hold::{Hold, Mode};
pub use known_links::KnownLink;
pub use layout::{Class, Layout, Placement, ResourcePath, SurveyItem};
pub use move_plan::{
    MoveApplyOptions, MoveArea, MoveBinding, MoveLayout, MovePlan, MoveReport, UnrecordedLink,
};
pub use os::{FakeOs, Os};
pub use root::{Listing, Root, RootInfo, Straggler};
pub use scratch::Scratch;
pub use space::{
    EvictedRoot, KeepReason, KeptRoot, PruneReason, PruneReport, SkippedRoot, Space, SweepReport,
    SweptRoot,
};
pub use volume::{
    Availability, Durability, Volume, VolumeRegistration, VolumeSpec, VolumeState, VolumeStatus,
};
pub use workflow::{BlockState, ExcludeState, InitAction, InitCheck, InitReport, WorkflowBlock};

/// Small helpers for tests: a private temporary directory that removes itself.
///
/// The module is a feature and not part of the library Oxide links: a build that
/// does not ask for `testing` carries no test helpers at all. Barge's own tests
/// enable it through its dev-dependency.
#[cfg(any(test, feature = "testing"))]
pub mod testing;

/// The shared interior of [`Barge`], [`area::Area`] and [`scratch::Scratch`].
#[derive(Debug, Clone)]
pub(crate) struct Core {
    pub(crate) state: StateDir,
    pub(crate) os: Arc<dyn Os>,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) read_only: bool,
    /// The home folder known link locations are found under, when a caller named
    /// one instead of `HOME`.
    pub(crate) home: Option<PathBuf>,
}

impl Core {
    /// The home folder to look for known link locations under: the one a caller
    /// named, else `HOME`. Without either, no known-link scan runs.
    pub(crate) fn home(&self) -> Option<PathBuf> {
        self.home.clone().or_else(|| {
            std::env::var_os("HOME")
                .filter(|home| !home.is_empty())
                .map(PathBuf::from)
        })
    }

    pub(crate) fn ensure_writable(&self) -> Result<()> {
        if self.read_only {
            Err(Error::refused("Barge account was opened read-only"))
        } else {
            Ok(())
        }
    }

    /// One registered volume by name, reading the account's volume list.
    pub(crate) fn volume(&self, name: &str) -> Result<Volume> {
        self.state
            .read_volumes()?
            .into_iter()
            .find(|spec| spec.name == name)
            .map(|spec| Volume::new(self.clone(), spec))
            .ok_or_else(|| Error::not_found(format!("volume {name} is not registered")))
    }

    pub(crate) fn declare_area(&self, proposed: AreaRecord) -> Result<Area> {
        proposed.validate()?;
        let volume = self.volume(&proposed.volume)?;
        let _lock = if self.read_only {
            None
        } else {
            Some(self.state.lock()?)
        };
        let record = match self.state.read_area(&proposed.key())? {
            Some(stored) => {
                let differing = stored.differing_fields(&proposed);
                if !differing.is_empty() {
                    return Err(Error::refused(format!(
                        "area {} is already declared with different {}",
                        proposed.key().label(),
                        differing.join(", ")
                    )));
                }
                stored
            }
            None if self.read_only => {
                return Err(Error::refused(format!(
                    "area {} is not declared on a read-only account",
                    proposed.key().label()
                )))
            }
            None => {
                self.state.write_area(&proposed)?;
                proposed
            }
        };
        Ok(Area::new(self.clone(), record, volume))
    }
}

/// Barge's entry point: the account state directory plus the platform seams.
#[derive(Debug, Clone)]
pub struct Barge {
    core: Core,
}

impl Barge {
    /// Open the account state directory named by `BARGE_STATE_DIR`, or the
    /// platform default (`~/Library/Application Support/barge` on macOS,
    /// `$XDG_STATE_HOME/barge` or `~/.local/state/barge` elsewhere).
    pub fn open() -> Result<Self> {
        let root = StateDir::discover()?;
        Self::open_with(root, os::platform(), Arc::new(clock::SystemClock::new()))
    }

    /// Open a named state directory with explicit platform seams. Consumers'
    /// tests use this with [`FakeOs`] and [`ManualClock`].
    pub fn open_with(
        state_dir: impl Into<PathBuf>,
        os: Arc<dyn Os>,
        clock: Arc<dyn Clock>,
    ) -> Result<Self> {
        let state = StateDir::open(state_dir.into())?;
        Ok(Self {
            core: Core {
                state,
                os,
                clock,
                read_only: false,
                home: None,
            },
        })
    }

    /// Open the default account without creating or changing files.
    pub fn open_read_only() -> Result<Self> {
        let root = StateDir::discover()?;
        Self::open_read_only_with(root, os::platform(), Arc::new(clock::SystemClock::new()))
    }

    /// Open a named account without creating or changing files.
    pub fn open_read_only_with(
        state_dir: impl Into<PathBuf>,
        os: Arc<dyn Os>,
        clock: Arc<dyn Clock>,
    ) -> Result<Self> {
        let state = StateDir::open_read_only(state_dir.into())?;
        Ok(Self {
            core: Core {
                state,
                os,
                clock,
                read_only: true,
                home: None,
            },
        })
    }

    /// Look for known link locations under `home` instead of `HOME`. Only a
    /// caller that is not scanning a real home folder needs this, which is what
    /// Barge's own tests do; a real run reads `HOME`.
    pub fn with_home(mut self, home: impl Into<PathBuf>) -> Self {
        self.core.home = Some(home.into());
        self
    }

    /// The account state directory in use.
    pub fn state_dir(&self) -> &Path {
        self.core.state.root()
    }

    /// Every registered volume, in name order.
    pub fn volumes(&self) -> Result<Vec<Volume>> {
        Ok(self
            .core
            .state
            .read_volumes()?
            .into_iter()
            .map(|spec| Volume::new(self.core.clone(), spec))
            .collect())
    }

    /// One registered volume by name.
    pub fn volume(&self, name: &str) -> Result<Volume> {
        self.core.volume(name)
    }

    /// Declare an area. The stored declaration is authoritative: a second
    /// declaration that differs is refused, naming the differing fields.
    pub fn area(&self, spec: AreaSpec) -> Result<Area> {
        self.core.declare_area(spec.into_record())
    }

    /// Every stored area declaration, ordered by namespace then name.
    pub fn areas(&self) -> Result<Vec<Area>> {
        let mut records = self.core.state.read_areas()?;
        records.sort_by(|left, right| {
            (left.namespace.as_str(), left.name.as_str())
                .cmp(&(right.namespace.as_str(), right.name.as_str()))
        });
        let mut areas = Vec::with_capacity(records.len());
        for record in records {
            areas.push(self.area_from_record(record)?);
        }
        Ok(areas)
    }

    /// Find a declared area by its namespace/name key.
    pub fn find_area(&self, key: &str) -> Result<Area> {
        let key = area::AreaKey::parse(key)?;
        self.areas()?
            .into_iter()
            .find(|area| area.key() == key)
            .ok_or_else(|| {
                Error::not_found(format!(
                    "area {} is not declared; declare it with `barge area declare`",
                    key.label()
                ))
            })
    }

    /// Status of declared areas, optionally limited to a volume.
    pub fn area_statuses(&self, volume: Option<&str>) -> Result<Vec<AreaStatus>> {
        self.areas()?
            .into_iter()
            .filter(|area| volume.is_none_or(|name| area.record().volume == name))
            .map(|area| area.status())
            .collect()
    }

    /// Free space, reserve and floor for a volume.
    pub fn space(&self, volume_name: &str) -> Result<Space> {
        let volume = self.volume(volume_name)?;
        let mount = volume.require_present()?;
        let free = self.core.os.free_bytes(&mount)?;
        let spec = volume.spec();
        Ok(Space {
            free_bytes: free,
            reserve_bytes: spec.reserve_bytes,
            floor_bytes: spec.floor_bytes,
        })
    }

    /// Prune a volume: each area's soft cap first, then least-recently-used idle
    /// unpinned roots until free space is back above the volume's reserve.
    pub fn prune(&self, volume_name: &str, dry_run: bool) -> Result<PruneReport> {
        if !dry_run {
            self.core.ensure_writable()?;
        }
        space::prune(&self.core, volume_name, dry_run, None)
    }

    /// Admit work on a volume: prune, then refuse only below the floor once
    /// nothing idle is left.
    pub fn admit(&self, volume_name: &str) -> Result<PruneReport> {
        self.core.ensure_writable()?;
        let floor = self.volume(volume_name)?.spec().floor_bytes;
        let report = space::prune(&self.core, volume_name, false, Some(floor))?;
        let space = self.space(volume_name)?;
        if space.free_bytes < space.floor_bytes {
            return Err(Error::refused(format!(
                "volume {volume_name} has {} free, below its {} floor",
                space.free_bytes, space.floor_bytes
            )));
        }
        Ok(report)
    }

    /// Delete every owned trash entry of every area on a volume.
    pub fn sweep_trash(&self, volume_name: &str) -> Result<SweepReport> {
        self.core.ensure_writable()?;
        let mut report = SweepReport::default();
        for area in self.areas()? {
            if area.record().volume != volume_name {
                continue;
            }
            let swept = area.sweep_trash()?;
            report.deleted.extend(swept.deleted);
            report.kept.extend(swept.kept);
        }
        Ok(report)
    }

    /// Barge's own scratch area for one project on one volume.
    pub fn scratch(&self, volume_name: &str, project: &str) -> Result<Scratch> {
        Scratch::open(self.core.clone(), volume_name, project)
    }

    /// Sweep expired idle scratch roots for one project, or every project on the
    /// volume when `project` is `None`.
    pub fn sweep_scratch(&self, volume_name: &str, project: Option<&str>) -> Result<SweepReport> {
        self.core.ensure_writable()?;
        let mut report = SweepReport::default();
        for area in self.areas()? {
            let record = area.record();
            if record.namespace != scratch::NAMESPACE || record.volume != volume_name {
                continue;
            }
            if project.is_some_and(|name| name != record.name) {
                continue;
            }
            let swept = area.sweep_expired()?;
            report.deleted.extend(swept.deleted);
            report.kept.extend(swept.kept);
        }
        Ok(report)
    }

    /// One document describing every volume and area, for the CLI's `status` and
    /// for a consumer's diagnostics.
    pub fn status(&self) -> Result<Vec<VolumeStatus>> {
        let mut volumes = Vec::new();
        let all_areas = self.areas()?;
        for volume in self.volumes()? {
            let state = volume.state()?;
            let free_bytes = match &state {
                VolumeState::Present => Some(self.core.os.free_bytes(&volume.spec().mount)?),
                VolumeState::Mismatch { .. } => None,
                VolumeState::Absent => None,
            };
            let mut areas = Vec::new();
            for area in &all_areas {
                if area.record().volume != volume.spec().name {
                    continue;
                }
                areas.push(area.status()?);
            }
            volumes.push(VolumeStatus {
                spec: volume.spec().clone(),
                state,
                free_bytes,
                areas,
            });
        }
        Ok(volumes)
    }

    pub(crate) fn area_from_record(&self, record: AreaRecord) -> Result<Area> {
        let volume = self.volume(&record.volume)?;
        Ok(Area::new(self.core.clone(), record, volume))
    }
}
