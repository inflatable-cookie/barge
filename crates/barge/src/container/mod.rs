//! Per-profile reservations for container stacks. Consumers start and stop
//! stacks, then return their reservations.

mod profiles;
mod records;

use std::fs::File;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use self::records::LeaseRecord;
use crate::state::validate_name;
use crate::{root, Barge, Error, ErrorKind, Result};

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

    fn ended(&self, barge: &Barge) -> Result<bool> {
        match self {
            Self::Worktree(id) => match barge.worktree_lease_by_id(id) {
                Ok(lease) => Ok(!lease.state.reserves_root()),
                Err(error) if error.kind() == ErrorKind::NotFound => Ok(true),
                Err(error) => Err(error),
            },
            Self::Checkout(path) => path
                .try_exists()
                .map(|exists| !exists)
                .map_err(|error| Error::io("cannot inspect container owner", path, &error)),
        }
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
        if request.owner.ended(self)? {
            return Err(Error::refused("container lease owner has ended"));
        }
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
        let record = LeaseRecord::new(request)?;
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
    if record.owner.ended(barge).ok()? {
        return Some(ContainerEnding {
            reason: ContainerEndReason::OwnerEnded,
            remove_volumes: true,
        });
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
