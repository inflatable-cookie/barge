//! Volumes: a registered external volume, its identity check and its space
//! policy.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::area::AreaStatus;
use crate::error::{Error, Result};
use crate::os::canonical_uuid;
use crate::state::validate_relative_path;
use crate::{Barge, Core};

/// The schema name of `volumes.json`.
pub const VOLUMES_SCHEMA: &str = "barge.volumes/1";

/// The floor that refuses work when it is set explicitly and nothing idle is
/// left to evict.
pub const DEFAULT_FLOOR_BYTES: u64 = 10 * 1024 * 1024 * 1024;

/// Whether a volume is expected to stay attached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Availability {
    Permanent,
    Ephemeral,
}

/// Whether the data on a volume can be rebuilt and pruned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Durability {
    Expendable,
    Kept,
}

/// A registered volume as it is stored in `volumes.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolumeSpec {
    pub name: String,
    /// Canonical uppercase UUID of the volume expected at `mount`.
    pub uuid: String,
    pub mount: PathBuf,
    /// Folder inside the mount that holds Barge's areas; `""` is the mount root.
    pub root_dir: String,
    pub availability: Availability,
    pub durability: Durability,
    /// Below this, prune evicts least-recently-used idle unpinned roots.
    pub reserve_bytes: u64,
    /// Below this, `admit` refuses once nothing idle is left.
    pub floor_bytes: u64,
}

impl VolumeSpec {
    /// Validate every stored field before the record is used as truth.
    pub fn validate(&self) -> Result<()> {
        self.validate_common()?;
        canonical_uuid(&self.uuid)?;
        Ok(())
    }

    /// Validate a registration whose identity is read at the mount when empty.
    fn validate_pending(&self) -> Result<()> {
        self.validate_common()?;
        if !self.uuid.is_empty() {
            canonical_uuid(&self.uuid)?;
        }
        Ok(())
    }

    fn validate_common(&self) -> Result<()> {
        crate::state::validate_volume_name("volume name", &self.name)?;
        if !self.mount.is_absolute() {
            return Err(Error::usage(format!(
                "volume {} mount {} is not absolute",
                self.name,
                self.mount.display()
            )));
        }
        validate_relative_path("volume root_dir", &self.root_dir)?;
        if self.floor_bytes > 0 && self.reserve_bytes > 0 && self.floor_bytes > self.reserve_bytes {
            return Err(Error::usage(format!(
                "volume {} floor {} is above its reserve {}",
                self.name, self.floor_bytes, self.reserve_bytes
            )));
        }
        Ok(())
    }

    /// The folder inside the mount that holds this volume's areas.
    pub fn base(&self) -> PathBuf {
        if self.root_dir.is_empty() {
            self.mount.clone()
        } else {
            self.mount.join(&self.root_dir)
        }
    }
}

/// A caller's request to register or update a volume. The identity is read at
/// the mount when the caller does not supply it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeRegistration {
    pub name: String,
    pub uuid: Option<String>,
    pub mount: PathBuf,
    pub root_dir: String,
    pub availability: Availability,
    pub durability: Durability,
    pub reserve_bytes: u64,
    pub floor_bytes: u64,
}

impl VolumeRegistration {
    /// A permanent, expendable volume at `mount` with the default floor.
    pub fn new(name: impl Into<String>, mount: impl Into<PathBuf>) -> Self {
        Self {
            name: name.into(),
            uuid: None,
            mount: mount.into(),
            root_dir: String::new(),
            availability: Availability::Permanent,
            durability: Durability::Expendable,
            reserve_bytes: 0,
            floor_bytes: DEFAULT_FLOOR_BYTES,
        }
    }

    pub fn uuid(mut self, uuid: Option<impl Into<String>>) -> Self {
        self.uuid = uuid.map(Into::into);
        self
    }

    pub fn root_dir(mut self, root_dir: impl Into<String>) -> Self {
        self.root_dir = root_dir.into();
        self
    }

    pub fn availability(mut self, availability: Availability) -> Self {
        self.availability = availability;
        self
    }

    pub fn durability(mut self, durability: Durability) -> Self {
        self.durability = durability;
        self
    }

    pub fn reserve_bytes(mut self, reserve_bytes: u64) -> Self {
        self.reserve_bytes = reserve_bytes;
        self
    }

    pub fn floor_bytes(mut self, floor_bytes: u64) -> Self {
        self.floor_bytes = floor_bytes;
        self
    }

    pub(crate) fn into_spec(self) -> Result<VolumeSpec> {
        let uuid = match self.uuid {
            Some(uuid) if !uuid.trim().is_empty() => canonical_uuid(&uuid)?,
            _ => String::new(),
        };
        let spec = VolumeSpec {
            name: self.name,
            uuid,
            mount: self.mount,
            root_dir: self.root_dir,
            availability: self.availability,
            durability: self.durability,
            reserve_bytes: self.reserve_bytes,
            floor_bytes: self.floor_bytes,
        };
        spec.validate_pending()?;
        Ok(spec)
    }
}

/// What Barge finds at a registered volume's mount.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum VolumeState {
    /// The registered UUID is at the registered mount.
    Present,
    /// Nothing is mounted, or its identity cannot be read.
    Absent,
    /// A different volume is at the registered mount.
    Mismatch { registered: String, found: String },
}

/// A registered volume with the platform seam Barge checks it through.
#[derive(Debug, Clone)]
pub struct Volume {
    core: Core,
    spec: VolumeSpec,
}

impl Volume {
    pub(crate) fn new(core: Core, spec: VolumeSpec) -> Self {
        Self { core, spec }
    }

    pub fn spec(&self) -> &VolumeSpec {
        &self.spec
    }

    pub fn name(&self) -> &str {
        &self.spec.name
    }

    /// The folder inside the mount that holds this volume's areas.
    pub fn base(&self) -> PathBuf {
        self.spec.base()
    }

    /// Check the volume identity at the registered mount.
    pub fn state(&self) -> Result<VolumeState> {
        let found = self.core.os.volume_identity(&self.spec.mount)?;
        Ok(match found {
            Some(found) if found.eq_ignore_ascii_case(&self.spec.uuid) => VolumeState::Present,
            Some(found) => VolumeState::Mismatch {
                registered: self.spec.uuid.clone(),
                found,
            },
            None => VolumeState::Absent,
        })
    }

    /// Return the mount, refusing an absent volume or a different one.
    pub fn require_present(&self) -> Result<PathBuf> {
        match self.state()? {
            VolumeState::Present => Ok(self.spec.mount.clone()),
            VolumeState::Absent => Err(Error::refused(format!(
                "volume {} is absent; attach it at {}",
                self.spec.name,
                self.spec.mount.display()
            ))),
            VolumeState::Mismatch { registered, found } => Err(Error::refused(format!(
                "volume at {} is {found}, not the registered {registered}; refusing to use it",
                self.spec.mount.display()
            ))),
        }
    }

    /// Free bytes at the mount.
    pub fn free_bytes(&self) -> Result<u64> {
        let mount = self.require_present()?;
        self.core.os.free_bytes(&mount)
    }
}

/// One volume in the status document.
#[derive(Debug, Clone, Serialize)]
pub struct VolumeStatus {
    #[serde(flatten)]
    pub spec: VolumeSpec,
    pub state: VolumeState,
    pub free_bytes: Option<u64>,
    pub areas: Vec<AreaStatus>,
}

/// Every volume, for status output.
pub type Status = Vec<VolumeStatus>;

impl Barge {
    /// Register a volume, reading its identity at the mount when the caller did
    /// not supply one.
    pub fn register_volume(&self, registration: VolumeRegistration) -> Result<Volume> {
        self.core.ensure_writable()?;
        let mut spec = registration.into_spec()?;
        let _lock = self.core.state.lock()?;
        let mut stored = self.core.state.read_volumes()?;
        let found = match self.core.os.volume_identity(&spec.mount)? {
            Some(found) => found,
            None => {
                return Err(Error::refused(format!(
                    "cannot read a volume identity at {}; mount the volume first",
                    spec.mount.display()
                )))
            }
        };
        if !spec.uuid.is_empty() && !spec.uuid.eq_ignore_ascii_case(&found) {
            return Err(Error::refused(format!(
                "volume at {} is {found}, not the declared {}",
                spec.mount.display(),
                spec.uuid
            )));
        }
        spec.uuid = found;
        for existing in &stored {
            if existing.name != spec.name && existing.uuid.eq_ignore_ascii_case(&spec.uuid) {
                return Err(Error::refused(format!(
                    "volume identity {} is already registered as {}",
                    spec.uuid, existing.name
                )));
            }
        }
        match stored
            .iter()
            .position(|existing| existing.name == spec.name)
        {
            Some(index) => {
                if !stored[index].uuid.eq_ignore_ascii_case(&spec.uuid) {
                    return Err(Error::refused(format!(
                        "volume {} is registered as {}, not {}",
                        spec.name, stored[index].uuid, spec.uuid
                    )));
                }
                stored[index] = spec.clone();
            }
            None => stored.push(spec.clone()),
        }
        stored.sort_by(|left, right| left.name.cmp(&right.name));
        self.core.state.write_volumes(&stored)?;
        Ok(Volume::new(self.core.clone(), spec))
    }
}
