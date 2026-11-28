//! Account-scoped leases on remote machine slots. Consumers stop work before
//! returning a slot.

use std::fs::File;
use std::time::{Duration, Instant};

use serde::Serialize;

use super::machine_state::{self, HoldRecord, LeaseRecord, MachineState, SlotRecord, Unavailable};
use super::targets::{validate_machine, Targets};
use super::wait::{Queue, Waiter};
use super::{DenyReason, RunOwner, RunRequest, Waiting};
use crate::lease_owner::{CheckoutLocation, OwnerState};
use crate::{root, Barge, Error, Result};

const POLL: Duration = Duration::from_millis(20);

/// One reserved remote slot. Dropping the lease releases its cooperative hold.
#[derive(Debug)]
#[must_use]
pub struct MachineLease {
    id: String,
    machine: String,
    slot: u32,
    _hold: File,
}

impl MachineLease {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn machine(&self) -> &str {
        &self.machine
    }

    pub fn slot(&self) -> u32 {
        self.slot
    }

    /// Every live handle carries a hold.
    pub fn is_held(&self) -> bool {
        true
    }
}

/// Local account reservations and operator holds.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct MachineStatus {
    pub leases: Vec<MachineLeaseStatus>,
    pub holds: Vec<MachineHold>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MachineLeaseStatus {
    pub id: String,
    pub machine: String,
    pub slot: u32,
    pub repo: String,
    pub owner: RunOwner,
    pub ended: Option<MachineEndReason>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MachineEndReason {
    OwnerEnded,
    HoldReleased,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MachineHold {
    pub machine: String,
    pub reason: Option<String>,
}

impl Barge {
    /// Stop taking new leases on this machine. Existing leases can finish.
    pub fn machine_hold(&self, machine: &str, reason: Option<String>) -> Result<()> {
        validate_machine(machine)?;
        self.core.ensure_writable()?;
        let _lock = self.core.state.lock()?;
        let state = self.state_dir();
        let mut records = MachineState::read(state)?;
        records.machines.entry(machine.to_owned()).or_default().hold = Some(HoldRecord { reason });
        records.write(state)
    }

    /// Allow new leases on this machine. Releasing an absent hold succeeds.
    pub fn machine_release(&self, machine: &str) -> Result<()> {
        validate_machine(machine)?;
        self.core.ensure_writable()?;
        let _lock = self.core.state.lock()?;
        let state = self.state_dir();
        let mut records = MachineState::read(state)?;
        let Some(record) = records.machines.get_mut(machine) else {
            return Ok(());
        };
        if record.hold.take().is_none() {
            return Ok(());
        }
        if record.slots.is_empty() {
            records.machines.remove(machine);
        }
        records.write(state)
    }

    /// Return the slot after the consumer has stopped its work. Repeating a
    /// return succeeds. The last repository is kept for warm placement.
    pub fn machine_return(&self, id: &str) -> Result<()> {
        self.core.ensure_writable()?;
        if !root::valid_id(id) {
            return Err(Error::usage(format!("invalid machine lease ID {id:?}")));
        }
        let _lock = self.core.state.lock()?;
        let state = self.state_dir();
        let mut records = MachineState::read(state)?;
        let slot = records
            .machines
            .values_mut()
            .flat_map(|machine| machine.slots.values_mut())
            .find(|slot| slot.lease.as_ref().is_some_and(|lease| lease.id == id));
        if let Some(slot) = slot {
            slot.lease = None;
            records.write(state)?;
        }
        machine_state::remove_hold(state, id);
        Ok(())
    }

    /// Read reservations with their endings, plus operator holds. Uncertain
    /// liveness leaves the reservation in place.
    pub fn machine_status(&self) -> Result<MachineStatus> {
        let state = self.state_dir();
        if !MachineState::exists(state)? {
            return Ok(MachineStatus::default());
        }
        let _lock = self.core.state.read_lock()?;
        let mut status = MachineStatus::default();
        for (machine, record) in MachineState::read(state)?.machines {
            if let Some(hold) = record.hold {
                status.holds.push(MachineHold {
                    machine: machine.clone(),
                    reason: hold.reason,
                });
            }
            for (slot, record) in record.slots {
                if let Some(lease) = record.lease {
                    let ended = ending(self, &lease);
                    status.leases.push(MachineLeaseStatus {
                        id: lease.id,
                        machine: machine.clone(),
                        slot,
                        repo: record.repo,
                        owner: lease.owner,
                        ended,
                    });
                }
            }
        }
        Ok(status)
    }

    pub(super) fn preview_remote<'a>(
        &self,
        request: &'a RunRequest,
        targets: &'a Targets,
    ) -> Result<std::result::Result<&'a str, Unavailable>> {
        let mut matching = targets.matching_remote(request).peekable();
        if matching.peek().is_none() {
            return Ok(Err(Unavailable::NoMatch));
        }
        Ok(MachineState::read(self.state_dir())?
            .select(matching, &request.source.repo)
            .map(|(machine, _)| machine))
    }

    pub(super) fn resolve_remote(
        &self,
        request: &RunRequest,
        targets: &mut Targets,
        wait: Duration,
        progress: &mut dyn FnMut(Waiting),
    ) -> Result<(std::result::Result<MachineLease, Unavailable>, Duration)> {
        if targets.matching_remote(request).next().is_none() {
            return Ok((Err(Unavailable::NoMatch), Duration::ZERO));
        }

        self.core.ensure_writable()?;
        let mut outcome = {
            let _lock = self.core.state.lock()?;
            self.try_machine_lease(request, targets)?
        };
        let started = Instant::now();
        let mut waiter = None;
        while let Err(Unavailable::Full { machine }) = &outcome {
            if started.elapsed() >= wait {
                break;
            }
            Waiter::report(
                &mut waiter,
                self.state_dir(),
                Queue::Machine,
                machine,
                progress,
            );
            let remaining = wait.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                break;
            }
            std::thread::sleep(POLL.min(remaining));
            if let Some(_lock) = self.core.state.try_lock()? {
                outcome = self.try_machine_lease(request, targets)?;
            }
        }
        Ok((outcome, started.elapsed()))
    }

    // The caller holds the account lock across selection and publication.
    fn try_machine_lease(
        &self,
        request: &RunRequest,
        targets: &mut Targets,
    ) -> Result<std::result::Result<MachineLease, Unavailable>> {
        let state = self.state_dir();
        *targets = Targets::read(state)?;
        let mut records = MachineState::read(state)?;
        let (machine, slot) =
            match records.select(targets.matching_remote(request), &request.source.repo) {
                Ok(choice) => choice,
                Err(unavailable) => return Ok(Err(unavailable)),
            };
        request.owner.validate()?;
        match request.owner.state(self, None)? {
            OwnerState::Live => {}
            OwnerState::Ended => return Err(Error::refused("machine lease owner has ended")),
            OwnerState::Unknown => return Err(Error::refused("machine lease owner is unknown")),
        }
        let lease = LeaseRecord {
            id: root::mint_id()?,
            owner: request.owner.clone(),
            checkout_location: CheckoutLocation::capture(&request.owner)?,
        };
        let hold = lease.take_hold(state)?;
        let id = lease.id.clone();
        records
            .machines
            .entry(machine.to_owned())
            .or_default()
            .slots
            .insert(
                slot,
                SlotRecord {
                    repo: request.source.repo.clone(),
                    lease: Some(lease),
                },
            );
        if let Err(error) = records.write(state) {
            machine_state::remove_hold(state, &id);
            return Err(error);
        }
        Ok(Ok(MachineLease {
            id,
            machine: machine.to_owned(),
            slot,
            _hold: hold,
        }))
    }
}

impl Unavailable {
    pub(super) fn denial(&self) -> Option<DenyReason> {
        match self {
            Self::Full { .. } => Some(DenyReason::MachinesFull),
            Self::Held => Some(DenyReason::MachinesHeld),
            Self::NoMatch => None,
        }
    }
}

fn ending(barge: &Barge, lease: &LeaseRecord) -> Option<MachineEndReason> {
    match lease
        .owner
        .state(barge, lease.checkout_location.as_ref())
        .ok()?
    {
        OwnerState::Ended => Some(MachineEndReason::OwnerEnded),
        OwnerState::Unknown => None,
        OwnerState::Live if !lease.is_held(barge.state_dir()).ok()? => {
            Some(MachineEndReason::HoldReleased)
        }
        OwnerState::Live => None,
    }
}
