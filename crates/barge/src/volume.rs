//! Explicit volume registration plans and atomic publication.

use crate::digest::sha256_hex;
use crate::error::{Error, ErrorKind, Result};
use crate::os::{self, MountRoot, SpaceFacts, VerifiedVolume, VolumeFacts, VolumeUuid};
use crate::registry::{
    self, Availability, Durability, FolderOverrides, RecordOrigin, Registry, RegistryDocument,
    RegistryWriter, ScratchGrants, SpacePolicy, VolumeRecord, DEFAULT_FLOOR_BYTES,
    REGISTRY_VOLUME_ROWS,
};
use crate::state::{self, LockSet, StatePaths};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Maximum bytes in a registration plan or registry publication.
pub const REGISTRATION_SIZE_CAP_BYTES: usize = 64 * 1024;

/// A registration operation and the exact record it proposes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    /// Register a new record or update an existing one.
    pub operation: Operation,
    /// Hash of the registry's logical contents before the change.
    pub registry_sha256: String,
    /// Generation of the existing record, or zero for a new registration.
    pub previous_generation: u64,
    /// The exact proposed volume record.
    pub record: VolumeRecord,
}

/// Which registration operation a plan authorizes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    Register,
    Update,
}

/// A plan with its exact encoded bytes and digest.
#[derive(Debug, Clone, Serialize)]
pub struct Preview {
    /// JSON bytes to save and pass to apply.
    pub plan: String,
    /// SHA-256 of the UTF-8 plan bytes.
    pub plan_sha256: String,
}

/// Explicit fields accepted for a new registration.
#[derive(Debug, Clone)]
pub struct RegisterRequest {
    /// Mount root to verify.
    pub mount: PathBuf,
    /// Explicit availability.
    pub availability: Availability,
    /// Explicit durability.
    pub durability: Durability,
    /// Folder relative to the volume.
    pub root_dir: String,
    /// Reserve threshold.
    pub reserve_bytes: u64,
    /// Explicit floor, or the documented default.
    pub floor_bytes: Option<u64>,
    /// Person making the declaration.
    pub declared_by: String,
}

/// Fields to change on an existing record.
#[derive(Debug, Clone)]
pub struct UpdateRequest {
    /// Existing identity.
    pub volume_uuid: VolumeUuid,
    /// Replacement reserve, or retain the existing value.
    pub reserve_bytes: Option<u64>,
    /// Replacement floor, or retain the existing value.
    pub floor_bytes: Option<u64>,
    /// Optional explicit availability.
    pub availability: Option<Availability>,
    /// Optional explicit durability.
    pub durability: Option<Durability>,
    /// Optional root directory.
    pub root_dir: Option<String>,
    /// Person making the declaration.
    pub declared_by: String,
}

fn registry_hash(registry: &Registry) -> Result<String> {
    let bytes = serde_json::to_vec(&registry.volumes())
        .map_err(|e| Error::new(ErrorKind::Io, e.to_string()))?;
    Ok(sha256_hex(&bytes))
}
fn observed(paths: &StatePaths) -> Result<Registry> {
    let mut locks = LockSet::new(paths.clone());
    let _lock = locks.observe_registry()?;
    registry::read(paths)
}
fn verify(
    record: &VolumeRecord,
    facts: &dyn VolumeFacts,
    mounts: &dyn MountRoot,
    space: &dyn SpaceFacts,
) -> Result<VerifiedVolume> {
    let mount = record
        .mount_hint
        .as_ref()
        .ok_or_else(|| Error::new(ErrorKind::UnknownState, "volume has no mount hint"))?;
    let verified = os::verify_volume(facts, mounts, mount, &record.expected_volume())?;
    if verified.identity().mount_point != *mount {
        return Err(Error::new(
            ErrorKind::VolumeMismatch,
            "registration requires the mount root",
        ));
    }
    let measured = space.measure_space(&verified)?;
    let policy = record.declared_space()?;
    if policy.reserve_bytes > measured.total_bytes || policy.floor_bytes > measured.total_bytes {
        return Err(Error::new(
            ErrorKind::Usage,
            "space thresholds exceed the volume's total bytes",
        ));
    }
    verified.reverify(facts)?;
    Ok(verified)
}
fn preview(plan: Plan) -> Result<Preview> {
    let plan = serde_json::to_string_pretty(&plan)
        .map_err(|e| Error::new(ErrorKind::Io, e.to_string()))?;
    if plan.len() > REGISTRATION_SIZE_CAP_BYTES {
        return Err(
            Error::new(ErrorKind::ResourceBound, "registration plan exceeds 64 KiB")
                .with_detail("bytes", plan.len().into())
                .with_detail("cap", REGISTRATION_SIZE_CAP_BYTES.into()),
        );
    }
    Ok(Preview {
        plan_sha256: sha256_hex(plan.as_bytes()),
        plan,
    })
}
fn validate_plan(plan: &Plan, registry: &Registry) -> Result<RegistryDocument> {
    if registry_hash(registry)? != plan.registry_sha256 {
        return Err(Error::new(
            ErrorKind::UnknownState,
            "registry changed since the plan",
        ));
    }
    let current = registry.volume(&plan.record.volume_uuid);
    if plan.record.durability == Durability::Undeclared {
        return Err(Error::new(
            ErrorKind::Usage,
            "registration requires a declared durability",
        ));
    }
    if let Some(old) = current {
        let mut proposed = plan.record.clone();
        proposed.generation = old.generation;
        if proposed == *old {
            return Err(Error::new(
                ErrorKind::Usage,
                "volume update makes no change",
            ));
        }
    }
    match (plan.operation, current) {
        (Operation::Register, None)
            if plan.previous_generation == 0 && plan.record.generation == 1 => {}
        (Operation::Update, Some(old))
            if old.generation == plan.previous_generation
                && old.generation.checked_add(1) == Some(plan.record.generation) => {}
        _ => {
            return Err(Error::new(
                ErrorKind::UnknownState,
                "registration plan has a stale generation or operation",
            ))
        }
    }
    let mut volumes = registry.volumes().to_vec();
    volumes.retain(|row| row.volume_uuid != plan.record.volume_uuid);
    volumes.push(plan.record.clone());
    let document = RegistryDocument::new(volumes);
    document.validate(REGISTRY_VOLUME_ROWS)?;
    Ok(document)
}

/// Plan a registration without creating state.
pub fn register_plan(
    paths: &StatePaths,
    request: &RegisterRequest,
    facts: &dyn VolumeFacts,
    mounts: &dyn MountRoot,
    space: &dyn SpaceFacts,
) -> Result<Preview> {
    let identity = facts.identify(&request.mount)?;
    let uuid = identity
        .volume_uuid
        .ok_or_else(|| Error::new(ErrorKind::UnknownState, "volume UUID is unreadable"))?;
    let now = state::now_ms();
    let record = VolumeRecord {
        volume_uuid: uuid,
        mount_hint: Some(request.mount.clone()),
        availability: request.availability,
        durability: request.durability,
        root_dir: request.root_dir.clone(),
        folders: FolderOverrides::none(),
        origin: RecordOrigin::Declared,
        scratch_grants: ScratchGrants::none(),
        space: Some(SpacePolicy {
            reserve_bytes: request.reserve_bytes,
            floor_bytes: request.floor_bytes.unwrap_or(DEFAULT_FLOOR_BYTES),
            declared_by: request.declared_by.clone(),
            declared_ms: now,
        }),
        generation: 1,
        registered_ms: now,
    };
    let current = observed(paths)?;
    let plan = Plan {
        operation: Operation::Register,
        registry_sha256: registry_hash(&current)?,
        previous_generation: 0,
        record,
    };
    validate_plan(&plan, &current)?;
    verify(&plan.record, facts, mounts, space)?;
    preview(plan)
}

/// Plan an update without creating state.
pub fn update_plan(
    paths: &StatePaths,
    request: &UpdateRequest,
    facts: &dyn VolumeFacts,
    mounts: &dyn MountRoot,
    space: &dyn SpaceFacts,
) -> Result<Preview> {
    let current = observed(paths)?;
    let old = current
        .volume(&request.volume_uuid)
        .ok_or_else(|| Error::new(ErrorKind::Usage, "volume is not registered"))?;
    let mut record = old.clone();
    let reserve = match request.reserve_bytes {
        Some(value) => value,
        None => old.declared_space()?.reserve_bytes,
    };
    let floor = match request.floor_bytes {
        Some(value) => value,
        None => old.declared_space()?.floor_bytes,
    };
    record.space = Some(SpacePolicy {
        reserve_bytes: reserve,
        floor_bytes: floor,
        declared_by: request.declared_by.clone(),
        declared_ms: state::now_ms(),
    });
    if let Some(value) = request.availability {
        record.availability = value;
    }
    if let Some(value) = request.durability {
        record.durability = value;
    }
    if let Some(value) = &request.root_dir {
        record.root_dir.clone_from(value);
    }
    record.origin = RecordOrigin::Declared;
    record.generation = old
        .generation
        .checked_add(1)
        .ok_or_else(|| Error::new(ErrorKind::UnknownState, "volume generation exhausted"))?;
    let plan = Plan {
        operation: Operation::Update,
        registry_sha256: registry_hash(&current)?,
        previous_generation: old.generation,
        record,
    };
    validate_plan(&plan, &current)?;
    verify(&plan.record, facts, mounts, space)?;
    preview(plan)
}

/// Publish the exact digest-bound plan, rechecking the registry and volume.
pub fn apply(
    paths: &StatePaths,
    bytes: &[u8],
    digest: &str,
    facts: &dyn VolumeFacts,
    mounts: &dyn MountRoot,
    space: &dyn SpaceFacts,
) -> Result<VolumeRecord> {
    if bytes.len() > REGISTRATION_SIZE_CAP_BYTES {
        return Err(
            Error::new(ErrorKind::ResourceBound, "registration plan exceeds 64 KiB")
                .with_detail("bytes", bytes.len().into())
                .with_detail("cap", REGISTRATION_SIZE_CAP_BYTES.into()),
        );
    }
    if sha256_hex(bytes) != digest {
        return Err(Error::new(ErrorKind::Usage, "plan digest does not match"));
    }
    let plan: Plan = serde_json::from_slice(bytes)
        .map_err(|e| Error::new(ErrorKind::Usage, format!("invalid volume plan: {e}")))?;
    let current = observed(paths)?;
    validate_plan(&plan, &current)?;
    let verified = verify(&plan.record, facts, mounts, space)?;
    paths.initialize()?;
    let mut locks = LockSet::new(paths.clone());
    let lock = locks.lock_registry()?;
    let _volume_lock = locks.lock_volume(&plan.record.volume_uuid)?;
    let current = registry::read(paths)?;
    let document = validate_plan(&plan, &current)?;
    verified.reverify(facts)?;
    let written = RegistryWriter::new(paths).publish_with_cap(
        &lock,
        &document,
        REGISTRATION_SIZE_CAP_BYTES,
    )?;
    written
        .volume(&plan.record.volume_uuid)
        .cloned()
        .ok_or_else(|| Error::new(ErrorKind::UnknownState, "published record is missing"))
}

/// Read one registered volume without creating state.
pub fn describe(paths: &StatePaths, uuid: &VolumeUuid) -> Result<VolumeRecord> {
    observed(paths)?
        .volume(uuid)
        .cloned()
        .ok_or_else(|| Error::new(ErrorKind::Usage, "volume is not registered"))
}

/// Parse bytes or an integer with a binary unit.
pub fn parse_bytes(value: &str) -> Result<u64> {
    let (digits, factor) = [
        ("KiB", 1024_u64),
        ("MiB", 1024_u64.pow(2)),
        ("GiB", 1024_u64.pow(3)),
        ("TiB", 1024_u64.pow(4)),
    ]
    .into_iter()
    .find_map(|(suffix, factor)| value.strip_suffix(suffix).map(|digits| (digits, factor)))
    .unwrap_or((value, 1));
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(Error::new(
            ErrorKind::Usage,
            "space size must be an integer byte count or binary unit",
        ));
    }
    digits
        .parse::<u64>()
        .ok()
        .and_then(|v| v.checked_mul(factor))
        .ok_or_else(|| Error::new(ErrorKind::Usage, "space size is out of range"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::os::{SpaceMeasurement, VerifiedVolume};
    use crate::testkit::VolumeFixture;

    struct Measurement;
    impl SpaceFacts for Measurement {
        fn measure_space(&self, volume: &VerifiedVolume) -> Result<SpaceMeasurement> {
            SpaceMeasurement::from_blocks(
                Some(1),
                Some(80),
                Some(80),
                Some(100),
                1,
                volume.identity().device,
            )
        }
    }
    #[test]
    fn register_update_and_stale_plans() {
        let fixture = VolumeFixture::new("volume-plan");
        let facts = fixture.facts();
        let mounts = fixture.mounts();
        let request = RegisterRequest {
            mount: fixture.root.clone(),
            availability: Availability::Permanent,
            durability: Durability::Expendable,
            root_dir: String::new(),
            reserve_bytes: 20,
            floor_bytes: Some(30),
            declared_by: "operator".into(),
        };
        let preview =
            register_plan(&fixture.state, &request, &facts, &mounts, &Measurement).expect("plan");
        assert!(!fixture.state.registry_path().exists());
        assert!(apply(
            &fixture.state,
            preview.plan.as_bytes(),
            &"0".repeat(64),
            &facts,
            &mounts,
            &Measurement
        )
        .is_err());
        assert!(!fixture.state.registry_path().exists());
        let record = apply(
            &fixture.state,
            preview.plan.as_bytes(),
            &preview.plan_sha256,
            &facts,
            &mounts,
            &Measurement,
        )
        .expect("register");
        assert_eq!(record.generation, 1);
        assert_eq!(
            describe(&fixture.state, &fixture.uuid).expect("describe"),
            record
        );
        let update = UpdateRequest {
            volume_uuid: fixture.uuid.clone(),
            reserve_bytes: Some(40),
            floor_bytes: None,
            availability: None,
            durability: None,
            root_dir: None,
            declared_by: "operator".into(),
        };
        let plan = update_plan(&fixture.state, &update, &facts, &mounts, &Measurement)
            .expect("update plan");
        let stale = plan.clone();
        let result = apply(
            &fixture.state,
            plan.plan.as_bytes(),
            &plan.plan_sha256,
            &facts,
            &mounts,
            &Measurement,
        )
        .expect("update");
        assert_eq!(result.generation, 2);
        assert_eq!(result.declared_space().expect("space").floor_bytes, 30);
        let before = std::fs::read(fixture.state.registry_path()).expect("registry bytes");
        assert!(apply(
            &fixture.state,
            stale.plan.as_bytes(),
            &stale.plan_sha256,
            &facts,
            &mounts,
            &Measurement
        )
        .is_err());
        assert_eq!(
            std::fs::read(fixture.state.registry_path()).expect("registry bytes"),
            before
        );
    }
    #[test]
    fn oversized_plan_and_registry_write_refuse_without_changing_state() {
        let fixture = VolumeFixture::new("volume-bounds");
        fixture.register();
        let before = std::fs::read(fixture.state.registry_path()).expect("registry");
        let oversized = vec![b' '; REGISTRATION_SIZE_CAP_BYTES + 1];
        let digest = sha256_hex(&oversized);
        assert_eq!(
            apply(
                &fixture.state,
                &oversized,
                &digest,
                &fixture.facts(),
                &fixture.mounts(),
                &Measurement
            )
            .expect_err("plan cap")
            .kind(),
            ErrorKind::ResourceBound
        );
        assert_eq!(
            std::fs::read(fixture.state.registry_path()).expect("registry"),
            before
        );
        let mut other = fixture.record.clone();
        other.volume_uuid =
            VolumeUuid::parse("A1B2C3D4-E5F6-4A7B-8C9D-0E1F2A3B4C5D").expect("uuid");
        other.root_dir = "x".repeat(REGISTRATION_SIZE_CAP_BYTES);
        let document = RegistryDocument::new(vec![fixture.record.clone(), other]);
        let mut locks = LockSet::new(fixture.state.clone());
        let lock = locks.lock_registry().expect("lock");
        RegistryWriter::new(&fixture.state)
            .publish(&lock, &document)
            .expect("fixture registry");
        drop(lock);
        let before = std::fs::read(fixture.state.registry_path()).expect("registry");
        let update = UpdateRequest {
            volume_uuid: fixture.uuid.clone(),
            reserve_bytes: Some(20),
            floor_bytes: Some(10),
            availability: None,
            durability: None,
            root_dir: None,
            declared_by: "operator".into(),
        };
        let plan = update_plan(
            &fixture.state,
            &update,
            &fixture.facts(),
            &fixture.mounts(),
            &Measurement,
        )
        .expect("plan");
        assert_eq!(
            apply(
                &fixture.state,
                plan.plan.as_bytes(),
                &plan.plan_sha256,
                &fixture.facts(),
                &fixture.mounts(),
                &Measurement
            )
            .expect_err("write cap")
            .kind(),
            ErrorKind::ResourceBound
        );
        assert_eq!(
            std::fs::read(fixture.state.registry_path()).expect("registry"),
            before
        );
    }

    #[test]
    fn registration_refuses_when_the_volume_lock_is_held() {
        let fixture = VolumeFixture::new("volume-held");
        fixture.register();
        let update = UpdateRequest {
            volume_uuid: fixture.uuid.clone(),
            reserve_bytes: Some(20),
            floor_bytes: Some(10),
            availability: None,
            durability: None,
            root_dir: None,
            declared_by: "operator".into(),
        };
        let plan = update_plan(
            &fixture.state,
            &update,
            &fixture.facts(),
            &fixture.mounts(),
            &Measurement,
        )
        .expect("plan");
        let before = std::fs::read(fixture.state.registry_path()).expect("registry");
        let mut locks = LockSet::new(fixture.state.clone());
        let _held = locks.lock_volume(&fixture.uuid).expect("hold volume lock");
        let error = apply(
            &fixture.state,
            plan.plan.as_bytes(),
            &plan.plan_sha256,
            &fixture.facts(),
            &fixture.mounts(),
            &Measurement,
        )
        .expect_err("locked volume refuses publication");
        assert_eq!(error.kind(), ErrorKind::Locked);
        assert_eq!(
            std::fs::read(fixture.state.registry_path()).expect("registry"),
            before
        );
    }

    #[test]
    fn update_cannot_infer_a_missing_floor() {
        let fixture = VolumeFixture::new("volume-undeclared");
        fixture.register();
        let request = UpdateRequest {
            volume_uuid: fixture.uuid.clone(),
            reserve_bytes: Some(20),
            floor_bytes: None,
            availability: None,
            durability: None,
            root_dir: None,
            declared_by: "operator".into(),
        };
        assert_eq!(
            update_plan(
                &fixture.state,
                &request,
                &fixture.facts(),
                &fixture.mounts(),
                &Measurement
            )
            .expect_err("floor is undeclared")
            .kind(),
            ErrorKind::UnknownState
        );
    }

    #[test]
    fn invalid_thresholds_and_size_parsing_refuse() {
        let fixture = VolumeFixture::new("volume-invalid");
        let mut request = RegisterRequest {
            mount: fixture.root.clone(),
            availability: Availability::Permanent,
            durability: Durability::Expendable,
            root_dir: String::new(),
            reserve_bytes: 0,
            floor_bytes: Some(1),
            declared_by: "operator".into(),
        };
        assert!(register_plan(
            &fixture.state,
            &request,
            &fixture.facts(),
            &fixture.mounts(),
            &Measurement
        )
        .is_err());
        request.reserve_bytes = 101;
        assert!(register_plan(
            &fixture.state,
            &request,
            &fixture.facts(),
            &fixture.mounts(),
            &Measurement
        )
        .is_err());
        assert!(!fixture.state.registry_path().exists());
        assert_eq!(parse_bytes("2GiB").expect("size"), 2 * 1024 * 1024 * 1024);
        for bad in ["1.5GiB", "-1", "18446744073709551615TiB"] {
            assert!(parse_bytes(bad).is_err());
        }
    }
}
