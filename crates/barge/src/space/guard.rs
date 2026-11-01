//! The untrusted checkpoint token and the guard's admission decisions.

use crate::area;
use crate::error::{Error, ErrorKind, Result};
use crate::os::VolumeUuid;
use serde::{Deserialize, Serialize};

/// A checkpoint value, carrying lookup keys and report-only observations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpaceGuard {
    /// Registered volume lookup key.
    pub volume_uuid: VolumeUuid,
    /// Area namespace.
    pub namespace: String,
    /// Area name.
    pub area: String,
    /// Last observed policy generation; not authority.
    pub generation: u64,
    /// Last observation time; not authority.
    pub observed_ms: u64,
}
impl SpaceGuard {
    /// Strictly parse a bounded checkpoint token.
    pub fn parse(token: &str) -> Result<Self> {
        let invalid = || Error::new(ErrorKind::Usage, "invalid space guard token");
        if token.len() > 1024 {
            return Err(invalid());
        }
        let mut parts = token.split(':');
        let uuid = parts.next().ok_or_else(invalid)?;
        if uuid.len() != 36 {
            return Err(invalid());
        }
        let volume_uuid = VolumeUuid::parse(uuid).map_err(|_| invalid())?;
        let (namespace, name) = parts
            .next()
            .ok_or_else(invalid)?
            .split_once('/')
            .ok_or_else(invalid)?;
        area::validate_name("namespace", namespace).map_err(|_| invalid())?;
        area::validate_name("area", name).map_err(|_| invalid())?;
        let number = |text: &str| {
            if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(invalid());
            }
            text.parse::<u64>().map_err(|_| invalid())
        };
        let generation = number(parts.next().ok_or_else(invalid)?)?;
        let observed_ms = number(parts.next().ok_or_else(invalid)?)?;
        if parts.next().is_some() {
            return Err(invalid());
        }
        Ok(Self {
            volume_uuid,
            namespace: namespace.to_owned(),
            area: name.to_owned(),
            generation,
            observed_ms,
        })
    }
    /// Format this checkpoint as a token.
    pub fn token(&self) -> String {
        format!(
            "{}:{}/{}:{}:{}",
            self.volume_uuid, self.namespace, self.area, self.generation, self.observed_ms
        )
    }
}

/// A space decision cannot name an internal fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpaceDecision {
    /// Admit above the reserve and floor.
    Admit,
    /// Admit above the floor with a reserve warning.
    AdmitBelowReserve,
    /// A running consumer may continue.
    Continue,
    /// Deletion completed but the floor is breached.
    StopBelowFloor,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn checkpoint_token_round_trips_and_rejects_malformed_values() {
        let token = "A6AC3ED7-0A5B-4E6D-8BA0-813B8DD4C99A:oxide/pool:7:123";
        let guard = SpaceGuard::parse(token).unwrap();
        assert_eq!(SpaceGuard::parse(&guard.token()).unwrap(), guard);
        for invalid in [
            "",
            "x:oxide/pool:1:2",
            "A6AC3ED7-0A5B-4E6D-8BA0-813B8DD4C99A:oxide/pool:1:2:3",
            "A6AC3ED7-0A5B-4E6D-8BA0-813B8DD4C99A:oxide/pool/extra:1:2",
            "A6AC3ED7-0A5B-4E6D-8BA0-813B8DD4C99A:oxide/pool:+1:2",
        ] {
            assert_eq!(
                SpaceGuard::parse(invalid).unwrap_err().kind(),
                ErrorKind::Usage
            );
        }
    }
}
