//! Read-only measurements of registered volumes.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::contract::{RowSet, VOLUME_ROWS};
use crate::error::{Error, ErrorKind, Result};
use crate::os::{self, MountRoot, SpaceFacts, SpaceMeasurement, VolumeFacts, VolumeUuid};
use crate::registry;
use crate::state::{self, Deadline, LockRank, LockSet, StateLock, StatePaths, READ_DEADLINE};

/// Version of the read-only space report.
pub const SPACE_SCHEMA: &str = "barge.space/1";

/// Measurements at the time of the report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpaceReport {
    /// Time the report completed.
    pub observed_ms: u64,
    /// Registered volumes, bounded by the registry limit.
    pub volumes: RowSet<SpaceRow>,
}

/// A registered volume and its present measurement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpaceRow {
    /// Registered identity.
    pub volume_uuid: VolumeUuid,
    /// Verified mount root, when present.
    pub mount_point: Option<PathBuf>,
    /// Kernel measurement, absent only when the volume is absent.
    pub measurement: Option<SpaceMeasurement>,
    /// The declared policy, or none when undeclared.
    pub policy: Option<registry::SpacePolicy>,
    /// Whether the declared floor is below the default.
    #[serde(default)]
    pub floor_below_default: bool,
    /// Current state of the volume and its policy.
    pub state: String,
    /// Area accounting, unavailable until a later slice.
    pub areas: Option<RowSet<AreaSpace>>,
    /// Scratch accounting, unavailable until a later slice.
    pub scratch: Option<ScratchSpace>,
    /// Cache accounting, unavailable until a later slice.
    pub caches: Option<CacheSpace>,
    /// Attribution, unavailable until a later slice.
    pub attribution: Option<Attribution>,
}

/// Space attributed to one area.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AreaSpace {
    /// Owning namespace.
    pub namespace: String,
    /// Area name.
    pub area: String,
    /// Reserve participation.
    pub reserve: String,
    /// Owned bytes.
    pub bytes: u64,
    /// Idle roots.
    pub idle_roots: u64,
    /// Held roots.
    pub held_roots: u64,
    /// Roots with unknown activity.
    pub unknown_roots: u64,
}

/// Space attributed to scratch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScratchSpace {
    /// Owned bytes.
    pub bytes: u64,
    /// Idle eligible roots.
    pub idle_eligible_roots: u64,
    /// Held roots.
    pub held_roots: u64,
    /// Unconcluded roots.
    pub unconcluded_roots: u64,
}

/// Space attributed to caches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheSpace {
    /// Owned bytes.
    pub bytes: u64,
    /// Reserve participation.
    pub reserve: String,
}

/// Attribution of volume use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attribution {
    /// Owned bytes.
    pub owned_bytes: u64,
    /// Excluded bytes.
    pub excluded_bytes: u64,
    /// Held or unknown bytes.
    pub held_or_unknown_bytes: u64,
    /// Estimated remaining bytes.
    pub other_estimate_bytes: u64,
    /// Described cause.
    pub cause: String,
}

/// Read the registry and measure present volumes without creating state or locks.
pub fn report(
    paths: &StatePaths,
    facts: &dyn VolumeFacts,
    mounts: &dyn MountRoot,
    space: &dyn SpaceFacts,
) -> Result<SpaceReport> {
    let mut locks = LockSet::new(paths.clone());
    let _lock = locks.observe_registry()?;
    let registry = registry::read(paths)?;
    drop(_lock);
    let mut rows = Vec::new();
    for record in registry.volumes() {
        let _volume_lock = StateLock::shared_if_present(
            &paths.volume_lock_path(&record.volume_uuid),
            LockRank::Volume,
            Deadline::after(READ_DEADLINE),
        )?;
        let path = record.mount_hint.as_ref().ok_or_else(|| {
            Error::new(
                ErrorKind::UnknownState,
                "registered volume has no mount hint",
            )
        })?;
        let verified = match os::verify_volume(facts, mounts, path, &record.expected_volume()) {
            Ok(volume) => volume,
            Err(error) if error.kind() == ErrorKind::VolumeAbsent => {
                rows.push(SpaceRow {
                    volume_uuid: record.volume_uuid.clone(),
                    mount_point: None,
                    measurement: None,
                    policy: record.space.clone(),
                    floor_below_default: record
                        .space
                        .as_ref()
                        .is_some_and(|p| p.floor_bytes < registry::DEFAULT_FLOOR_BYTES),
                    state: "volume_absent".to_owned(),
                    areas: None,
                    scratch: None,
                    caches: None,
                    attribution: None,
                });
                continue;
            }
            Err(error) => return Err(error),
        };
        let measurement = space.measure_space(&verified)?;
        let state = match &record.space {
            None => "policy_undeclared",
            Some(_) if !record.durability.permits_pruning() => "durability_forbids",
            Some(policy) if measurement.free_bytes <= policy.floor_bytes => "below_floor",
            Some(policy) if measurement.free_bytes <= policy.reserve_bytes => "below_reserve",
            Some(_) => "above_reserve",
        };
        rows.push(SpaceRow {
            volume_uuid: record.volume_uuid.clone(),
            mount_point: Some(verified.identity().mount_point.clone()),
            measurement: Some(measurement),
            policy: record.space.clone(),
            floor_below_default: record
                .space
                .as_ref()
                .is_some_and(|p| p.floor_bytes < registry::DEFAULT_FLOOR_BYTES),
            state: state.to_owned(),
            areas: None,
            scratch: None,
            caches: None,
            attribution: None,
        });
    }
    let total = rows.len();
    Ok(SpaceReport {
        observed_ms: state::now_ms(),
        volumes: RowSet::bounded(rows, total, VOLUME_ROWS),
    })
}

/// Read a successful space envelope, refusing an unknown major or missing fields.
pub fn read_document(bytes: &[u8]) -> Result<SpaceReport> {
    let value = crate::contract::parse_document(bytes)?;
    let invalid = || Error::new(ErrorKind::UnknownState, "invalid barge.space/1 document");
    if value.get("schema").and_then(|v| v.as_str()) != Some(SPACE_SCHEMA)
        || value.get("command").and_then(|v| v.as_str()) != Some("space")
        || value.get("ok").and_then(|v| v.as_bool()) != Some(true)
    {
        return Err(invalid());
    }
    let data = value.get("data").ok_or_else(invalid)?;
    let rows = data
        .get("volumes")
        .and_then(|v| v.get("rows"))
        .and_then(|v| v.as_array())
        .ok_or_else(invalid)?;
    for row in rows {
        for field in [
            "volume_uuid",
            "mount_point",
            "measurement",
            "policy",
            "state",
            "areas",
            "scratch",
            "caches",
            "attribution",
        ] {
            if row.get(field).is_none() {
                return Err(invalid());
            }
        }
    }
    let report: SpaceReport = serde_json::from_value(data.clone()).map_err(|_| invalid())?;
    report.volumes.validate()?;
    for row in &report.volumes.rows {
        if let Some(areas) = &row.areas {
            areas.validate()?;
        }
        if !matches!(
            row.state.as_str(),
            "above_reserve"
                | "below_reserve"
                | "below_floor"
                | "policy_undeclared"
                | "volume_absent"
                | "space_unknown"
                | "durability_forbids"
        ) {
            return Err(invalid());
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::os::{DeviceId, VerifiedVolume};
    use crate::testkit::VolumeFixture;

    struct Injected(bool);
    impl SpaceFacts for Injected {
        fn measure_space(&self, volume: &VerifiedVolume) -> Result<SpaceMeasurement> {
            SpaceMeasurement::from_blocks(
                Some(4),
                self.0.then_some(3),
                Some(5),
                Some(10),
                12,
                volume.identity().device,
            )
        }
    }

    struct InjectedFlush(bool);
    impl crate::os::Flush for InjectedFlush {
        fn flush_volume(&self, _volume: &VerifiedVolume) -> Result<()> {
            if self.0 {
                Ok(())
            } else {
                Err(Error::new(
                    ErrorKind::UnknownState,
                    "injected flush failure",
                ))
            }
        }
    }

    #[test]
    fn flush_failure_is_exposed_through_the_trait() {
        let fixture = VolumeFixture::new("space-flush");
        let volume = os::verify_volume(
            &fixture.facts(),
            &fixture.mounts(),
            &fixture.root,
            &fixture.record.expected_volume(),
        )
        .expect("verified fixture");
        let flush: &dyn crate::os::Flush = &InjectedFlush(false);
        assert_eq!(
            flush.flush_volume(&volume).expect_err("failure").kind(),
            ErrorKind::UnknownState
        );
        let flush: &dyn crate::os::Flush = &InjectedFlush(true);
        assert!(flush.flush_volume(&volume).is_ok());
    }

    #[test]
    fn registered_volume_is_measured_with_undeclared_policy() {
        let fixture = VolumeFixture::new("space-report");
        fixture.register();
        let result = report(
            &fixture.state,
            &fixture.facts(),
            &fixture.mounts(),
            &Injected(true),
        )
        .expect("report");
        let row = &result.volumes.rows[0];
        assert_eq!(row.state, "policy_undeclared");
        assert_eq!(row.policy, None);
        let m = row.measurement.as_ref().expect("measurement");
        assert_eq!(m.free_bytes, 12);
        assert_eq!(m.purgeable, "unknown");
        assert_eq!(m.device, DeviceId::new(fixture.device.get()));
        let envelope = serde_json::json!({"schema": SPACE_SCHEMA, "command": "space", "ok": true, "data": result});
        let encoded = serde_json::to_vec(&envelope).expect("encode");
        read_document(&encoded).expect("valid report");
        for field in ["areas", "scratch", "caches", "attribution"] {
            assert_eq!(
                envelope["data"]["volumes"]["rows"][0][field],
                serde_json::Value::Null
            );
            let mut missing = envelope.clone();
            missing["data"]["volumes"]["rows"][0]
                .as_object_mut()
                .expect("row")
                .remove(field);
            assert!(read_document(&serde_json::to_vec(&missing).expect("encode")).is_err());
        }
        let mut populated = envelope.clone();
        let row = &mut populated["data"]["volumes"]["rows"][0];
        row["areas"] = serde_json::json!({"rows": [{"namespace":"oxide", "area":"pool", "reserve":"counts", "bytes":1, "idle_roots":2, "held_roots":3, "unknown_roots":4}], "count":1, "total":1, "truncated":false});
        row["scratch"] = serde_json::json!({"bytes":1, "idle_eligible_roots":2, "held_roots":3, "unconcluded_roots":4});
        row["caches"] = serde_json::json!({"bytes":1, "reserve":"excluded"});
        row["attribution"] = serde_json::json!({"owned_bytes":1, "excluded_bytes":2, "held_or_unknown_bytes":3, "other_estimate_bytes":4, "cause":"none"});
        read_document(&serde_json::to_vec(&populated).expect("encode")).expect("populated report");
        populated["data"]["volumes"]["rows"][0]["scratch"]["bytes"] = serde_json::json!("invalid");
        assert!(read_document(&serde_json::to_vec(&populated).expect("encode")).is_err());

        let mut broken = envelope;
        broken["data"]["volumes"]["rows"][0]
            .as_object_mut()
            .expect("row")
            .remove("policy");
        assert!(read_document(&serde_json::to_vec(&broken).expect("encode")).is_err());
        let error = report(
            &fixture.state,
            &fixture.facts(),
            &fixture.mounts(),
            &Injected(false),
        )
        .expect_err("unreadable");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
    }
}
