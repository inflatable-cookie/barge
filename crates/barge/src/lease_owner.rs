//! Liveness evidence shared by container and machine leases.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{os, root, Barge, Error, ErrorKind, Result, VolumeState};

/// The worktree reservation or checkout directory that owns a reservation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContainerOwner {
    Worktree(String),
    Checkout(PathBuf),
}

impl ContainerOwner {
    pub(crate) fn validate(&self) -> Result<()> {
        match self {
            Self::Worktree(id) if !root::valid_id(id) => {
                Err(Error::usage(format!("invalid worktree lease ID {id:?}")))
            }
            Self::Checkout(path)
                if !path.is_absolute() || path.to_str().is_none_or(|path| path.contains('\0')) =>
            {
                Err(Error::usage(
                    "lease checkout must be an absolute UTF-8 path",
                ))
            }
            _ => Ok(()),
        }
    }

    pub(crate) fn state(
        &self,
        barge: &Barge,
        location: Option<&CheckoutLocation>,
    ) -> Result<OwnerState> {
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
pub(crate) enum OwnerState {
    Live,
    Ended,
    Unknown,
}

// Resolve symlinks while the checkout exists, so a later dangling link cannot
// hide the filesystem where the checkout lived.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct CheckoutLocation {
    path: PathBuf,
    device: u64,
}

impl CheckoutLocation {
    pub(crate) fn capture(owner: &ContainerOwner) -> Result<Option<Self>> {
        let ContainerOwner::Checkout(path) = owner else {
            return Ok(None);
        };
        let resolved = path
            .canonicalize()
            .map_err(|error| Error::io("cannot resolve lease owner", path, &error))?;
        let device = os::path_device(&resolved)
            .map_err(|error| Error::io("cannot inspect lease owner", &resolved, &error))?;
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
            .map_err(|error| Error::io("cannot inspect lease owner", path, &error))
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
                    return Err(Error::io("cannot inspect lease owner", ancestor, &error))
                }
            }
        }
    }
    match resolved.parent() {
        Some(parent) if exists(parent)? => Ok(OwnerState::Ended),
        _ => Ok(OwnerState::Unknown),
    }
}
