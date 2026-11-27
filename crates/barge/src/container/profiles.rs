//! Operator-declared worker capacity per runtime profile.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde::Deserialize;

use crate::state::validate_name;
use crate::{Error, Result};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Profile {
    name: String,
    max: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    profiles: Vec<Profile>,
}

/// A missing file declares no profiles. An unreadable declaration fails closed.
pub(crate) fn read(state: &Path) -> Result<BTreeMap<String, u32>> {
    let path = state.join("container-profiles.json");
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(Error::io("cannot read", &path, &error)),
    };
    let invalid = |message| Error::unknown_state(format!("invalid {}: {message}", path.display()));
    let document: Document =
        serde_json::from_slice(&bytes).map_err(|error| invalid(error.to_string()))?;
    let mut profiles = BTreeMap::new();
    for profile in document.profiles {
        validate_name("container profile", &profile.name)
            .map_err(|error| invalid(error.to_string()))?;
        if profiles.insert(profile.name.clone(), profile.max).is_some() {
            return Err(invalid(format!(
                "profile {} is declared twice",
                profile.name
            )));
        }
    }
    Ok(profiles)
}
