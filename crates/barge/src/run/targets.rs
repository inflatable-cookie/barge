//! Operator-declared platform mappings for local container profiles.

use std::fs;
use std::path::Path;

use serde::Deserialize;

use super::PlatformConstraint;
use crate::container::read_profiles;
use crate::state::validate_name;
use crate::{Error, Result};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Targets {
    local_container: Vec<ContainerTarget>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ContainerTarget {
    profile: String,
    platforms: Vec<String>,
}

pub(super) fn profile(state: &Path, constraint: &PlatformConstraint) -> Result<Option<String>> {
    let path = state.join("run-targets.json");
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(Error::io("cannot read", &path, &error)),
    };
    let invalid = |message| Error::unknown_state(format!("invalid {}: {message}", path.display()));
    let targets: Targets =
        serde_json::from_slice(&bytes).map_err(|error| invalid(error.to_string()))?;
    let mut matching = Vec::new();
    for target in targets.local_container {
        validate_name("container profile", &target.profile)
            .map_err(|error| invalid(error.to_string()))?;
        let mut matches = false;
        for platform in target.platforms {
            let (os, arch) = platform
                .split_once('-')
                .ok_or_else(|| invalid(format!("expected os-arch, found {platform:?}")))?;
            validate_name("platform OS", os).map_err(|error| invalid(error.to_string()))?;
            validate_name("platform architecture", arch)
                .map_err(|error| invalid(error.to_string()))?;
            matches |= constraint.matches_target(os, (arch != "any").then_some(arch));
        }
        if matches {
            matching.push(target.profile);
        }
    }
    if matching.is_empty() {
        return Ok(None);
    }
    let managed = read_profiles(state)?;
    Ok(matching
        .into_iter()
        .find(|profile| managed.contains_key(profile)))
}
