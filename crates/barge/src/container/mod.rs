//! Per-profile reservations for container stacks. Consumers start and stop
//! stacks, then return their reservations.

mod profiles;
mod records;

use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use self::records::LeaseRecord;
use crate::state::validate_name;
use crate::{os, root, Barge, Error, ErrorKind, Result, VolumeState};

pub(crate) use profiles::read as read_profiles;

const POLL: Duration = Duration::from_millis(20);

/// The worktree reservation or checkout directory that owns a stack.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContainerOwner {
    Worktree(String),
    Checkout(PathBuf),
}

impl ContainerOwner {
    fn validate(&self) -> Result<()> {
        match self {
            Self::Worktree(id) if !root::valid_id(id) => {
                Err(Error::usage(format!("invalid worktree lease ID {id:?}")))
            }
            Self::Checkout(path)
                if !path.is_absolute() || path.to_str().is_none_or(|path| path.contains('\0')) =>
            {
                Err(Error::usage(
                    "container checkout must be an absolute UTF-8 path",
                ))
            }
            _ => Ok(()),
        }
    }

    fn state(&self, barge: &Barge, location: Option<&CheckoutLocation>) -> Result<OwnerState> {
        match self {
            Self::Worktree(id) => match barge.worktree_lease_by_id(id) {
                Ok(lease) if lease.state.reserves_root() => Ok(OwnerState::Live),
                Ok(_) => Ok(OwnerState::Ended),
                Err(error) if error.kind() == ErrorKind::NotFound => Ok(OwnerState::Ended),
                Err(error) => Err(error),
            },
            Self::Checkout(path) => checkout_state(barge, path, location),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OwnerState {
    Live,
    Ended,
    Unknown,
}

// Resolve symlinks while the checkout exists, so a later dangling link cannot
// hide the filesystem where the checkout lived.
#[derive(Debug, Serialize, Deserialize)]
struct CheckoutLocation {
    path: PathBuf,
    device: u64,
}

impl CheckoutLocation {
    fn capture(owner: &ContainerOwner) -> Result<Option<Self>> {
        let ContainerOwner::Checkout(path) = owner else {
            return Ok(None);
        };
        let resolved = path
            .canonicalize()
            .map_err(|error| Error::io("cannot resolve container owner", path, &error))?;
        let device = os::path_device(&resolved)
            .map_err(|error| Error::io("cannot inspect container owner", &resolved, &error))?;
        Ok(Some(Self {
            path: resolved,
            device,
        }))
    }
}

fn checkout_state(
    barge: &Barge,
    path: &Path,
    location: Option<&CheckoutLocation>,
) -> Result<OwnerState> {
    let exists = |path: &Path| {
        path.try_exists()
            .map_err(|error| Error::io("cannot inspect container owner", path, &error))
    };
    if exists(path)? {
        return Ok(OwnerState::Live);
    }
    let resolved = location.map_or(path, |location| location.path.as_path());
    for volume in barge.volumes()? {
        if (path.starts_with(&volume.spec().mount) || resolved.starts_with(&volume.spec().mount))
            && volume.state()? != VolumeState::Present
        {
            return Ok(OwnerState::Unknown);
        }
    }
    if let Some(location) = location {
        // A mount point can remain after unmounting. Compare the nearest
        // surviving ancestor with the device recorded while the owner lived.
        // Start at the resolved checkout: its symlink may be gone while the
        // checkout itself still exists on that filesystem.
        for ancestor in resolved.ancestors() {
            match os::path_device(ancestor) {
                Ok(device) if device != location.device => return Ok(OwnerState::Unknown),
                Ok(_) => break,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(Error::io(
                        "cannot inspect container owner",
                        ancestor,
                        &error,
                    ))
                }
            }
        }
    }
    match resolved.parent() {
        Some(parent) if exists(parent)? => Ok(OwnerState::Ended),
        _ => Ok(OwnerState::Unknown),
    }
}

/// Only worker stacks consume capacity. Interactive stacks last until owner end.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContainerClass {
    Worker,
    Interactive,
}

/// A reservation request for one compose project in one runtime profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerLeaseRequest {
    pub profile: String,
    pub project: String,
    pub owner: ContainerOwner,
    pub class: ContainerClass,
    /// Hold the lease until the returned value is dropped.
    pub hold: bool,
    /// Capacity wait for a worker lease. `None` returns immediately when full.
    pub wait: Option<Duration>,
}

/// Capacity and management outcomes the consumer can handle directly.
#[derive(Debug)]
#[must_use]
pub enum ContainerLeaseOutcome {
    Leased(ContainerLease),
    NotManaged,
    Full {
        profile: String,
        max: u32,
        active: u32,
    },
}

/// The reserved stack and, when requested, its shared advisory lock. Dropping
/// this value releases the hold. The reservation persists until returned.
#[derive(Debug)]
pub struct ContainerLease {
    record: LeaseRecord,
    hold: Option<File>,
}

impl ContainerLease {
    pub fn id(&self) -> &str {
        &self.record.id
    }

    pub fn profile(&self) -> &str {
        &self.record.profile
    }

    pub fn project(&self) -> &str {
        &self.record.project
    }

    pub fn is_held(&self) -> bool {
        self.hold.is_some()
    }
}

/// One recorded lease and the evidence that it has ended, if available.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ContainerLeaseStatus {
    pub id: String,
    pub profile: String,
    pub project: String,
    pub owner: ContainerOwner,
    pub class: ContainerClass,
    pub ended: Option<ContainerEnding>,
}

/// A stack the consumer can tear down, with the volume policy for that teardown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ContainerEnding {
    pub reason: ContainerEndReason,
    /// Whether the consumer should remove the project's volumes on teardown.
    pub remove_volumes: bool,
}

/// The condition that ended a lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContainerEndReason {
    OwnerEnded,
    HoldReleased,
}

impl Barge {
    /// Reserve a stack, optionally with a hold. A repeat by the same live
    /// owner keeps the original ID and class.
    pub fn container_lease(
        &self,
        request: &ContainerLeaseRequest,
    ) -> Result<ContainerLeaseOutcome> {
        self.container_lease_with_progress(request, &mut || {})
    }

    /// Notify a waiting caller outside the account lock when a profile is full.
    pub(crate) fn container_lease_with_progress(
        &self,
        request: &ContainerLeaseRequest,
        waiting: &mut dyn FnMut(),
    ) -> Result<ContainerLeaseOutcome> {
        if !profiles::read(self.state_dir())?.contains_key(&request.profile) {
            return Ok(ContainerLeaseOutcome::NotManaged);
        }
        self.core.ensure_writable()?;
        validate_name("container project", &request.project)?;
        request.owner.validate()?;

        let mut outcome = {
            let _lock = self.core.state.lock()?;
            self.try_container_lease(request)?
        };
        let started = Instant::now();
        let wait = request.wait.unwrap_or_default();
        while matches!(outcome, ContainerLeaseOutcome::Full { .. }) {
            if started.elapsed() >= wait {
                break;
            }
            waiting();
            let remaining = wait.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                break;
            }
            std::thread::sleep(POLL.min(remaining));
            if let Some(_lock) = self.core.state.try_lock()? {
                outcome = self.try_container_lease(request)?;
            }
        }
        Ok(outcome)
    }

    fn try_container_lease(
        &self,
        request: &ContainerLeaseRequest,
    ) -> Result<ContainerLeaseOutcome> {
        let state = self.state_dir();
        let profiles = profiles::read(state)?;
        let Some(&max) = profiles.get(&request.profile) else {
            return Ok(ContainerLeaseOutcome::NotManaged);
        };
        let mut leases = records::all(state)?;
        let key = (request.profile.as_str(), request.project.as_str());
        if let Some(index) = leases.iter().position(|lease| lease.key() == key) {
            return self.repeat_container_lease(request, leases.swap_remove(index));
        }
        match request.owner.state(self, None)? {
            OwnerState::Live => {}
            OwnerState::Ended => return Err(Error::refused("container lease owner has ended")),
            OwnerState::Unknown => return Err(Error::refused("container lease owner is unknown")),
        }
        let checkout_location = CheckoutLocation::capture(&request.owner)?;
        if request.class == ContainerClass::Worker {
            let workers = leases.iter().filter(|lease| {
                lease.profile == request.profile && lease.class == ContainerClass::Worker
            });
            let active = u32::try_from(workers.count()).unwrap_or(u32::MAX);
            if active >= max {
                return Ok(ContainerLeaseOutcome::Full {
                    profile: request.profile.clone(),
                    max,
                    active,
                });
            }
        }
        let record = LeaseRecord::new(request, checkout_location)?;
        let hold = request.hold.then(|| record.take_hold(state)).transpose()?;
        if let Err(error) = record.write(state) {
            let _ = records::remove(state, &record.id);
            return Err(error);
        }
        Ok(ContainerLeaseOutcome::Leased(ContainerLease {
            record,
            hold,
        }))
    }

    fn repeat_container_lease(
        &self,
        request: &ContainerLeaseRequest,
        mut record: LeaseRecord,
    ) -> Result<ContainerLeaseOutcome> {
        if record.owner != request.owner {
            return Err(Error::refused(format!(
                "container project {} in profile {} is reserved by lease {}",
                request.project, request.profile, record.id
            )));
        }
        // Once eligible for teardown, a lease must be returned before it can
        // serve a new run. Status uses the same account lock as this check.
        if ending(self, &record).is_some() {
            return Err(Error::refused(format!(
                "container lease {} has ended and needs to be returned",
                record.id
            )));
        }
        let state = self.state_dir();
        let hold = request.hold.then(|| record.take_hold(state)).transpose()?;
        if record.hold != request.hold {
            record.hold = request.hold;
            record.write(state)?;
        }
        Ok(ContainerLeaseOutcome::Leased(ContainerLease {
            record,
            hold,
        }))
    }

    /// Release the reservation after the consumer has stopped the stack.
    /// Repeating a return succeeds.
    pub fn container_return(&self, id: &str) -> Result<()> {
        self.core.ensure_writable()?;
        if !root::valid_id(id) {
            return Err(Error::usage(format!("invalid container lease ID {id:?}")));
        }
        let _lock = self.core.state.lock()?;
        records::remove(self.state_dir(), id)
    }

    /// Read all reservations. An uncertain owner or hold keeps the lease live
    /// for cleanup purposes.
    pub fn container_status(&self) -> Result<Vec<ContainerLeaseStatus>> {
        let state = self.state_dir();
        // Lease publication creates this directory under the account lock.
        if !records::directory_exists(state)? {
            return Ok(Vec::new());
        }
        let _lock = self.core.state.read_lock()?;
        let status = records::all(state)?
            .into_iter()
            .map(|record| {
                let ended = ending(self, &record);
                ContainerLeaseStatus {
                    id: record.id,
                    profile: record.profile,
                    project: record.project,
                    owner: record.owner,
                    class: record.class,
                    ended,
                }
            })
            .collect();
        Ok(status)
    }
}

fn ending(barge: &Barge, record: &LeaseRecord) -> Option<ContainerEnding> {
    // An inconclusive owner or lock check keeps the reservation.
    match record
        .owner
        .state(barge, record.checkout_location.as_ref())
        .ok()?
    {
        OwnerState::Ended => {
            return Some(ContainerEnding {
                reason: ContainerEndReason::OwnerEnded,
                remove_volumes: true,
            })
        }
        OwnerState::Unknown => return None,
        OwnerState::Live => {}
    }
    if record.class == ContainerClass::Worker
        && record.hold
        && !record.is_held(barge.state_dir()).ok()?
    {
        return Some(ContainerEnding {
            reason: ContainerEndReason::HoldReleased,
            remove_volumes: false,
        });
    }
    None
}
