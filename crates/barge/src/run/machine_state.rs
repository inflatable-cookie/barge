//! Durable slot assignments and operator holds, written under the account lock.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::targets::{validate_machine, RemoteTarget};
use super::RunOwner;
use crate::lease_owner::CheckoutLocation;
use crate::state::publish;
use crate::{os, root, Error, Mode, Result};

const SCHEMA: &str = "barge.machine-leases/1";

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct MachineState {
    schema: String,
    pub(super) machines: BTreeMap<String, MachineRecord>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct MachineRecord {
    pub(super) hold: Option<HoldRecord>,
    pub(super) slots: BTreeMap<u32, SlotRecord>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct HoldRecord {
    pub(super) reason: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SlotRecord {
    pub(super) repo: String,
    pub(super) lease: Option<LeaseRecord>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct LeaseRecord {
    pub(super) id: String,
    pub(super) owner: RunOwner,
    pub(super) checkout_location: Option<CheckoutLocation>,
}

pub(super) enum Unavailable {
    Full { machine: String },
    Held,
    NoMatch,
}

impl MachineState {
    pub(super) fn exists(state: &Path) -> Result<bool> {
        let path = state.join("machine-leases.json");
        path.try_exists()
            .map_err(|error| Error::io("cannot inspect", &path, &error))
    }

    pub(super) fn read(state: &Path) -> Result<Self> {
        let path = state.join("machine-leases.json");
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self {
                    schema: SCHEMA.to_owned(),
                    machines: BTreeMap::new(),
                });
            }
            Err(error) => return Err(Error::io("cannot read", &path, &error)),
        };
        let invalid =
            |message| Error::unknown_state(format!("invalid {}: {message}", path.display()));
        let records: Self =
            serde_json::from_slice(&bytes).map_err(|error| invalid(error.to_string()))?;
        if records.schema != SCHEMA {
            return Err(invalid(format!("expected schema {SCHEMA}")));
        }
        let mut ids = BTreeSet::new();
        for (machine, record) in &records.machines {
            validate_machine(machine).map_err(|error| invalid(error.to_string()))?;
            for lease in record.slots.values().filter_map(|slot| slot.lease.as_ref()) {
                if !root::valid_id(&lease.id) || !ids.insert(&lease.id) {
                    return Err(invalid(format!(
                        "invalid or duplicate lease ID {:?}",
                        lease.id
                    )));
                }
                lease
                    .owner
                    .validate()
                    .map_err(|error| invalid(error.to_string()))?;
            }
        }
        Ok(records)
    }

    pub(super) fn write(&self, state: &Path) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(self).map_err(|error| {
            Error::unknown_state(format!("cannot encode machine leases: {error}"))
        })?;
        publish(&state.join("machine-leases.json"), &bytes)
    }

    /// Prefer warmth across all eligible machines, with file order breaking ties.
    pub(super) fn select<'a>(
        &self,
        targets: impl Iterator<Item = &'a RemoteTarget>,
        repo: &str,
    ) -> std::result::Result<(&'a str, u32), Unavailable> {
        let mut cold = None;
        let mut full = None;
        let mut held = false;
        for target in targets {
            let machine = target.machine.as_str();
            let record = self.machines.get(machine);
            if record.is_some_and(|record| record.hold.is_some()) {
                held = true;
                continue;
            }
            let active = record.map_or(0, |record| {
                record
                    .slots
                    .values()
                    .filter(|slot| slot.lease.is_some())
                    .count()
            });
            if active >= target.max as usize {
                full.get_or_insert(machine);
                continue;
            }
            if let Some(record) = record {
                if let Some((&slot, _)) = record
                    .slots
                    .range(..target.max)
                    .find(|(_, slot)| slot.lease.is_none() && slot.repo == repo)
                {
                    return Ok((machine, slot));
                }
            }
            if cold.is_none() {
                let slot = (0..target.max).find(|slot| {
                    record
                        .and_then(|record| record.slots.get(slot))
                        .is_none_or(|slot| slot.lease.is_none())
                });
                cold = slot.map(|slot| (machine, slot));
            }
        }
        cold.ok_or_else(|| match full {
            Some(machine) => Unavailable::Full {
                machine: machine.to_owned(),
            },
            None if held => Unavailable::Held,
            None => Unavailable::NoMatch,
        })
    }
}

impl LeaseRecord {
    pub(super) fn take_hold(&self, state: &Path) -> Result<File> {
        let dir = state.join("machine-lease-holds");
        fs::create_dir_all(&dir).map_err(|error| Error::io("cannot create", &dir, &error))?;
        let path = hold_path(state, &self.id);
        let file = os::open_lock(&path, "cannot open the machine lease hold")?;
        os::flock(&file, Mode::Shared, true)?;
        Ok(file)
    }

    pub(super) fn is_held(&self, state: &Path) -> Result<bool> {
        let path = hold_path(state, &self.id);
        // A missing lock file cannot establish that an open descriptor is gone.
        let file = File::open(&path)
            .map_err(|error| Error::io("cannot open the machine lease hold", &path, &error))?;
        Ok(!os::flock(&file, Mode::Exclusive, false)?)
    }
}

pub(super) fn remove_hold(state: &Path, id: &str) {
    // A leftover lock file protects only its old, unique lease ID.
    let _ = fs::remove_file(hold_path(state, id));
}

fn hold_path(state: &Path, id: &str) -> PathBuf {
    state.join("machine-lease-holds").join(format!("{id}.lock"))
}
