//! Durable leases and their cooperative hold files in the account directory.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{CheckoutLocation, ContainerClass, ContainerLeaseRequest, ContainerOwner};
use crate::state::{publish, validate_name};
use crate::{os, root, Error, Mode, Result};

const DIR: &str = "container-leases";
const SCHEMA: &str = "barge.container-lease/1";

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct LeaseRecord {
    schema: String,
    pub(super) id: String,
    pub(super) profile: String,
    pub(super) project: String,
    pub(super) owner: ContainerOwner,
    pub(super) class: ContainerClass,
    pub(super) hold: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) checkout_location: Option<CheckoutLocation>,
}

impl LeaseRecord {
    pub(super) fn new(
        request: &ContainerLeaseRequest,
        checkout_location: Option<CheckoutLocation>,
    ) -> Result<Self> {
        Ok(Self {
            schema: SCHEMA.to_owned(),
            id: root::mint_id()?,
            profile: request.profile.clone(),
            project: request.project.clone(),
            owner: request.owner.clone(),
            class: request.class,
            hold: request.hold,
            checkout_location,
        })
    }

    pub(super) fn key(&self) -> (&str, &str) {
        (&self.profile, &self.project)
    }

    pub(super) fn take_hold(&self, state: &Path) -> Result<File> {
        let dir = state.join(DIR);
        fs::create_dir_all(&dir).map_err(|error| Error::io("cannot create", &dir, &error))?;
        let path = hold_path(state, &self.id);
        let file = os::open_lock(&path, "cannot open the container hold")?;
        os::flock(&file, Mode::Shared, true)?;
        Ok(file)
    }

    pub(super) fn is_held(&self, state: &Path) -> Result<bool> {
        let path = hold_path(state, &self.id);
        // A record that expects a hold was published with a lock file. Its
        // absence is unknown, as an open descriptor may still hold the lock.
        let file = File::open(&path)
            .map_err(|error| Error::io("cannot open the container hold", &path, &error))?;
        Ok(!os::flock(&file, Mode::Exclusive, false)?)
    }

    pub(super) fn write(&self, state: &Path) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(self).map_err(|error| {
            Error::unknown_state(format!("cannot encode container lease: {error}"))
        })?;
        publish(&record_path(state, &self.id), &bytes)
    }
}

fn record_path(state: &Path, id: &str) -> PathBuf {
    state.join(DIR).join(format!("{id}.json"))
}

fn hold_path(state: &Path, id: &str) -> PathBuf {
    state.join(DIR).join(format!("{id}.lock"))
}

fn read(state: &Path, id: &str) -> Result<LeaseRecord> {
    let path = record_path(state, id);
    let bytes = fs::read(&path).map_err(|error| Error::io("cannot read", &path, &error))?;
    let invalid = |message| Error::unknown_state(format!("invalid {}: {message}", path.display()));
    let lease: LeaseRecord =
        serde_json::from_slice(&bytes).map_err(|error| invalid(error.to_string()))?;
    if lease.schema != SCHEMA || lease.id != id || !root::valid_id(id) {
        return Err(invalid("schema or lease ID does not match".to_owned()));
    }
    validate_name("container profile", &lease.profile)
        .and_then(|()| validate_name("container project", &lease.project))
        .and_then(|()| lease.owner.validate())
        .map_err(|error| invalid(error.to_string()))?;
    Ok(lease)
}

pub(super) fn directory_exists(state: &Path) -> Result<bool> {
    let dir = state.join(DIR);
    dir.try_exists()
        .map_err(|error| Error::io("cannot inspect", &dir, &error))
}

pub(super) fn all(state: &Path) -> Result<Vec<LeaseRecord>> {
    let dir = state.join(DIR);
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(Error::io("cannot list", &dir, &error)),
    };
    let mut leases = Vec::new();
    for entry in entries {
        let path = entry
            .map_err(|error| Error::io("cannot list", &dir, &error))?
            .path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let id = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .ok_or_else(|| {
                Error::unknown_state(format!("invalid container lease path {}", path.display()))
            })?;
        leases.push(read(state, id)?);
    }
    let mut keys = BTreeSet::new();
    for lease in &leases {
        if !keys.insert(lease.key()) {
            return Err(Error::unknown_state(format!(
                "multiple leases for container project {} in profile {}",
                lease.project, lease.profile
            )));
        }
    }
    leases.sort_unstable_by(|left, right| left.key().cmp(&right.key()));
    Ok(leases)
}

pub(super) fn remove(state: &Path, id: &str) -> Result<()> {
    remove_file(&record_path(state, id))?;
    // The sidecar protects only this ID. An interrupted removal can leave it
    // behind without affecting a later reservation.
    let _ = remove_file(&hold_path(state, id));
    Ok(())
}

fn remove_file(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(Error::io("cannot remove", path, &error)),
    }
}
