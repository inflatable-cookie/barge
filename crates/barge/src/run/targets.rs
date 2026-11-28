//! Operator-declared remote machines and local container profiles.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::{Capabilities, Platform, PlatformConstraint, RunRequest};
use crate::container::read_profiles;
use crate::state::validate_name;
use crate::{Error, Result};

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct Targets {
    local_container: Vec<ContainerTarget>,
    remote: Vec<RemoteTarget>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ContainerTarget {
    profile: String,
    platforms: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RemoteTarget {
    pub(super) machine: String,
    pub(super) max: u32,
    #[serde(deserialize_with = "remote_platform")]
    platform: Platform,
    #[serde(default)]
    capabilities: RemoteCapabilities,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RemoteCapabilities {
    cores: u32,
    memory_gib: u64,
    gpu: bool,
    vram_gib: u64,
}

impl RemoteCapabilities {
    fn meets(&self, needs: &Capabilities) -> bool {
        const GIB: u64 = 1024 * 1024 * 1024;
        self.cores >= needs.min_cores.unwrap_or_default()
            && self.memory_gib >= needs.min_memory_bytes.unwrap_or_default().div_ceil(GIB)
            && (!needs.needs_gpu() || self.gpu)
            && self.vram_gib >= needs.min_vram_bytes.unwrap_or_default().div_ceil(GIB)
    }
}

impl Targets {
    pub(super) fn read(state: &Path) -> Result<Self> {
        let path = targets_path(state);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default())
            }
            Err(error) => return Err(Error::io("cannot read", &path, &error)),
        };
        let targets: Self =
            serde_json::from_slice(&bytes).map_err(|error| invalid(&path, error))?;
        let mut machines = BTreeSet::new();
        for target in &targets.remote {
            validate_machine(&target.machine).map_err(|error| invalid(&path, error))?;
            if !machines.insert(&target.machine) {
                return Err(invalid(
                    &path,
                    format_args!("machine {:?} is declared twice", target.machine),
                ));
            }
        }
        Ok(targets)
    }

    pub(super) fn matching_remote<'a>(
        &'a self,
        request: &'a RunRequest,
    ) -> impl Iterator<Item = &'a RemoteTarget> {
        self.remote.iter().filter(|target| {
            request.platform.matches(&target.platform) && target.capabilities.meets(&request.needs)
        })
    }

    pub(super) fn remote_platform_matches(&self, constraint: &PlatformConstraint) -> bool {
        self.remote
            .iter()
            .any(|target| constraint.matches(&target.platform))
    }

    pub(super) fn profile(
        &self,
        state: &Path,
        constraint: &PlatformConstraint,
    ) -> Result<Option<String>> {
        let path = targets_path(state);
        let mut matching = Vec::new();
        for target in &self.local_container {
            validate_name("container profile", &target.profile)
                .map_err(|error| invalid(&path, error))?;
            let mut matches = false;
            for platform in &target.platforms {
                let (os, arch) = platform_parts(platform).map_err(|error| invalid(&path, error))?;
                matches |= constraint.matches_target(os, (arch != "any").then_some(arch));
            }
            if matches {
                matching.push(&target.profile);
            }
        }
        if matching.is_empty() {
            return Ok(None);
        }
        let managed = read_profiles(state)?;
        Ok(matching
            .into_iter()
            .find(|profile| managed.contains_key(*profile))
            .cloned())
    }
}

pub(super) fn validate_machine(machine: &str) -> Result<()> {
    if machine.trim().is_empty() {
        return Err(Error::usage("machine ID must not be empty"));
    }
    Ok(())
}

fn targets_path(state: &Path) -> PathBuf {
    state.join("run-targets.json")
}

fn invalid(path: &Path, error: impl std::fmt::Display) -> Error {
    Error::unknown_state(format!("invalid {}: {error}", path.display()))
}

fn platform_parts(platform: &str) -> Result<(&str, &str)> {
    let (os, arch) = platform
        .split_once('-')
        .ok_or_else(|| Error::usage(format!("expected os-arch, found {platform:?}")))?;
    validate_name("platform OS", os)?;
    validate_name("platform architecture", arch)?;
    Ok((os, arch))
}

fn remote_platform<'de, D: serde::Deserializer<'de>>(
    input: D,
) -> std::result::Result<Platform, D::Error> {
    let platform = String::deserialize(input)?;
    let (os, arch) = platform_parts(&platform).map_err(serde::de::Error::custom)?;
    if arch == "any" {
        return Err(serde::de::Error::custom(
            "remote target needs a concrete architecture",
        ));
    }
    Ok(Platform {
        os: os.to_owned(),
        arch: arch.to_owned(),
    })
}
