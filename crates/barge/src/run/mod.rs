//! Platform requests, placement and execution evidence. The initiator
//! executes a local resolution and records its result.

mod machine_state;
mod machines;
mod targets;
mod wait;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::container::{
    ContainerClass, ContainerLease, ContainerLeaseOutcome, ContainerLeaseRequest,
};
use crate::{os, root, Barge, Result};

use self::machine_state::Unavailable;
use self::targets::Targets;
use self::wait::{Queue, Waiter};

pub use crate::container::ContainerOwner as RunOwner;
pub use machines::{
    MachineEndReason, MachineHold, MachineLease, MachineLeaseStatus, MachineStatus,
};

/// A request for a platform and its minimum capabilities.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunRequest {
    pub platform: PlatformConstraint,
    #[serde(default)]
    pub needs: Capabilities,
    pub source: RunSource,
    pub command: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub budget: Duration,
    pub fallback: Fallback,
    pub owner: RunOwner,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlatformConstraint {
    Exact { os: String, arch: String },
    Os(String),
    Any,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Platform {
    pub os: String,
    pub arch: String,
}

impl PlatformConstraint {
    pub fn matches(&self, platform: &Platform) -> bool {
        self.matches_target(&platform.os, Some(&platform.arch))
    }

    fn matches_target(&self, target_os: &str, target_arch: Option<&str>) -> bool {
        match self {
            Self::Exact { os, arch } => {
                os == target_os && target_arch.map(canonical_arch) == Some(canonical_arch(arch))
            }
            Self::Os(os) => os == target_os,
            Self::Any => true,
        }
    }
}

fn canonical_arch(arch: &str) -> &str {
    match arch {
        "arm64" => "aarch64",
        "amd64" => "x86_64",
        arch => arch,
    }
}

/// Lower bounds on a target's resources. Start with `Default::default()`
/// and set the fields the run needs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(default, deny_unknown_fields)]
pub struct Capabilities {
    pub min_cores: Option<u32>,
    pub min_memory_bytes: Option<u64>,
    pub gpu: bool,
    pub min_vram_bytes: Option<u64>,
}

impl Capabilities {
    fn met_by_host(&self) -> bool {
        !self.needs_gpu()
            && self.min_cores.is_none_or(|min| {
                min == 0
                    || std::thread::available_parallelism()
                        .is_ok_and(|cores| cores.get() as u64 >= u64::from(min))
            })
            && self
                .min_memory_bytes
                .is_none_or(|min| min == 0 || os::memory_bytes().is_some_and(|bytes| bytes >= min))
    }

    fn needs_gpu(&self) -> bool {
        self.gpu || self.min_vram_bytes.unwrap_or_default() > 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunSource {
    pub repo: String,
    pub base: String,
    pub tree: String,
    /// Initiator-local path to a Git bundle.
    pub bundle: Option<PathBuf>,
    /// Initiator-local path to a binary patch.
    pub patch: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Fallback {
    LocalAllowed,
    RemoteOnly,
}

/// The result of an attempted resolution. A container or machine lease
/// reserves a slot. The initiator returns it once the work has stopped.
#[derive(Debug)]
#[must_use]
#[non_exhaustive]
pub enum Resolution {
    LocalNative,
    LocalContainer { lease: ContainerLease },
    Remote { lease: MachineLease },
    Denied { reason: DenyReason },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DenyReason {
    NoMatchingPlatform,
    UnmetCapability,
    FallbackForbidden,
    MachinesFull,
    MachinesHeld,
    ProfileFull {
        profile: String,
        max: u32,
        active: u32,
    },
}

/// Read-only placement preview. A named machine or profile carries no
/// reservation or guarantee of worker capacity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RunPreview {
    LocalNative,
    LocalContainer { profile: String },
    Remote { machine: String },
    Denied { reason: DenyReason },
}

enum FallbackPlacement {
    Native,
    Container(String),
    Denied(DenyReason),
}

/// Advisory position among this process's waiting runs for a machine or
/// profile, starting at one. Other processes compete independently.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Waiting {
    pub machine_or_profile: String,
    pub position: u32,
}

/// Evidence of a run, constructible by the initiator for local execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunResult {
    pub machine: String,
    pub platform: Platform,
    pub repo: String,
    pub base: String,
    pub patch_digest: Option<String>,
    pub tree: String,
    pub toolchain: Option<String>,
    pub command: Vec<String>,
    pub started_ms: u64,
    pub finished_ms: u64,
    pub waking_ms: Option<u64>,
    pub outcome: RunOutcome,
    pub log: LogRef,
    pub area: Option<AreaUse>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RunOutcome {
    Exited { code: i32 },
    Signalled { signal: i32 },
    Infrastructure { stage: RunStage, reason: String },
    Cancelled,
    BudgetExpired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStage {
    Waking,
    Syncing,
    Toolchain,
    Running,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogRef {
    pub digest: String,
    pub location: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AreaUse {
    pub id: String,
    pub bytes: u64,
}

impl Barge {
    /// Resolve a run and hold a worker lease for a remote machine or local
    /// container. `None` returns immediately when full.
    pub fn run_resolve(
        &self,
        request: &RunRequest,
        wait: Option<Duration>,
        progress: &mut dyn FnMut(Waiting),
    ) -> Result<Resolution> {
        let mut targets = Targets::read(self.state_dir())?;
        let (remote, waited) =
            self.resolve_remote(request, &mut targets, wait.unwrap_or_default(), progress)?;
        let unavailable = match remote {
            Ok(lease) => return Ok(Resolution::Remote { lease }),
            Err(unavailable) => unavailable,
        };
        let profile = match self.fallback_placement(request, &targets, unavailable)? {
            FallbackPlacement::Native => return Ok(Resolution::LocalNative),
            FallbackPlacement::Denied(reason) => return Ok(Resolution::Denied { reason }),
            FallbackPlacement::Container(profile) => profile,
        };
        let lease_request = ContainerLeaseRequest {
            profile,
            project: format!("run-{}", root::mint_id()?),
            owner: request.owner.clone(),
            class: ContainerClass::Worker,
            hold: true,
            wait: wait.map(|wait| wait.saturating_sub(waited)),
        };
        let mut waiter = None;
        let outcome = self.container_lease_with_progress(&lease_request, &mut || {
            Waiter::report(
                &mut waiter,
                self.state_dir(),
                Queue::Profile,
                &lease_request.profile,
                progress,
            );
        })?;
        Ok(match outcome {
            ContainerLeaseOutcome::Leased(lease) => Resolution::LocalContainer { lease },
            ContainerLeaseOutcome::Full {
                profile,
                max,
                active,
            } => Resolution::Denied {
                reason: DenyReason::ProfileFull {
                    profile,
                    max,
                    active,
                },
            },
            ContainerLeaseOutcome::NotManaged => Resolution::Denied {
                reason: DenyReason::NoMatchingPlatform,
            },
        })
    }

    /// Preview placement without taking a lease or opening the source files.
    pub fn run_preview(&self, request: &RunRequest) -> Result<RunPreview> {
        let targets = Targets::read(self.state_dir())?;
        let unavailable = match self.preview_remote(request, &targets)? {
            Ok(machine) => {
                return Ok(RunPreview::Remote {
                    machine: machine.to_owned(),
                })
            }
            Err(unavailable) => unavailable,
        };
        Ok(
            match self.fallback_placement(request, &targets, unavailable)? {
                FallbackPlacement::Native => RunPreview::LocalNative,
                FallbackPlacement::Denied(reason) => RunPreview::Denied { reason },
                FallbackPlacement::Container(profile) => RunPreview::LocalContainer { profile },
            },
        )
    }

    fn fallback_placement(
        &self,
        request: &RunRequest,
        targets: &Targets,
        unavailable: Unavailable,
    ) -> Result<FallbackPlacement> {
        let remote_denial = unavailable.denial().or_else(|| {
            targets
                .remote_platform_matches(&request.platform)
                .then_some(DenyReason::UnmetCapability)
        });
        if request.fallback == Fallback::RemoteOnly {
            return Ok(FallbackPlacement::Denied(
                remote_denial.unwrap_or(DenyReason::FallbackForbidden),
            ));
        }
        let host_matches = request
            .platform
            .matches_target(std::env::consts::OS, Some(std::env::consts::ARCH));
        if host_matches && request.needs.met_by_host() {
            return Ok(FallbackPlacement::Native);
        }
        let container = targets.profile(self.state_dir(), &request.platform)?;
        let reason = match container {
            Some(profile) if !request.needs.needs_gpu() => {
                return Ok(FallbackPlacement::Container(profile));
            }
            Some(_) => DenyReason::UnmetCapability,
            None if host_matches => DenyReason::UnmetCapability,
            None => DenyReason::NoMatchingPlatform,
        };
        Ok(FallbackPlacement::Denied(remote_denial.unwrap_or(reason)))
    }
}
