//! A fake platform seam for tests: named mounts with a UUID and a free-space
//! reading a test can change.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::error::{Error, Result};

use super::{canonical_uuid, Os};

/// One mount the fake reports.
#[derive(Debug, Clone)]
struct FakeVolume {
    mount: PathBuf,
    uuid: Option<String>,
    free_bytes: u64,
}

/// A platform seam whose mounts, identities and free space are set by the test.
#[derive(Debug, Clone, Default)]
pub struct FakeOs {
    volumes: Arc<Mutex<Vec<FakeVolume>>>,
}

impl FakeOs {
    pub fn new() -> Self {
        Self::default()
    }

    /// A fake with one present volume.
    pub fn with_volume(mount: impl Into<PathBuf>, uuid: &str, free_bytes: u64) -> Self {
        let fake = Self::new();
        fake.add_volume(mount, Some(uuid), free_bytes);
        fake
    }

    /// Add or replace one mount.
    pub fn add_volume(&self, mount: impl Into<PathBuf>, uuid: Option<&str>, free_bytes: u64) {
        let mount = mount.into();
        let uuid = uuid.map(|uuid| canonical_uuid(uuid).unwrap_or_else(|_| uuid.to_string()));
        let mut volumes = self
            .volumes
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        volumes.retain(|volume| volume.mount != mount);
        volumes.push(FakeVolume {
            mount,
            uuid,
            free_bytes,
        });
    }

    /// Change the identity a mount reports, as a different drive at the same
    /// mount point would.
    pub fn set_uuid(&self, mount: &Path, uuid: Option<&str>) {
        let uuid = uuid.map(|uuid| canonical_uuid(uuid).unwrap_or_else(|_| uuid.to_string()));
        let mut volumes = self
            .volumes
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Some(volume) = volumes.iter_mut().find(|volume| volume.mount == mount) {
            volume.uuid = uuid;
        }
    }

    /// Change the free-space reading of the mount that contains `path`.
    pub fn set_free(&self, mount: &Path, free_bytes: u64) {
        let mut volumes = self
            .volumes
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Some(volume) = volumes.iter_mut().find(|volume| volume.mount == mount) {
            volume.free_bytes = free_bytes;
        }
    }

    /// The free-space reading the fake reports for `mount`.
    pub fn free(&self, mount: &Path) -> Option<u64> {
        let volumes = self
            .volumes
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        volumes
            .iter()
            .find(|volume| volume.mount == mount)
            .map(|volume| volume.free_bytes)
    }

    fn lookup(&self, path: &Path) -> Option<FakeVolume> {
        let volumes = self
            .volumes
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        volumes
            .iter()
            .filter(|volume| path.starts_with(&volume.mount))
            .max_by_key(|volume| volume.mount.as_os_str().len())
            .cloned()
    }
}

impl Os for FakeOs {
    fn volume_identity(&self, mount: &Path) -> Result<Option<String>> {
        match self.lookup(mount) {
            Some(volume) if volume.mount == mount => Ok(volume.uuid),
            Some(_) => Ok(None),
            None => Ok(None),
        }
    }

    fn free_bytes(&self, path: &Path) -> Result<u64> {
        self.lookup(path)
            .map(|volume| volume.free_bytes)
            .ok_or_else(|| {
                Error::not_found(format!("no fake volume is mounted at {}", path.display()))
            })
    }
}
