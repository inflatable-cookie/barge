//! Read-only parsers for someone else's state, and the `Observed<T>` values they
//! return (stage 1 design, "Reading Oxide and Effigy state in place" and
//! "Read-only inspection versus authoritative writes").
//!
//! Every value here comes from bytes another tool wrote. It is parsed strictly
//! against that tool's own field set, carries the `source` and `version` it was
//! read at, and is reported. It is never authority: the [`Observed`] wrapper and
//! [`ObservedVolume`] have no conversion into [`crate::registry::VolumeRecord`],
//! so an imported registration cannot be written as a registration by accident.
//! An unreadable, oversized, unknown-version or schema-divergent file becomes an
//! unknown-state observation naming the file, never a default, and an absent
//! file is reported as absent rather than as an empty record.
//!
//! Slice S2 reads Oxide's `removable.json` (v1 and v2) and `pools.json`, and
//! owns the one candidate-pool list every Oxide lookup shares. Root markers,
//! receipts and scratch markers belong to slices S3 to S5; the output-registry
//! census that reads its rows read-only belongs to slice F5 and lives in
//! [`crate::census`].

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, ErrorKind, Result};
use crate::os::{AccountFacts, VolumeUuid};
use crate::registry::{Availability, Durability, RecordOrigin};
use crate::state::{self, Bounds, Deadline, ReadOutcome, READ_DEADLINE};

/// The observation source for Oxide's removable-volume registration.
pub const OXIDE_REGISTRATION_SOURCE: &str = "oxide.removable";
/// The observation source for Oxide's informational pools record.
pub const OXIDE_POOLS_SOURCE: &str = "oxide.pools";
/// The registration's file name inside a pool's control directory.
pub const OXIDE_REGISTRATION_FILE: &str = "removable.json";
/// Oxide's control directory inside a pool.
pub const OXIDE_CONTROL_DIR: &str = "oxide-managed";
/// Oxide's informational pools record.
pub const OXIDE_POOLS_FILE: &str = "pools.json";
/// The tool whose state Barge reads in place.
pub const OXIDE_TOOL: &str = "oxide";
/// Oxide's current default internal pool, inside its cache directory.
pub const OXIDE_DEFAULT_POOL_DIR: &str = "build-pool";
/// Oxide's retired default internal pool, inside its cache directory.
pub const OXIDE_LEGACY_POOL_DIR: &str = "pool";

/// Oxide's own registration size cap, which Barge keeps rather than inventing a
/// larger one.
pub const OXIDE_REGISTRATION_SIZE_CAP_BYTES: usize = 16 * 1024;
/// Oxide's own pools-record size cap.
pub const OXIDE_POOLS_SIZE_CAP_BYTES: usize = 64 * 1024;
/// Oxide's own pools row cap.
pub const OXIDE_POOL_ROWS: usize = 16;

/// A foreign value, with the source and version it was read at.
///
/// The wrapper is the type-level separation between observation and authority:
/// nothing converts an `Observed<T>` into a record Barge may write, and the
/// foreign types inside it carry no Barge record fields, so a write needs a
/// separate, explicit declaration in a later slice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observed<T> {
    source: &'static str,
    version: String,
    path: PathBuf,
    value: T,
}

impl<T> Observed<T> {
    pub(crate) fn new(source: &'static str, version: String, path: PathBuf, value: T) -> Self {
        Self {
            source,
            version,
            path,
            value,
        }
    }

    /// The source family, for example `oxide.removable`.
    pub fn source(&self) -> &'static str {
        self.source
    }

    /// The version the source states, as its own text.
    pub fn version(&self) -> &str {
        &self.version
    }

    /// The file that was read.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The parsed value.
    pub fn value(&self) -> &T {
        &self.value
    }
}

/// How one observation came out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationState {
    /// Read and valid for its stated version.
    Reported,
    /// The file is not there. Absence is a fact, not an unknown.
    Absent,
    /// Unreadable, oversized, unknown-version or schema-divergent; never a
    /// default.
    UnknownState,
}

/// One foreign observation: what exists, what was read, and what could not be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Observation<T> {
    /// The file was read and validated.
    Reported(Observed<T>),
    /// The file is not there.
    Absent {
        /// The source family.
        source: &'static str,
        /// The path that was looked at.
        path: PathBuf,
        /// What was looked for and where, for a human.
        detail: String,
    },
    /// The file is there but could not be read as what it claims to be.
    Unknown {
        /// The source family.
        source: &'static str,
        /// The file that could not be read.
        path: PathBuf,
        /// Why, naming the file and the divergence.
        detail: String,
    },
}

impl<T> Observation<T> {
    /// The source family.
    pub fn source(&self) -> &'static str {
        match self {
            Self::Reported(observed) => observed.source(),
            Self::Absent { source, .. } | Self::Unknown { source, .. } => source,
        }
    }

    /// The path that was read or looked at.
    pub fn path(&self) -> &Path {
        match self {
            Self::Reported(observed) => observed.path(),
            Self::Absent { path, .. } | Self::Unknown { path, .. } => path,
        }
    }

    /// How the observation came out.
    pub fn state(&self) -> ObservationState {
        match self {
            Self::Reported(_) => ObservationState::Reported,
            Self::Absent { .. } => ObservationState::Absent,
            Self::Unknown { .. } => ObservationState::UnknownState,
        }
    }

    /// Why the observation is absent or unknown.
    pub fn detail(&self) -> Option<&str> {
        match self {
            Self::Reported(_) => None,
            Self::Absent { detail, .. } | Self::Unknown { detail, .. } => Some(detail),
        }
    }

    /// The observed value, when it was read.
    pub fn observed(&self) -> Option<&Observed<T>> {
        match self {
            Self::Reported(observed) => Some(observed),
            Self::Absent { .. } | Self::Unknown { .. } => None,
        }
    }

    /// The observation as one row of a document.
    pub fn row(&self) -> ObservationRow {
        ObservationRow {
            source: self.source().to_owned(),
            version: self
                .observed()
                .map_or_else(String::new, |observed| observed.version().to_owned()),
            path: self.path().to_path_buf(),
            state: self.state(),
            detail: self.detail().map(str::to_owned),
        }
    }
}

/// One foreign observation, as `describe` and `inspect` report it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservationRow {
    /// The source family, for example `oxide.removable`.
    pub source: String,
    /// The version the source states, or empty when it states none: an absent or
    /// unreadable file, or a format that carries no version field (Oxide's pools
    /// record is a bare array). A reported file's version is never defaulted, so
    /// a reader that needs one checks `state` first.
    pub version: String,
    /// The file that was read or looked at.
    pub path: PathBuf,
    /// Whether the observation was reported, absent or unknown.
    pub state: ObservationState,
    /// Why the observation is absent or unknown, when it is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Oxide's removable-volume registration, as Oxide writes it (`Registration` v1
/// and v2, `deny_unknown_fields`).
///
/// `mode` is availability: version 2 states it, and version 1 does not, so a v1
/// record is `ephemeral` exactly as Oxide reads it. Oxide's `reserve_bytes` and
/// `cap_bytes` are reported and never used as Barge's own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OxideRegistration {
    /// The registration version that was read.
    pub version: u32,
    /// The volume's mount point at registration.
    pub mount_point: PathBuf,
    /// The volume's APFS UUID.
    pub volume_uuid: VolumeUuid,
    /// The container's APFS UUID.
    pub container_uuid: VolumeUuid,
    /// Oxide's selection reserve.
    pub reserve_bytes: u64,
    /// Oxide's admission budget.
    pub cap_bytes: u64,
    /// The volume's APFS quota at registration, when it had one.
    pub quota_bytes: Option<u64>,
    /// The volume's physical capacity at registration.
    pub capacity_bytes: Option<u64>,
    /// When Oxide registered the volume.
    pub registered_ms: u64,
    /// Availability, derived from Oxide's `mode`.
    pub mode: Availability,
}

impl OxideRegistration {
    /// The volume this registration describes, as an observation.
    ///
    /// The mapping is the design's: `origin` is `observed_oxide`, availability
    /// comes from Oxide's `mode`, and durability is `undeclared` because an
    /// Oxide registration says nothing about it.
    pub fn observed_volume(&self) -> ObservedVolume {
        ObservedVolume {
            volume_uuid: self.volume_uuid.clone(),
            mount_hint: Some(self.mount_point.clone()),
            availability: self.mode,
            durability: Durability::Undeclared,
            origin: RecordOrigin::ObservedOxide,
        }
    }
}

/// A volume as a foreign registration describes it.
///
/// It is deliberately not a [`crate::registry::VolumeRecord`]: it has no
/// `scratch_grants`, no `generation` and no `registered_ms` of Barge's own, and
/// there is no conversion from this type to that one. Turning an observation
/// into a registration is an explicit operator declaration in a later slice, so
/// nothing read here can be written as authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservedVolume {
    /// The identity that matters.
    pub volume_uuid: VolumeUuid,
    /// The mount point the foreign record observed; used to find it, never as
    /// identity.
    pub mount_hint: Option<PathBuf>,
    /// Availability, from the foreign record's own mode.
    pub availability: Availability,
    /// Always `undeclared`: the foreign record does not say.
    pub durability: Durability,
    /// Always `observed_oxide`.
    pub origin: RecordOrigin,
}

/// Oxide's informational pools record: a bounded array of pool paths.
///
/// Oxide's own comment says it is never ownership or reclaim proof, so Barge
/// reports it and never treats a pool path as a volume identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OxidePools {
    paths: Vec<PathBuf>,
}

impl OxidePools {
    /// The pool paths the record names, in file order.
    pub fn paths(&self) -> &[PathBuf] {
        &self.paths
    }

    /// Whether the record names no pools.
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }
}

/// Oxide's observations for this account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OxideObservations {
    /// Oxide's pools record.
    pub pools: Observation<OxidePools>,
    /// Oxide's removable-volume registration.
    pub registration: Observation<OxideRegistration>,
    /// Every internal pool directory that was looked at, in order. A pool is
    /// where Oxide keeps `oxide-managed/removable.json` and `outputs/`.
    pub internal_pools: Vec<PathBuf>,
    /// Every registration path that was looked at, in order.
    pub searched: Vec<PathBuf>,
}

impl OxideObservations {
    /// The observation rows both documents report.
    pub fn rows(&self) -> [ObservationRow; 2] {
        [self.registration.row(), self.pools.row()]
    }
}

/// Read Oxide's observations for this account.
///
/// Oxide's state directory and cache directory are the platform's, so they come
/// from the `os` layer; the leaf names are Oxide's own and live here.
pub fn observe_oxide(account: &dyn AccountFacts) -> Result<OxideObservations> {
    let state_dir = account.tool_state_dir(OXIDE_TOOL)?;
    let cache_dir = account.tool_cache_dir(OXIDE_TOOL)?;
    Ok(observe_oxide_at(&state_dir, &cache_dir))
}

/// Read Oxide's observations from explicit directories.
///
/// The registration is looked for where Oxide's own pools record says its pools
/// are, then at Oxide's current and retired default pools. The first candidate
/// that is present decides: a registration that is present but unreadable is
/// unknown state, and Barge does not look past it for a more convenient one.
pub fn observe_oxide_at(state_dir: &Path, cache_dir: &Path) -> OxideObservations {
    let pools = read_pools(&state_dir.join(OXIDE_POOLS_FILE));
    let internal_pools = pool_candidates(&pools, cache_dir);
    let candidates: Vec<PathBuf> = internal_pools
        .iter()
        .map(|pool| registration_path(pool))
        .collect();

    let registration = resolve_registration(&candidates);
    OxideObservations {
        pools,
        registration,
        internal_pools,
        searched: candidates,
    }
}

/// Every internal pool directory to look in, in order: the pools Oxide's own
/// record names, then its current and retired default pools.
///
/// A pool only appears once. Nothing is created or changed: this is the shared
/// candidate list the registration and the output registry are both looked for
/// in, so two readers of the same tool cannot disagree about where it lives.
pub(crate) fn pool_candidates(pools: &Observation<OxidePools>, cache_dir: &Path) -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Observation::Reported(observed) = pools {
        for pool in observed.value().paths() {
            candidates.push(pool.clone());
        }
    }
    candidates.push(cache_dir.join(OXIDE_DEFAULT_POOL_DIR));
    candidates.push(cache_dir.join(OXIDE_LEGACY_POOL_DIR));
    candidates.dedup();
    candidates
}

/// Where a registration lives inside one internal pool.
fn registration_path(pool: &Path) -> PathBuf {
    pool.join(OXIDE_CONTROL_DIR).join(OXIDE_REGISTRATION_FILE)
}

fn resolve_registration(candidates: &[PathBuf]) -> Observation<OxideRegistration> {
    for path in candidates {
        match read_registration(path) {
            Observation::Absent { .. } => continue,
            found => return found,
        }
    }
    let path = candidates
        .first()
        .cloned()
        .unwrap_or_else(|| PathBuf::from(OXIDE_CONTROL_DIR).join(OXIDE_REGISTRATION_FILE));
    Observation::Absent {
        source: OXIDE_REGISTRATION_SOURCE,
        detail: format!(
            "no Oxide registration is present in any of the {} known internal pool{}",
            candidates.len(),
            if candidates.len() == 1 { "" } else { "s" }
        ),
        path,
    }
}

/// Read Oxide's registration strictly. A file that is absent, oversized,
/// unreadable, an unknown version, or not exactly the field set that version
/// defines is reported as exactly that, naming the file.
pub fn read_registration(path: &Path) -> Observation<OxideRegistration> {
    let bounds = Bounds::new(
        OXIDE_REGISTRATION_SIZE_CAP_BYTES,
        1,
        Deadline::after(READ_DEADLINE),
    );
    match state::read_bounded(path, &bounds) {
        ReadOutcome::Absent => Observation::Absent {
            source: OXIDE_REGISTRATION_SOURCE,
            path: path.to_path_buf(),
            detail: format!("no registration file at {}", path.display()),
        },
        ReadOutcome::Refused(error) => observation_error(OXIDE_REGISTRATION_SOURCE, path, &error),
        ReadOutcome::Present(bytes) => match parse_registration(path, &bytes) {
            Ok(observed) => Observation::Reported(observed),
            Err(error) => observation_error(OXIDE_REGISTRATION_SOURCE, path, &error),
        },
    }
}

fn observation_error<T>(source: &'static str, path: &Path, error: &Error) -> Observation<T> {
    Observation::Unknown {
        source,
        path: path.to_path_buf(),
        detail: format!("{}: {}", path.display(), error.message()),
    }
}

/// Oxide's registration v1: the field set before `mode` existed.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RegistrationV1 {
    version: u32,
    mount_point: PathBuf,
    volume_uuid: String,
    container_uuid: String,
    reserve_bytes: u64,
    cap_bytes: u64,
    quota_bytes: Option<u64>,
    capacity_bytes: Option<u64>,
    registered_ms: u64,
}

/// Oxide's registration v2: the current field set, including `mode`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RegistrationV2 {
    version: u32,
    mount_point: PathBuf,
    volume_uuid: String,
    container_uuid: String,
    reserve_bytes: u64,
    cap_bytes: u64,
    quota_bytes: Option<u64>,
    capacity_bytes: Option<u64>,
    registered_ms: u64,
    mode: Availability,
}

fn parse_registration(path: &Path, bytes: &[u8]) -> Result<Observed<OxideRegistration>> {
    let value: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|error| divergence(path, format!("the file is not one JSON document: {error}")))?;
    let version = value
        .get("version")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| divergence(path, "the registration has no numeric version".to_owned()))?;
    let registration = match version {
        1 => {
            let raw: RegistrationV1 = serde_json::from_value(value).map_err(|error| {
                divergence(
                    path,
                    format!("the version 1 registration is not the field set Oxide reads: {error}"),
                )
            })?;
            finish_registration(
                path,
                raw.version,
                &raw.mount_point,
                &raw.volume_uuid,
                &raw.container_uuid,
                raw.reserve_bytes,
                raw.cap_bytes,
                raw.quota_bytes,
                raw.capacity_bytes,
                raw.registered_ms,
                // Version 1 states no mode; Oxide reads it as ephemeral.
                Availability::Ephemeral,
            )?
        }
        2 => {
            let raw: RegistrationV2 = serde_json::from_value(value).map_err(|error| {
                divergence(
                    path,
                    format!(
                        "the version 2 registration is not the field set Oxide writes: {error}"
                    ),
                )
            })?;
            finish_registration(
                path,
                raw.version,
                &raw.mount_point,
                &raw.volume_uuid,
                &raw.container_uuid,
                raw.reserve_bytes,
                raw.cap_bytes,
                raw.quota_bytes,
                raw.capacity_bytes,
                raw.registered_ms,
                raw.mode,
            )?
        }
        other => {
            return Err(divergence(
                path,
                format!("version {other} is not a registration version Barge reads (1 or 2)"),
            ));
        }
    };
    Ok(Observed::new(
        OXIDE_REGISTRATION_SOURCE,
        version.to_string(),
        path.to_path_buf(),
        registration,
    ))
}

#[allow(clippy::too_many_arguments)]
fn finish_registration(
    path: &Path,
    version: u32,
    mount_point: &Path,
    volume_uuid: &str,
    container_uuid: &str,
    reserve_bytes: u64,
    cap_bytes: u64,
    quota_bytes: Option<u64>,
    capacity_bytes: Option<u64>,
    registered_ms: u64,
    mode: Availability,
) -> Result<OxideRegistration> {
    if !mount_point.is_absolute() {
        return Err(divergence(
            path,
            format!(
                "the registered mount point `{}` is not absolute",
                mount_point.display()
            ),
        ));
    }
    Ok(OxideRegistration {
        version,
        mount_point: mount_point.to_path_buf(),
        volume_uuid: VolumeUuid::parse(volume_uuid).map_err(|error| {
            divergence(
                path,
                format!("the registered volume UUID is unusable: {error}"),
            )
        })?,
        container_uuid: VolumeUuid::parse(container_uuid).map_err(|error| {
            divergence(
                path,
                format!("the registered container UUID is unusable: {error}"),
            )
        })?,
        reserve_bytes,
        cap_bytes,
        quota_bytes,
        capacity_bytes,
        registered_ms,
        mode,
    })
}

/// Read Oxide's pools record strictly.
pub fn read_pools(path: &Path) -> Observation<OxidePools> {
    let bounds = Bounds::new(
        OXIDE_POOLS_SIZE_CAP_BYTES,
        OXIDE_POOL_ROWS,
        Deadline::after(READ_DEADLINE),
    );
    match state::read_bounded(path, &bounds) {
        ReadOutcome::Absent => Observation::Absent {
            source: OXIDE_POOLS_SOURCE,
            path: path.to_path_buf(),
            detail: format!("no pools record at {}", path.display()),
        },
        ReadOutcome::Refused(error) => observation_error(OXIDE_POOLS_SOURCE, path, &error),
        ReadOutcome::Present(bytes) => match parse_pools(path, &bytes) {
            Ok(observed) => Observation::Reported(observed),
            Err(error) => observation_error(OXIDE_POOLS_SOURCE, path, &error),
        },
    }
}

fn parse_pools(path: &Path, bytes: &[u8]) -> Result<Observed<OxidePools>> {
    let paths: Vec<PathBuf> = serde_json::from_slice(bytes).map_err(|error| {
        divergence(
            path,
            format!("the pools record is not an array of paths: {error}"),
        )
    })?;
    for pool in &paths {
        if !pool.is_absolute() {
            return Err(divergence(
                path,
                format!("the recorded pool `{}` is not absolute", pool.display()),
            ));
        }
    }
    Ok(Observed::new(
        OXIDE_POOLS_SOURCE,
        String::new(),
        path.to_path_buf(),
        OxidePools { paths },
    ))
}

fn divergence(path: &Path, message: String) -> Error {
    Error::new(ErrorKind::UnknownState, message)
        .with_detail("path", path.display().to_string().into())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::path::{Path, PathBuf};

    use super::{
        observe_oxide_at, read_pools, read_registration, Observation, ObservationState,
        OXIDE_POOLS_SOURCE, OXIDE_POOL_ROWS, OXIDE_REGISTRATION_SIZE_CAP_BYTES,
        OXIDE_REGISTRATION_SOURCE,
    };
    use crate::error::{ErrorKind, Result};
    use crate::os::AccountFacts;
    use crate::registry::{Availability, Durability, RecordOrigin, VolumeRecord};
    use crate::testkit::TempDir;

    const UUID: &str = "A6AC3ED7-33EA-46B4-8082-7254CF291ED2";
    const CONTAINER: &str = "AD1C4598-BCE0-4B23-9AAE-01349C7FA596";

    fn registration_v2(mode: &str) -> String {
        format!(
            r#"{{"version":2,"mount_point":"/Volumes/Builds","volume_uuid":"{UUID}","container_uuid":"{CONTAINER}","reserve_bytes":100000595968,"cap_bytes":749999226880,"quota_bytes":null,"capacity_bytes":999995129856,"registered_ms":1792525019847,"mode":"{mode}"}}"#
        )
    }

    fn registration_v1() -> String {
        format!(
            r#"{{"version":1,"mount_point":"/Volumes/Builds","volume_uuid":"{UUID}","container_uuid":"{CONTAINER}","reserve_bytes":100000595968,"cap_bytes":749999226880,"quota_bytes":null,"capacity_bytes":999995129856,"registered_ms":1792525019847}}"#
        )
    }

    fn unknown_detail<T: std::fmt::Debug>(observation: &Observation<T>) -> String {
        match observation {
            Observation::Unknown { detail, .. } => detail.clone(),
            other => panic!("expected an unknown observation, got {other:?}"),
        }
    }

    #[test]
    fn a_v2_registration_reports_its_facts_and_its_mode() {
        let fixture = TempDir::new("inspect-v2");
        let path = fixture.write("removable.json", registration_v2("permanent").as_bytes());
        let observation = read_registration(&path);
        assert_eq!(observation.state(), ObservationState::Reported);
        let observed = observation.observed().expect("a reported observation");
        assert_eq!(observed.source(), OXIDE_REGISTRATION_SOURCE);
        assert_eq!(observed.version(), "2");
        assert_eq!(observed.value().mode, Availability::Permanent);
        assert_eq!(observed.value().volume_uuid.as_str(), UUID);
        assert_eq!(observed.value().cap_bytes, 749_999_226_880);
        assert_eq!(observed.value().quota_bytes, None);
        assert!(observed.value().mount_point.is_absolute());
    }

    #[test]
    fn a_v1_registration_without_a_mode_is_ephemeral_exactly_as_oxide_reads_it() {
        let fixture = TempDir::new("inspect-v1");
        let path = fixture.write("removable.json", registration_v1().as_bytes());
        let observation = read_registration(&path);
        let observed = observation.observed().expect("a reported v1 observation");
        assert_eq!(observed.version(), "1");
        assert_eq!(observed.value().version, 1);
        assert_eq!(observed.value().mode, Availability::Ephemeral);
    }

    #[test]
    fn a_v1_registration_with_a_mode_and_a_v2_without_one_are_both_refused() {
        let fixture = TempDir::new("inspect-mixed-versions");
        let mut v1_with_mode: serde_json::Value =
            serde_json::from_str(&registration_v1()).expect("the fixture parses");
        v1_with_mode["mode"] = serde_json::json!("permanent");
        let path = fixture.write(
            "v1-with-mode.json",
            &serde_json::to_vec(&v1_with_mode).expect("encodes"),
        );
        assert_eq!(
            read_registration(&path).state(),
            ObservationState::UnknownState,
            "a v1 record with a mode is not the field set Oxide reads"
        );

        let mut v2_without_mode: serde_json::Value =
            serde_json::from_str(&registration_v2("permanent")).expect("the fixture parses");
        v2_without_mode
            .as_object_mut()
            .expect("an object")
            .remove("mode");
        let path = fixture.write(
            "v2-without-mode.json",
            &serde_json::to_vec(&v2_without_mode).expect("encodes"),
        );
        assert_eq!(
            read_registration(&path).state(),
            ObservationState::UnknownState
        );
    }

    #[test]
    fn an_unknown_field_an_unknown_version_and_an_unparsable_file_are_unknown_state() {
        let fixture = TempDir::new("inspect-divergent");
        let mut extra: serde_json::Value =
            serde_json::from_str(&registration_v2("permanent")).expect("the fixture parses");
        extra["nonce"] = serde_json::json!("a field Barge does not know");
        let extra = fixture.write("extra.json", &serde_json::to_vec(&extra).expect("encodes"));
        let observation = read_registration(&extra);
        assert_eq!(observation.state(), ObservationState::UnknownState);
        let detail = unknown_detail(&observation);
        assert!(detail.contains("extra.json"), "{detail}");
        assert!(detail.contains("nonce"), "{detail}");

        let future = fixture.write(
            "future.json",
            &serde_json::to_vec(&serde_json::json!({"version": 3})).expect("encodes"),
        );
        assert_eq!(
            read_registration(&future).state(),
            ObservationState::UnknownState
        );

        let broken = fixture.write("broken.json", b"{not json");
        let observation = read_registration(&broken);
        assert_eq!(observation.state(), ObservationState::UnknownState);
        assert!(unknown_detail(&observation).contains("broken.json"));

        let no_version = fixture.write("no-version.json", br#"{"mount_point":"/Volumes/X"}"#);
        assert_eq!(
            read_registration(&no_version).state(),
            ObservationState::UnknownState
        );

        let bad_uuid = fixture.write(
            "bad-uuid.json",
            registration_v2("permanent")
                .replace(UUID, "not-a-uuid")
                .as_bytes(),
        );
        assert_eq!(
            read_registration(&bad_uuid).state(),
            ObservationState::UnknownState
        );

        let relative_mount = fixture.write(
            "relative.json",
            registration_v2("permanent")
                .replace("/Volumes/Builds", "Volumes/Builds")
                .as_bytes(),
        );
        assert_eq!(
            read_registration(&relative_mount).state(),
            ObservationState::UnknownState
        );
    }

    #[test]
    fn an_oversized_registration_is_unknown_state() {
        let fixture = TempDir::new("inspect-registration-oversized");
        let path = fixture.write(
            "removable.json",
            &vec![b'x'; OXIDE_REGISTRATION_SIZE_CAP_BYTES + 1],
        );
        let observation = read_registration(&path);
        assert_eq!(observation.state(), ObservationState::UnknownState);
        assert!(unknown_detail(&observation).contains("removable.json"));
    }

    #[test]
    fn a_registration_that_is_a_link_is_not_read_through_it() {
        let fixture = TempDir::new("inspect-registration-link");
        let real = fixture.write("real.json", registration_v2("permanent").as_bytes());
        let link = fixture.join("removable.json");
        std::os::unix::fs::symlink(&real, &link).expect("the fixture link is made");
        assert_eq!(
            read_registration(&link).state(),
            ObservationState::UnknownState
        );
    }

    #[test]
    fn an_absent_registration_is_absent_and_not_an_empty_record() {
        let fixture = TempDir::new("inspect-registration-absent");
        let observation = read_registration(&fixture.join("removable.json"));
        assert_eq!(observation.state(), ObservationState::Absent);
        assert!(observation.observed().is_none());
        let row = observation.row();
        assert_eq!(row.state, ObservationState::Absent);
        assert_eq!(row.source, OXIDE_REGISTRATION_SOURCE);
        assert_eq!(row.version, "", "an absent file states no version");
        assert!(row.detail.is_some());
    }

    #[test]
    fn an_imported_registration_maps_to_an_observed_volume_and_never_to_authority() {
        let fixture = TempDir::new("inspect-import");
        let path = fixture.write("removable.json", registration_v2("permanent").as_bytes());
        let observation = read_registration(&path);
        let observed = observation.observed().expect("a reported observation");
        let volume = observed.value().observed_volume();
        assert_eq!(volume.origin, RecordOrigin::ObservedOxide);
        assert_eq!(volume.availability, Availability::Permanent);
        assert_eq!(volume.durability, Durability::Undeclared);
        assert!(!volume.durability.permits_pruning());
        assert_eq!(volume.mount_hint, Some(PathBuf::from("/Volumes/Builds")));

        // The type-level separation, proved in bytes: the observed mapping does
        // not satisfy the authority record's required field set.
        let json = serde_json::to_value(&volume).expect("the observed volume encodes");
        assert!(
            serde_json::from_value::<VolumeRecord>(json).is_err(),
            "an observed volume is not a registry record"
        );

        // A v1 import derives ephemeral availability and still declares no
        // durability.
        let v1 = fixture.write("removable-v1.json", registration_v1().as_bytes());
        let observed = read_registration(&v1);
        let volume = observed
            .observed()
            .expect("a reported v1 observation")
            .value()
            .observed_volume();
        assert_eq!(volume.availability, Availability::Ephemeral);
        assert_eq!(volume.durability, Durability::Undeclared);
    }

    #[test]
    fn a_pools_record_is_reported_as_paths_only() {
        let fixture = TempDir::new("inspect-pools");
        let path = fixture.write(
            "pools.json",
            br#"["/Users/fixture/Dev/oxide","/Volumes/Builds/oxide-pool"]"#,
        );
        let observation = read_pools(&path);
        assert_eq!(observation.state(), ObservationState::Reported);
        let observed = observation.observed().expect("a reported observation");
        assert_eq!(observed.source(), OXIDE_POOLS_SOURCE);
        assert_eq!(
            observed.value().paths(),
            [
                Path::new("/Users/fixture/Dev/oxide"),
                Path::new("/Volumes/Builds/oxide-pool")
            ]
        );
    }

    #[test]
    fn a_pools_record_over_its_cap_or_off_its_schema_is_unknown_state() {
        let fixture = TempDir::new("inspect-pools-divergent");
        let oversized = fixture.write("oversized.json", &vec![b' '; 64 * 1024 + 1]);
        assert_eq!(
            read_pools(&oversized).state(),
            ObservationState::UnknownState
        );

        let not_an_array = fixture.write("object.json", br#"{"pools":[]}"#);
        assert_eq!(
            read_pools(&not_an_array).state(),
            ObservationState::UnknownState
        );

        let not_paths = fixture.write("numbers.json", br#"[1,2,3]"#);
        assert_eq!(
            read_pools(&not_paths).state(),
            ObservationState::UnknownState
        );

        let relative = fixture.write("relative.json", br#"["Dev/oxide"]"#);
        assert_eq!(
            read_pools(&relative).state(),
            ObservationState::UnknownState
        );

        let broken = fixture.write("broken.json", b"[");
        assert_eq!(read_pools(&broken).state(), ObservationState::UnknownState);
    }

    #[test]
    fn a_pools_record_beyond_the_row_cap_is_reported_whole_and_bounded_by_the_document() {
        let fixture = TempDir::new("inspect-pools-rows");
        let paths: Vec<String> = (0..OXIDE_POOL_ROWS + 1)
            .map(|index| format!("\"/Volumes/pool-{index}\""))
            .collect();
        let path = fixture.write("pools.json", format!("[{}]", paths.join(",")).as_bytes());
        let observation = read_pools(&path);
        let observed = observation.observed().expect("a reported observation");
        assert_eq!(
            observed.value().paths().len(),
            OXIDE_POOL_ROWS + 1,
            "the parser reports what the file says"
        );
        let rows = crate::contract::RowSet::bounded(
            observed.value().paths().to_vec(),
            observed.value().paths().len(),
            OXIDE_POOL_ROWS,
        );
        assert!(rows.truncated, "a document truncates it with counts");
        assert_eq!(rows.count, OXIDE_POOL_ROWS);
    }

    #[test]
    fn the_registration_is_found_where_oxides_own_pools_record_says() {
        let fixture = TempDir::new("inspect-observe");
        let oxide_state = fixture.join("Library/Application Support/oxide/state");
        let oxide_cache = fixture.join("Library/Caches/oxide");
        std::fs::create_dir_all(&oxide_state).expect("the fixture state directory");
        std::fs::create_dir_all(&oxide_cache).expect("the fixture cache directory");
        let pool = fixture.join("Dev/oxide/oxide-managed");
        std::fs::write(
            oxide_state.join("pools.json"),
            serde_json::to_vec(&vec![fixture.join("Dev/oxide")]).expect("the record encodes"),
        )
        .expect("the pools record is written");
        std::fs::create_dir_all(&pool).expect("the fixture pool control directory");
        std::fs::write(pool.join("removable.json"), registration_v2("ephemeral"))
            .expect("the registration is written");

        let observations = observe_oxide_at(&oxide_state, &oxide_cache);
        assert_eq!(
            observations.registration.state(),
            ObservationState::Reported
        );
        let observed = observations
            .registration
            .observed()
            .expect("a reported observation");
        assert_eq!(observed.path(), pool.join("removable.json"));
        assert_eq!(observed.value().mode, Availability::Ephemeral);
        assert_eq!(observations.pools.state(), ObservationState::Reported);
        assert_eq!(
            observations.searched.len(),
            3,
            "the recorded pool plus the current and retired default pools"
        );
    }

    #[test]
    fn a_present_but_unreadable_registration_is_not_looked_past() {
        let fixture = TempDir::new("inspect-observe-unknown");
        let oxide_state = fixture.join("state");
        let oxide_cache = fixture.join("cache");
        std::fs::create_dir_all(&oxide_state).expect("the fixture state directory");
        std::fs::create_dir_all(&oxide_cache).expect("the fixture cache directory");
        let pool = fixture.join("pool/oxide-managed");
        std::fs::write(
            oxide_state.join("pools.json"),
            serde_json::to_vec(&vec![fixture.join("pool")]).expect("the record encodes"),
        )
        .expect("the pools record is written");
        std::fs::create_dir_all(&pool).expect("the fixture pool control directory");
        std::fs::write(pool.join("removable.json"), b"{broken")
            .expect("the broken registration is written");
        let default = fixture.join("cache/build-pool/oxide-managed");
        std::fs::create_dir_all(&default).expect("the default pool control directory");
        std::fs::write(default.join("removable.json"), registration_v2("permanent"))
            .expect("a valid registration is written elsewhere");

        let observations = observe_oxide_at(&oxide_state, &oxide_cache);
        assert_eq!(
            observations.registration.state(),
            ObservationState::UnknownState,
            "unknown state is a refusal, not a cue to look elsewhere"
        );
        assert_eq!(
            observations.registration.path(),
            pool.join("removable.json")
        );
    }

    #[test]
    fn no_registration_anywhere_is_absent_and_names_where_it_looked() {
        let fixture = TempDir::new("inspect-observe-absent");
        let observations = observe_oxide_at(&fixture.join("state"), &fixture.join("cache"));
        assert_eq!(observations.registration.state(), ObservationState::Absent);
        assert_eq!(observations.pools.state(), ObservationState::Absent);
        assert_eq!(observations.searched.len(), 2);
        let detail = observations
            .registration
            .detail()
            .expect("an absent observation says why");
        assert!(detail.contains("known internal pool"), "{detail}");
        assert!(!fixture.join("state").exists(), "observing creates nothing");
    }

    #[test]
    fn an_observation_row_carries_source_version_and_state() {
        let fixture = TempDir::new("inspect-rows");
        let path = fixture.write("removable.json", registration_v2("permanent").as_bytes());
        let row = read_registration(&path).row();
        assert_eq!(row.source, OXIDE_REGISTRATION_SOURCE);
        assert_eq!(row.version, "2");
        assert_eq!(row.state, ObservationState::Reported);
        assert_eq!(row.detail, None);
        let encoded = serde_json::to_value(&row).expect("the row encodes");
        assert_eq!(encoded["state"], "reported");
        assert!(encoded.get("detail").is_none(), "no detail when reported");
    }

    struct FakeAccount {
        home: PathBuf,
    }

    impl AccountFacts for FakeAccount {
        fn account_home(&self) -> Result<PathBuf> {
            Ok(self.home.clone())
        }

        fn state_root(&self) -> Result<PathBuf> {
            Ok(self.home.join("Library/Application Support/barge/state"))
        }

        fn tool_state_dir(&self, tool: &str) -> Result<PathBuf> {
            Ok(self
                .home
                .join("Library/Application Support")
                .join(tool)
                .join("state"))
        }

        fn tool_cache_dir(&self, tool: &str) -> Result<PathBuf> {
            Ok(self.home.join("Library/Caches").join(tool))
        }
    }

    #[test]
    fn the_account_facts_place_oxides_state_and_never_the_environment() {
        let fixture = TempDir::new("inspect-account");
        let account = FakeAccount {
            home: fixture.path().to_path_buf(),
        };
        let observations = super::observe_oxide(&account).expect("the account facts read");
        assert_eq!(observations.pools.state(), ObservationState::Absent);
        assert_eq!(
            observations.pools.path(),
            fixture
                .path()
                .join("Library/Application Support/oxide/state/pools.json")
        );
        assert!(
            !fixture.join("Library").exists(),
            "observing creates nothing"
        );
    }

    #[test]
    fn a_pools_observation_error_names_the_file() {
        let fixture = TempDir::new("inspect-pools-error");
        let path = fixture.write("pools.json", &vec![b'x'; 64 * 1024 + 1]);
        let observation = read_pools(&path);
        let error = observation
            .detail()
            .expect("an unknown observation says why");
        assert!(error.contains("pools.json"), "{error}");
        assert_eq!(observation.state(), ObservationState::UnknownState);
        assert_eq!(
            crate::error::Error::new(ErrorKind::UnknownState, "x").kind(),
            ErrorKind::UnknownState
        );
    }
}
