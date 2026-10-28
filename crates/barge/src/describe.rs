//! The versioned documents the library and CLI return: `barge.describe/1` and
//! `barge.inspect/1` (stage 1 design, "Versioned JSON and error contract").
//!
//! `describe` reports what Barge knows: the account state directory and whether
//! it is initialized, every registered volume from the registry, and every
//! foreign observation with its source and version. It performs no mutation and
//! takes only a shared registry lock, and it refuses an invalid registry rather
//! than reporting an empty one — a silently short list would read as "no
//! volumes".
//!
//! `inspect` is the read-only diagnostic over the same facts: it reports the
//! state directory, the registry as absent, valid or invalid with the reason, and
//! Oxide's observations in full, including the volume an import would describe.
//! It creates no file, directory or lock, so it is safe against a host whose
//! authority is not initialized.
//!
//! Both readers refuse an unknown major, a document over the size cap, an
//! unknown command and any missing or mistyped required field. An extra field is
//! accepted, because within a major version a new optional field is a compatible
//! addition — reading is strict about what it requires, not about what it may
//! ignore.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub use crate::inspect::{ObservationRow, ObservationState};
pub use crate::registry::{Availability, Durability, FolderOverrides, RecordOrigin};
pub use crate::state::StateDirSource;

use crate::area::{self, AreaDocument, RootObservation};
use crate::contract::{
    parse_document, FormatVersions, RowSet, AREA_ROWS, DESCRIBE_SCHEMA, DESCRIBE_SCHEMA_PREFIX,
    INSPECT_SCHEMA, INSPECT_SCHEMA_PREFIX, OBSERVATION_ROWS, ROOT_ROWS, VOLUME_ROWS,
};
use crate::error::{Error, ErrorKind, Result};
use crate::inspect::{
    self, Observation, ObservedVolume, OxideObservations, OxideRegistration, OXIDE_POOL_ROWS,
};
use crate::os::{AccountFacts, DirOps, MountRoot, ProcessObserver, VolumeFacts};
use crate::registry::{self, Registry, VolumeRecord};
use crate::state::{LockSet, StatePaths};

/// The command name the describe document belongs to.
pub const DESCRIBE_COMMAND: &str = "describe";

/// The command name the inspect document belongs to.
pub const INSPECT_COMMAND: &str = "inspect";

/// The tool name a document reports.
pub const TOOL_NAME: &str = "barge";

/// What `describe` reads. The caller supplies the platform's account facts and
/// the environment it was given; nothing is created or written.
pub struct DescribeRequest<'a> {
    /// The platform's account facts. The account home comes from here, never
    /// from the environment.
    pub account: &'a dyn AccountFacts,
    /// The environment's `HOME`, reported only so a mismatch with the account
    /// home is visible.
    pub environment_home: Option<&'a Path>,
    /// `BARGE_STATE_DIR`, the fixture-only override. When present it is the
    /// state directory and the document says the path came from the
    /// environment.
    pub state_dir_override: Option<&'a Path>,
    /// The tool version the document reports.
    pub tool_version: &'a str,
    /// The platform's directory operations, used to read owned roots without
    /// creating or changing anything.
    pub dirs: &'a dyn DirOps,
    /// The platform's volume facts, used to verify each volume before its areas
    /// are walked.
    pub facts: &'a dyn VolumeFacts,
    /// The platform's mount-root proof.
    pub mounts: &'a dyn MountRoot,
    /// Space measurement provider, when the caller requests space rows.
    pub space: Option<&'a dyn crate::os::SpaceFacts>,
    /// The platform's process observations, for a root's holds.
    pub observer: &'a dyn ProcessObserver,
}

impl fmt::Debug for DescribeRequest<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DescribeRequest")
            .field("environment_home", &self.environment_home)
            .field("state_dir_override", &self.state_dir_override)
            .field("tool_version", &self.tool_version)
            .finish_non_exhaustive()
    }
}

/// What `inspect` reads: the same account facts and environment, and nothing
/// else.
pub struct InspectRequest<'a> {
    /// The platform's account facts.
    pub account: &'a dyn AccountFacts,
    /// The environment's `HOME`, reported beside the account home.
    pub environment_home: Option<&'a Path>,
    /// `BARGE_STATE_DIR`, the fixture-only override.
    pub state_dir_override: Option<&'a Path>,
    /// The tool version the document reports.
    pub tool_version: &'a str,
}

impl fmt::Debug for InspectRequest<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InspectRequest")
            .field("environment_home", &self.environment_home)
            .field("state_dir_override", &self.state_dir_override)
            .field("tool_version", &self.tool_version)
            .finish_non_exhaustive()
    }
}

/// The `data` of `barge.describe/1`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DescribeData {
    /// The tool that produced the document.
    pub tool: ToolVersions,
    /// Every format version this build speaks.
    pub formats: FormatVersions,
    /// The account state directory and whether it is initialized.
    pub state: StateReport,
    /// Registered volumes, bounded to the volume row cap.
    pub volumes: RowSet<VolumeRow>,
    /// Declared areas, bounded to the area row cap.
    pub areas: RowSet<AreaRow>,
    /// Placements, bounded to the placement row cap.
    pub placements: RowSet<PlacementRow>,
    /// Owned roots found under every declared area of every present volume,
    /// bounded to the root row cap. Each row reports the path its marker
    /// recorded and the path the current configuration resolves, so a
    /// `root_dir` change is visible without anything having moved.
    #[serde(default)]
    pub roots: RowSet<RootObservation>,
    /// Per-volume space measurements and declared policies.
    #[serde(default)]
    pub space: RowSet<crate::space::SpaceRow>,
    /// Foreign observations, bounded to the observation row cap.
    pub observations: RowSet<ObservationRow>,
    /// Identity facts used across a remount.
    #[serde(default)]
    pub identity: IdentityCapability,
    /// The writer-custody capability this build offers, its named limits, and
    /// the fact that no kernel proof is claimed (slice S5). An older document
    /// without this field still reads.
    #[serde(default)]
    pub custody: crate::custody::CustodyReport,
}

/// Which root identity facts survive a remount.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdentityCapability {
    pub capability: String,
    pub stable_facts: Vec<String>,
    pub device_role: String,
}

impl Default for IdentityCapability {
    fn default() -> Self {
        Self {
            capability: "volume_uuid_root_id_nonce_inode_relative_path".into(),
            stable_facts: [
                "volume_uuid",
                "root_id",
                "nonce",
                "inode",
                "mount_relative_path",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
            device_role: "within_run_only".into(),
        }
    }
}

/// A complete live census, or a reason the count is unknown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum MountCensus {
    Complete { count: usize, paths: Vec<PathBuf> },
    Unknown { reason: String },
}

/// The `data` of `barge.inspect/1`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectData {
    /// The tool that produced the document.
    pub tool: ToolVersions,
    /// Every format version this build speaks.
    pub formats: FormatVersions,
    /// The account state directory and whether it is initialized.
    pub state: StateReport,
    /// The registry as it was found, with the volumes it holds.
    pub registry: RegistryReport,
    /// Foreign observations, bounded to the observation row cap.
    pub observations: RowSet<ObservationRow>,
    /// Oxide's observations in full.
    pub oxide: OxideReport,
}

/// The tool that produced a document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolVersions {
    /// `barge`.
    pub name: String,
    /// The build's version.
    pub version: String,
}

/// The account state directory, as the documents report it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateReport {
    /// The state directory path, as resolved. It is not canonicalised: the path
    /// may not exist yet, and a read-only command creates nothing.
    pub dir: PathBuf,
    /// Whether the state directory exists as a directory.
    pub initialized: bool,
    /// Where the path came from.
    pub source: StateDirSource,
    /// The account home and the environment's own, so a mismatch is visible.
    pub home: HomeReport,
}

/// The account home, and the environment's `HOME` when it differs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HomeReport {
    /// The password database's home for the effective uid.
    pub account: PathBuf,
    /// The environment's `HOME`, when one was set.
    pub environment: Option<PathBuf>,
    /// Whether the environment's `HOME` differs from the account home.
    pub mismatch: bool,
}

/// One registered volume, as the documents report it.
///
/// It projects the fields the design names for `describe`; the record's own
/// `scratch_grants`, `generation` and `registered_ms` stay in the registry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeRow {
    /// The APFS volume UUID from the kernel: the identity that matters.
    pub volume_uuid: String,
    /// The last observed mount point; used to find the volume, never as
    /// identity.
    pub mount_hint: Option<PathBuf>,
    /// The current census; unknown is never reported as zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mounts: Option<MountCensus>,
    /// When the volume is attached.
    pub availability: Availability,
    /// What the data on it is.
    pub durability: Durability,
    /// The folder relative to the mount that holds everything Barge owns;
    /// `""` means the drive root.
    pub root_dir: String,
    /// Per-volume overrides of Barge's own folders.
    pub folders: FolderOverrides,
    /// Whether an operator declared the record or it was imported.
    pub origin: RecordOrigin,
}

impl AreaRow {
    /// Project one stored declaration into the document's row shape.
    pub fn from_document(document: &AreaDocument) -> Self {
        Self {
            namespace: document.namespace.clone(),
            area: document.area.clone(),
            folder: document.folder.clone(),
            overrides: document.overrides.clone(),
        }
    }
}

impl VolumeRow {
    /// Project one registry record into the document's row shape.
    pub fn from_record(record: &VolumeRecord) -> Self {
        Self {
            volume_uuid: record.volume_uuid.as_str().to_owned(),
            mount_hint: record.mount_hint.clone(),
            mounts: None,
            availability: record.availability,
            durability: record.durability,
            root_dir: record.root_dir.clone(),
            folders: record.folders.clone(),
            origin: record.origin,
        }
    }
}

/// One declared consumer area, as `describe` reports it (stage 1 design,
/// "Area declaration record on disk"). Slice S3 fills these rows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AreaRow {
    /// The consumer-chosen namespace; opaque to Barge.
    pub namespace: String,
    /// The consumer-chosen area name; opaque to Barge.
    pub area: String,
    /// The default folder under `root_dir`.
    pub folder: String,
    /// Per-volume overrides, keyed by volume UUID.
    pub overrides: BTreeMap<String, String>,
}

/// One placement, as `describe` reports it (stage 1 design, "Kinds and
/// placement forms"). Slice S6 fills these rows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlacementRow {
    /// What kind of cache or data the placement serves.
    pub kind: PlacementKind,
    /// The tool's own path that the placement serves.
    pub original_path: PathBuf,
    /// The volume the placement lives on.
    pub volume_uuid: String,
    /// The path resolved and recorded at adopt time.
    pub recorded_path: PathBuf,
    /// The path the current configuration would resolve. Recorded beats
    /// configured, so the two may differ without moving anything.
    pub configured_path: PathBuf,
    /// The state of the link or setting that publishes the placement.
    pub link: LinkState,
    /// What a caller sees when the volume is absent.
    pub absent: AbsentPolicy,
}

/// The placement kinds stage 1 designs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlacementKind {
    /// The npm cache, redirected by its own setting.
    NpmCache,
    /// The bun cache, redirected by its own setting.
    BunCache,
    /// The Cargo registry, adopted in place or linked.
    CargoRegistry,
    /// The Cargo git cache, adopted in place or linked.
    CargoGit,
}

/// The state of a placement's published path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkState {
    /// The recorded link or setting is in place.
    Linked,
    /// The placement is recorded and publishes no link.
    Unlinked,
    /// The path is absent although the record expects it.
    Missing,
    /// The path could not be inspected; unknown is never read as absent.
    Unknown,
}

/// What a caller sees when the placement's volume is absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AbsentPolicy {
    /// Fail, naming the drive to attach.
    Fail,
    /// The caller decides; Barge reports the placement unavailable.
    Wait,
    /// A consumer-owned internal path, declared explicitly.
    FallbackLocal,
}

/// How the registry was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RegistryReportState {
    /// There is no registry file, so no volume is registered.
    Absent,
    /// The registry validated.
    Valid,
    /// The registry is there but did not validate; nothing was changed.
    Invalid,
}

/// The registry as `inspect` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryReport {
    /// The registry file that was looked at.
    pub path: PathBuf,
    /// Absent, valid or invalid.
    pub state: RegistryReportState,
    /// Why the registry is invalid, when it is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The registered volumes, bounded to the volume row cap. Empty unless the
    /// registry validated.
    pub volumes: RowSet<VolumeRow>,
}

/// Oxide's observations as `inspect` reports them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OxideReport {
    /// The removable-volume registration.
    pub registration: RegistrationReport,
    /// The informational pools record.
    pub pools: PoolsReport,
}

/// One foreign registration as `inspect` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistrationReport {
    /// The file that was read or looked at.
    pub path: PathBuf,
    /// Whether it was reported, absent or unknown.
    pub state: ObservationState,
    /// Why it is absent or unknown, when it is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The facts it states, when it was read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub facts: Option<OxideRegistration>,
    /// The volume it describes, as an observation and never as authority.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_volume: Option<ObservedVolume>,
    /// Every registration path that was looked at, in order.
    pub searched: Vec<PathBuf>,
}

/// Oxide's pools record as `inspect` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolsReport {
    /// The file that was read or looked at.
    pub path: PathBuf,
    /// Whether it was reported, absent or unknown.
    pub state: ObservationState,
    /// Why it is absent or unknown, when it is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The pool paths it names, bounded to Oxide's own row cap.
    pub paths: RowSet<PathBuf>,
}

/// Build the `barge.describe/1` data.
///
/// No file, directory or lock is created: the state directory is inspected, the
/// registry is read under a shared lock when its lock file already exists, and
/// Oxide's records are read. An invalid registry is refused, naming the file,
/// because reporting no volumes would read as "none registered".
pub fn describe(request: &DescribeRequest<'_>) -> Result<DescribeData> {
    let paths = StatePaths::resolve(request.account, request.state_dir_override)?;
    let state = state_report(request.account, request.environment_home, &paths)?;
    let registry = read_registry_observed(&paths)?;
    let observations = inspect::observe_oxide(request.account)?;
    let declarations = area::read_declarations(&paths)?;

    let volume_rows: Vec<VolumeRow> = registry
        .volumes()
        .iter()
        .map(|record| {
            let mut row = VolumeRow::from_record(record);
            row.mounts = Some(match request.facts.mount_census(&record.volume_uuid) {
                Ok(paths) => MountCensus::Complete {
                    count: paths.len(),
                    paths,
                },
                Err(error) => MountCensus::Unknown {
                    reason: error.to_string(),
                },
            });
            row
        })
        .collect();
    let volume_total = volume_rows.len();
    let area_rows: Vec<AreaRow> = declarations
        .documents
        .iter()
        .map(AreaRow::from_document)
        .collect();
    let area_total = area_rows.len();
    let roots = collect_roots(request, &paths, registry.volumes(), &declarations);
    let placements = crate::cache::placement_rows(
        &paths,
        request.dirs,
        request.facts,
        request.mounts,
        request.observer,
        registry.volumes(),
    )?;
    let observation_rows = observations.rows().to_vec();
    let observation_total = observation_rows.len();

    let space = match request.space {
        Some(provider) => {
            crate::space::report(&paths, request.facts, request.mounts, provider)?.volumes
        }
        None => RowSet::empty(),
    };

    Ok(DescribeData {
        tool: ToolVersions {
            name: TOOL_NAME.to_owned(),
            version: request.tool_version.to_owned(),
        },
        formats: FormatVersions::current(),
        state,
        volumes: RowSet::bounded(volume_rows, volume_total, VOLUME_ROWS),
        areas: RowSet::bounded(area_rows, area_total, AREA_ROWS),
        placements,
        roots,
        space,
        observations: RowSet::bounded(observation_rows, observation_total, OBSERVATION_ROWS),
        custody: crate::custody::capability_report(),
        identity: IdentityCapability::default(),
    })
}

/// Read every owned root of every declared area on every present volume.
///
/// This is a read-only walk: it creates nothing, takes only the shared registry
/// lock the caller already holds, and skips a volume it cannot verify (an
/// unplugged drive is reported by its volume row, and `describe` stays usable).
/// The reported paths are the marker's recorded path and the path the current
/// configuration resolves, which is how a `root_dir` change is visible.
fn collect_roots(
    request: &DescribeRequest<'_>,
    paths: &StatePaths,
    volumes: &[VolumeRecord],
    declarations: &area::DeclarationSet,
) -> RowSet<RootObservation> {
    let mut rows = Vec::new();
    let mut total = 0usize;
    for record in volumes {
        if record.mount_hint.is_none() {
            continue;
        }
        for document in &declarations.documents {
            let caller = document.declaration();
            let Ok(resolved) = area::resolve(
                request.dirs,
                request.facts,
                request.mounts,
                request.observer,
                record,
                &caller,
                Some(document),
            ) else {
                continue;
            };
            let Ok(listing) =
                area::list_roots(request.dirs, request.observer, paths, &resolved, ROOT_ROWS)
            else {
                continue;
            };
            total = total.saturating_add(listing.roots.len());
            rows.extend(listing.roots);
            if rows.len() >= ROOT_ROWS {
                return RowSet::bounded(rows, total, ROOT_ROWS);
            }
        }
    }
    RowSet::bounded(rows, total, ROOT_ROWS)
}

/// Build the `barge.inspect/1` data.
///
/// Like `describe` it creates nothing and takes only a shared lock. Unlike
/// `describe` an invalid registry is not a failure here: diagnosing it is the
/// point, so the report says `invalid` and names the file.
pub fn inspect(request: &InspectRequest<'_>) -> Result<InspectData> {
    let paths = StatePaths::resolve(request.account, request.state_dir_override)?;
    let state = state_report(request.account, request.environment_home, &paths)?;
    let registry = registry_report(&paths)?;
    let observations = inspect::observe_oxide(request.account)?;
    let rows = observations.rows().to_vec();
    let total = rows.len();
    Ok(InspectData {
        tool: ToolVersions {
            name: TOOL_NAME.to_owned(),
            version: request.tool_version.to_owned(),
        },
        formats: FormatVersions::current(),
        state,
        registry,
        observations: RowSet::bounded(rows, total, OBSERVATION_ROWS),
        oxide: oxide_report(&observations),
    })
}

/// The state directory and home report both documents share.
pub(crate) fn state_report(
    account: &dyn AccountFacts,
    environment_home: Option<&Path>,
    paths: &StatePaths,
) -> Result<StateReport> {
    let account_home = account.account_home()?;
    let environment_home = environment_home.map(Path::to_path_buf);
    let mismatch = environment_home
        .as_ref()
        .is_some_and(|home| home != &account_home);
    Ok(StateReport {
        dir: paths.root().to_path_buf(),
        initialized: paths.is_initialized()?,
        source: paths.source(),
        home: HomeReport {
            account: account_home,
            environment: environment_home,
            mismatch,
        },
    })
}

/// Read the registry under a shared lock, when a lock file already exists.
fn read_registry_observed(paths: &StatePaths) -> Result<Registry> {
    let mut locks = LockSet::new(paths.clone());
    let _lock = locks.observe_registry()?;
    registry::read(paths)
}

fn registry_report(paths: &StatePaths) -> Result<RegistryReport> {
    let path = paths.registry_path();
    match read_registry_observed(paths) {
        Ok(Registry::Absent) => Ok(RegistryReport {
            path,
            state: RegistryReportState::Absent,
            detail: Some("there is no registry, so no volume is registered".to_owned()),
            volumes: RowSet::empty(),
        }),
        Ok(Registry::Present(document)) => {
            let rows: Vec<VolumeRow> = document
                .volumes
                .iter()
                .map(VolumeRow::from_record)
                .collect();
            let total = rows.len();
            Ok(RegistryReport {
                path,
                state: RegistryReportState::Valid,
                detail: None,
                volumes: RowSet::bounded(rows, total, VOLUME_ROWS),
            })
        }
        Err(error) => Ok(RegistryReport {
            path: path.clone(),
            state: RegistryReportState::Invalid,
            detail: Some(format!("{}: {}", path.display(), error.message())),
            volumes: RowSet::empty(),
        }),
    }
}

fn oxide_report(observations: &OxideObservations) -> OxideReport {
    let searched = observations.searched.clone();
    let registration = match &observations.registration {
        Observation::Reported(observed) => RegistrationReport {
            path: observed.path().to_path_buf(),
            state: ObservationState::Reported,
            detail: None,
            facts: Some(observed.value().clone()),
            observed_volume: Some(observed.value().observed_volume()),
            searched,
        },
        Observation::Absent { path, detail, .. } => RegistrationReport {
            path: path.clone(),
            state: ObservationState::Absent,
            detail: Some(detail.clone()),
            facts: None,
            observed_volume: None,
            searched,
        },
        Observation::Unknown { path, detail, .. } => RegistrationReport {
            path: path.clone(),
            state: ObservationState::UnknownState,
            detail: Some(detail.clone()),
            facts: None,
            observed_volume: None,
            searched,
        },
    };
    let pools = match &observations.pools {
        Observation::Reported(observed) => {
            let paths = observed.value().paths().to_vec();
            let total = paths.len();
            PoolsReport {
                path: observed.path().to_path_buf(),
                state: ObservationState::Reported,
                detail: None,
                paths: RowSet::bounded(paths, total, OXIDE_POOL_ROWS),
            }
        }
        Observation::Absent { path, detail, .. } => PoolsReport {
            path: path.clone(),
            state: ObservationState::Absent,
            detail: Some(detail.clone()),
            paths: RowSet::empty(),
        },
        Observation::Unknown { path, detail, .. } => PoolsReport {
            path: path.clone(),
            state: ObservationState::UnknownState,
            detail: Some(detail.clone()),
            paths: RowSet::empty(),
        },
    };
    OxideReport {
        registration,
        pools,
    }
}

/// Read one `barge.describe/1` document.
///
/// A reader refuses an unknown major, a document over the size cap, an unknown
/// command and any missing or mistyped required field, and refuses a list whose
/// counts disagree with its rows. It accepts an extra field, which is how a new
/// optional field lands inside a major version.
pub fn read_describe_document(bytes: &[u8]) -> Result<DescribeData> {
    let data = document_data(
        bytes,
        DESCRIBE_SCHEMA,
        DESCRIBE_SCHEMA_PREFIX,
        DESCRIBE_COMMAND,
    )?;
    let data: DescribeData = serde_json::from_value(data).map_err(|error| {
        unknown(format!(
            "the document does not provide the field set {DESCRIBE_SCHEMA} requires: {error}"
        ))
    })?;
    data.volumes.validate()?;
    data.areas.validate()?;
    data.placements.validate()?;
    data.roots.validate()?;
    data.space.validate()?;
    data.observations.validate()?;
    Ok(data)
}

/// Read one `barge.inspect/1` document.
pub fn read_inspect_document(bytes: &[u8]) -> Result<InspectData> {
    let data = document_data(
        bytes,
        INSPECT_SCHEMA,
        INSPECT_SCHEMA_PREFIX,
        INSPECT_COMMAND,
    )?;
    let data: InspectData = serde_json::from_value(data).map_err(|error| {
        unknown(format!(
            "the document does not provide the field set {INSPECT_SCHEMA} requires: {error}"
        ))
    })?;
    data.registry.volumes.validate()?;
    data.observations.validate()?;
    data.oxide.pools.paths.validate()?;
    Ok(data)
}

/// Check one document's envelope and return its `data`.
fn document_data(
    bytes: &[u8],
    schema: &str,
    schema_prefix: &str,
    command: &str,
) -> Result<serde_json::Value> {
    let document = parse_document(bytes)?;
    let found = document
        .get("schema")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| unknown("the document has no schema string"))?;
    if found != schema {
        let message = if found.starts_with(schema_prefix) {
            format!("`{found}` is an unknown major; this reader knows {schema}")
        } else {
            format!("`{found}` is not a {schema_prefix} document")
        };
        return Err(unknown(message)
            .with_detail("schema", found.into())
            .with_detail("supported", schema.into()));
    }
    if document.get("command").and_then(serde_json::Value::as_str) != Some(command) {
        return Err(unknown(format!(
            "the document is not a `{command}` document"
        )));
    }
    if document.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
        return Err(unknown("the document is not a successful command document"));
    }
    document
        .get("data")
        .cloned()
        .ok_or_else(|| unknown("the document has no data"))
}

fn unknown(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::UnknownState, message)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use super::{
        describe, inspect, read_describe_document, read_inspect_document, AbsentPolicy,
        DescribeData, DescribeRequest, InspectData, InspectRequest, LinkState, ObservationRow,
        ObservationState, PlacementKind, RegistryReportState, StateDirSource,
    };
    use crate::contract::{success_bytes, RowSet, DESCRIBE_SCHEMA, INSPECT_SCHEMA};
    use crate::error::{Error, ErrorKind, Result};
    use crate::os::{AccountFacts, DirOps};
    use crate::registry::{
        Availability, Durability, FolderOverrides, RecordOrigin, RegistryDocument, RegistryWriter,
        ScratchGrants, VolumeRecord,
    };
    use crate::state::{Deadline, LockSet, StatePaths};
    use crate::testkit::TempDir;

    const UUID: &str = "A6AC3ED7-33EA-46B4-8082-7254CF291ED2";

    fn registration_v2() -> &'static str {
        r#"{"version":2,"mount_point":"/Volumes/Builds","volume_uuid":"A6AC3ED7-33EA-46B4-8082-7254CF291ED2","container_uuid":"AD1C4598-BCE0-4B23-9AAE-01349C7FA596","reserve_bytes":100000595968,"cap_bytes":749999226880,"quota_bytes":null,"capacity_bytes":999995129856,"registered_ms":1792525019847,"mode":"permanent"}"#
    }

    /// Injected account facts: the account home is a fixture, so no test reads
    /// or writes a real account's state or another tool's state.
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

    struct Fixture {
        dir: TempDir,
        account: FakeAccount,
        state_dir: PathBuf,
        platform: crate::os::MacOs,
    }

    impl Fixture {
        fn new(label: &str) -> Self {
            let dir = TempDir::new(label);
            let account = FakeAccount {
                home: dir.path().to_path_buf(),
            };
            let state_dir = dir.join("state");
            Self {
                dir,
                account,
                state_dir,
                platform: crate::os::platform(),
            }
        }

        fn state_override(&self) -> &Path {
            &self.state_dir
        }

        fn request(&self) -> DescribeRequest<'_> {
            DescribeRequest {
                account: &self.account,
                environment_home: None,
                state_dir_override: Some(self.state_override()),
                tool_version: "0.1.0",
                dirs: &self.platform,
                facts: &self.platform,
                mounts: &self.platform,
                space: None,
                observer: &self.platform,
            }
        }

        fn inspect_request(&self) -> InspectRequest<'_> {
            InspectRequest {
                account: &self.account,
                environment_home: None,
                state_dir_override: Some(self.state_override()),
                tool_version: "0.1.0",
            }
        }

        fn state(&self) -> StatePaths {
            StatePaths::resolve(&self.account, Some(self.state_override()))
                .expect("the fixture state directory resolves")
        }

        /// Write a valid registry holding one declared volume.
        fn register_a_volume(&self) {
            self.state().initialize().expect("the layout exists");
            let mut locks =
                LockSet::with_deadline(self.state(), Deadline::after(Duration::from_secs(5)));
            let lock = locks.lock_registry().expect("the registry lock");
            let document = RegistryDocument::new(vec![VolumeRecord {
                volume_uuid: crate::os::VolumeUuid::parse(UUID).expect("a UUID"),
                mount_hint: Some(PathBuf::from("/Volumes/Builds")),
                availability: Availability::Permanent,
                durability: Durability::Expendable,
                root_dir: String::new(),
                folders: FolderOverrides::none(),
                origin: RecordOrigin::Declared,
                scratch_grants: ScratchGrants::none(),
                space: None,
                generation: 1,
                registered_ms: 1_792_525_019_847,
            }]);
            RegistryWriter::new(&self.state())
                .publish(&lock, &document)
                .expect("the registry is written");
        }

        /// Write Oxide's pools record and registration into the fixture.
        fn observe_oxide(&self) {
            let pools =
                serde_json::to_vec(&vec![self.dir.join("pool")]).expect("the record encodes");
            self.dir
                .write("Library/Application Support/oxide/state/pools.json", &pools);
            self.dir.write(
                "pool/oxide-managed/removable.json",
                registration_v2().as_bytes(),
            );
        }
    }

    fn document(data: &DescribeData) -> Vec<u8> {
        success_bytes(DESCRIBE_SCHEMA, super::DESCRIBE_COMMAND, data).expect("the document encodes")
    }

    fn inspect_document(data: &InspectData) -> Vec<u8> {
        success_bytes(INSPECT_SCHEMA, super::INSPECT_COMMAND, data).expect("the document encodes")
    }

    #[test]
    fn an_uninitialized_state_directory_is_reported_and_not_created() {
        let fixture = Fixture::new("describe-uninitialized");
        assert!(
            !fixture.state_override().exists(),
            "the fixture starts absent"
        );
        let data = describe(&fixture.request()).expect("describe reads");
        assert!(!data.state.initialized);
        assert_eq!(data.state.dir, fixture.state_override());
        assert_eq!(data.state.source, StateDirSource::EnvironmentOverride);
        assert!(
            !fixture.state_override().exists(),
            "describe must not create the state directory"
        );
    }

    #[test]
    fn the_account_state_root_is_reported_when_no_override_is_given() {
        let fixture = Fixture::new("describe-account-root");
        let data = describe(&DescribeRequest {
            account: &fixture.account,
            environment_home: None,
            state_dir_override: None,
            tool_version: "0.1.0",
            dirs: &fixture.platform,
            facts: &fixture.platform,
            mounts: &fixture.platform,
            space: None,
            observer: &fixture.platform,
        })
        .expect("describe reads");
        assert_eq!(
            data.state.dir,
            fixture
                .account
                .home
                .join("Library/Application Support/barge/state")
        );
        assert_eq!(data.state.source, StateDirSource::Account);
        assert!(!data.state.initialized);
        assert!(!data.state.dir.exists(), "describe creates nothing");
    }

    #[test]
    fn a_home_that_differs_from_the_account_home_is_reported_and_moves_nothing() {
        let fixture = Fixture::new("describe-home-mismatch");
        let other = PathBuf::from("/Users/an-environment-home");
        let data = describe(&DescribeRequest {
            account: &fixture.account,
            environment_home: Some(&other),
            state_dir_override: None,
            tool_version: "0.1.0",
            dirs: &fixture.platform,
            facts: &fixture.platform,
            mounts: &fixture.platform,
            space: None,
            observer: &fixture.platform,
        })
        .expect("describe reads");
        assert!(data.state.home.mismatch);
        assert_eq!(data.state.home.environment, Some(other.clone()));
        assert_eq!(
            data.state.dir,
            fixture
                .account
                .home
                .join("Library/Application Support/barge/state"),
            "a differing HOME does not move state"
        );

        let same = describe(&DescribeRequest {
            account: &fixture.account,
            environment_home: Some(&fixture.account.home),
            state_dir_override: None,
            tool_version: "0.1.0",
            dirs: &fixture.platform,
            facts: &fixture.platform,
            mounts: &fixture.platform,
            space: None,
            observer: &fixture.platform,
        })
        .expect("describe reads");
        assert!(!same.state.home.mismatch);
    }

    #[test]
    fn an_absent_registry_reports_no_volumes_and_two_absent_observations() {
        let fixture = Fixture::new("describe-empty");
        let data = describe(&fixture.request()).expect("describe reads");
        assert_eq!(data.volumes.count, 0);
        assert_eq!(data.volumes.total, 0);
        assert!(!data.volumes.truncated);
        assert_eq!(
            data.observations.count, 2,
            "the registration and the pools record"
        );
        assert_eq!(data.observations.total, 2);
        assert!(!data.observations.truncated);
        for row in &data.observations.rows {
            assert_eq!(row.state, ObservationState::Absent, "{}", row.source);
        }
        assert_eq!(data.areas.count, 0);
        assert_eq!(data.placements.count, 0);
        assert_eq!(
            data.custody.capability,
            crate::custody::CAPABILITY,
            "describe reports the custody capability honestly"
        );
        assert!(
            !data.custody.kernel_proof,
            "no kernel proof is claimed for writer custody"
        );
    }

    #[test]
    fn a_registered_volume_and_a_present_registration_are_reported() {
        let fixture = Fixture::new("describe-filled");
        fixture.register_a_volume();
        fixture.observe_oxide();
        let data = describe(&fixture.request()).expect("describe reads");
        assert_eq!(data.volumes.count, 1);
        let row = &data.volumes.rows[0];
        assert_eq!(row.volume_uuid, UUID);
        assert_eq!(row.availability, Availability::Permanent);
        assert_eq!(row.durability, Durability::Expendable);
        assert_eq!(row.origin, RecordOrigin::Declared);
        assert_eq!(row.root_dir, "");

        let reported: Vec<&ObservationRow> = data
            .observations
            .rows
            .iter()
            .filter(|row| row.state == ObservationState::Reported)
            .collect();
        assert_eq!(reported.len(), 2, "both foreign records were read");
        let registration = reported
            .iter()
            .find(|row| row.source == "oxide.removable")
            .expect("the registration row");
        assert_eq!(registration.version, "2");
        assert!(registration.detail.is_none());
        assert!(
            fixture.state_override().join("registry.json").exists(),
            "describe read the registry it reports"
        );
    }

    #[test]
    fn describe_refuses_an_invalid_registry_instead_of_reporting_no_volumes() {
        let fixture = Fixture::new("describe-invalid-registry");
        fixture.state().initialize().expect("the layout exists");
        fixture
            .dir
            .write("state/registry.json", b"{ not a registry");
        let error = describe(&fixture.request()).expect_err("an invalid registry is refused");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
        assert_eq!(
            error.detail()["path"],
            fixture
                .state_override()
                .join("registry.json")
                .display()
                .to_string()
        );
    }

    #[test]
    fn inspect_reports_an_invalid_registry_with_its_reason() {
        let fixture = Fixture::new("inspect-invalid-registry");
        fixture.state().initialize().expect("the layout exists");
        let path = fixture
            .dir
            .write("state/registry.json", b"{ not a registry");
        let data =
            inspect(&fixture.inspect_request()).expect("inspect reports rather than refuses");
        assert_eq!(data.registry.state, RegistryReportState::Invalid);
        let detail = data.registry.detail.expect("the reason is named");
        assert!(detail.contains("registry.json"), "{detail}");
        assert_eq!(data.registry.volumes.count, 0);
        assert_eq!(
            data.registry.path, path,
            "the report names the file it read"
        );
    }

    #[test]
    fn inspect_reports_an_absent_registry_and_absent_observations_without_creating_them() {
        let fixture = Fixture::new("inspect-empty");
        let data = inspect(&fixture.inspect_request()).expect("inspect reads");
        assert!(!data.state.initialized);
        assert_eq!(data.registry.state, RegistryReportState::Absent);
        assert_eq!(data.registry.volumes.count, 0);
        assert_eq!(data.oxide.registration.state, ObservationState::Absent);
        assert!(data.oxide.registration.facts.is_none());
        assert!(data.oxide.registration.observed_volume.is_none());
        assert_eq!(data.oxide.pools.state, ObservationState::Absent);
        assert_eq!(data.oxide.pools.paths.count, 0);
        assert_eq!(data.observations.count, 2);
        assert!(
            !fixture.state_override().exists(),
            "inspect creates no directory"
        );
        assert!(
            !fixture.dir.join("Library").exists(),
            "inspect creates no foreign state either"
        );
    }

    #[test]
    fn inspect_reports_a_valid_registry_its_volumes_and_the_observed_volume() {
        let fixture = Fixture::new("inspect-filled");
        fixture.register_a_volume();
        fixture.observe_oxide();
        let data = inspect(&fixture.inspect_request()).expect("inspect reads");
        assert_eq!(data.registry.state, RegistryReportState::Valid);
        assert_eq!(data.registry.volumes.count, 1);
        assert_eq!(data.registry.volumes.rows[0].volume_uuid, UUID);

        assert_eq!(data.oxide.registration.state, ObservationState::Reported);
        let facts = data
            .oxide
            .registration
            .facts
            .as_ref()
            .expect("the registration facts");
        assert_eq!(facts.mode, Availability::Permanent);
        let observed = data
            .oxide
            .registration
            .observed_volume
            .as_ref()
            .expect("the observed volume");
        assert_eq!(observed.origin, RecordOrigin::ObservedOxide);
        assert_eq!(observed.durability, Durability::Undeclared);
        assert_eq!(observed.availability, Availability::Permanent);

        assert_eq!(data.oxide.pools.state, ObservationState::Reported);
        assert_eq!(data.oxide.pools.paths.count, 1);
        assert_eq!(data.oxide.pools.paths.rows[0], fixture.dir.join("pool"));
        assert_eq!(
            data.oxide.registration.searched.len(),
            3,
            "the recorded pool plus the current and retired defaults"
        );
    }

    #[test]
    fn inspect_reports_an_unreadable_registration_as_unknown_state_naming_the_file() {
        let fixture = Fixture::new("inspect-unknown-registration");
        fixture
            .dir
            .write("pool/oxide-managed/removable.json", b"{ broken");
        let pools =
            serde_json::to_vec(&vec![fixture.dir.join("pool")]).expect("the record encodes");
        fixture
            .dir
            .write("Library/Application Support/oxide/state/pools.json", &pools);
        let data = inspect(&fixture.inspect_request()).expect("inspect reads");
        assert_eq!(
            data.oxide.registration.state,
            ObservationState::UnknownState
        );
        let detail = data.oxide.registration.detail.expect("the reason is named");
        assert!(detail.contains("removable.json"), "{detail}");
        assert!(data.oxide.registration.facts.is_none());
    }

    #[test]
    fn a_describe_document_round_trips_through_its_reader() {
        let fixture = Fixture::new("describe-round-trip");
        fixture.register_a_volume();
        fixture.observe_oxide();
        let data = describe(&fixture.request()).expect("describe reads");
        let read = read_describe_document(&document(&data)).expect("its own document reads");
        assert_eq!(read, data);
    }

    #[test]
    fn an_inspect_document_round_trips_through_its_reader() {
        let fixture = Fixture::new("inspect-round-trip");
        fixture.register_a_volume();
        fixture.observe_oxide();
        let data = inspect(&fixture.inspect_request()).expect("inspect reads");
        let read = read_inspect_document(&inspect_document(&data)).expect("its own document reads");
        assert_eq!(read, data);
    }

    #[test]
    fn both_readers_refuse_an_unknown_major() {
        let fixture = Fixture::new("describe-unknown-major");
        let data = describe(&fixture.request()).expect("describe reads");
        let bytes = String::from_utf8(document(&data)).expect("the document is text");
        let future = bytes.replace("barge.describe/1", "barge.describe/2");
        let error = read_describe_document(future.as_bytes())
            .expect_err("a newer major is refused, never guessed at");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
        assert_eq!(error.detail()["schema"], "barge.describe/2");
        assert_eq!(error.detail()["supported"], "barge.describe/1");

        let data = inspect(&fixture.inspect_request()).expect("inspect reads");
        let bytes = String::from_utf8(inspect_document(&data)).expect("the document is text");
        let future = bytes.replace("barge.inspect/1", "barge.inspect/3");
        let error = read_inspect_document(future.as_bytes()).expect_err("a newer major is refused");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
        assert_eq!(error.detail()["schema"], "barge.inspect/3");
    }

    #[test]
    fn the_readers_refuse_another_document() {
        let bytes = br#"{"schema":"barge.error/1","command":"describe","ok":false,"error":{"kind":"io","message":"x","detail":{}}}"#;
        assert_eq!(
            read_describe_document(bytes)
                .expect_err("another schema")
                .kind(),
            ErrorKind::UnknownState
        );
        assert_eq!(
            read_inspect_document(bytes)
                .expect_err("another schema")
                .kind(),
            ErrorKind::UnknownState
        );
    }

    #[test]
    fn the_readers_refuse_a_removed_field_and_a_changed_type() {
        let fixture = Fixture::new("describe-strictness");
        let data = describe(&fixture.request()).expect("describe reads");
        let mut value: serde_json::Value =
            serde_json::from_slice(&document(&data)).expect("the document parses");
        value["data"]
            .as_object_mut()
            .expect("data is an object")
            .remove("state");
        let error = read_describe_document(&serde_json::to_vec(&value).expect("encodes"))
            .expect_err("a missing field is refused");
        assert_eq!(error.kind(), ErrorKind::UnknownState);

        let mut value: serde_json::Value =
            serde_json::from_slice(&document(&data)).expect("the document parses");
        value["data"]["state"]["initialized"] = serde_json::Value::String("no".to_owned());
        let error = read_describe_document(&serde_json::to_vec(&value).expect("encodes"))
            .expect_err("a changed type is refused");
        assert_eq!(error.kind(), ErrorKind::UnknownState);

        let inspect = inspect(&fixture.inspect_request()).expect("inspect reads");
        let mut value: serde_json::Value =
            serde_json::from_slice(&inspect_document(&inspect)).expect("the document parses");
        value["data"]["registry"]
            .as_object_mut()
            .expect("the registry is an object")
            .remove("volumes");
        let error = read_inspect_document(&serde_json::to_vec(&value).expect("encodes"))
            .expect_err("a missing required field is refused");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
    }

    #[test]
    fn the_readers_accept_an_optional_field_added_inside_the_major() {
        let fixture = Fixture::new("describe-optional");
        let data = describe(&fixture.request()).expect("describe reads");
        let mut value: serde_json::Value =
            serde_json::from_slice(&document(&data)).expect("the document parses");
        value["data"]["a_later_optional_field"] = serde_json::Value::Bool(true);
        read_describe_document(&serde_json::to_vec(&value).expect("encodes"))
            .expect("an optional addition stays readable");

        let inspect = inspect(&fixture.inspect_request()).expect("inspect reads");
        let mut value: serde_json::Value =
            serde_json::from_slice(&inspect_document(&inspect)).expect("the document parses");
        value["data"]["registry"]["a_later_optional_field"] = serde_json::Value::Bool(true);
        read_inspect_document(&serde_json::to_vec(&value).expect("encodes"))
            .expect("an optional addition stays readable");
    }

    #[test]
    fn the_readers_refuse_an_oversized_document_and_a_non_document() {
        let bytes = vec![b' '; crate::contract::DOCUMENT_SIZE_CAP_BYTES + 1];
        assert_eq!(
            read_describe_document(&bytes)
                .expect_err("an oversized document is refused")
                .kind(),
            ErrorKind::ResourceBound
        );
        assert_eq!(
            read_inspect_document(&bytes)
                .expect_err("an oversized document is refused")
                .kind(),
            ErrorKind::ResourceBound
        );
        assert_eq!(
            read_describe_document(b"not json")
                .expect_err("a non-document is refused")
                .kind(),
            ErrorKind::UnknownState
        );
        assert_eq!(
            read_inspect_document(b"not json")
                .expect_err("a non-document is refused")
                .kind(),
            ErrorKind::UnknownState
        );
    }

    #[test]
    fn a_truncated_list_round_trips_and_names_its_counts() {
        let fixture = Fixture::new("describe-truncated");
        let mut data = describe(&fixture.request()).expect("describe reads");
        data.observations = RowSet::bounded(
            vec![ObservationRow {
                source: "oxide.removable".to_owned(),
                version: "2".to_owned(),
                path: PathBuf::from("/fixture/oxide-managed/removable.json"),
                state: ObservationState::Reported,
                detail: None,
            }],
            3,
            1,
        );
        let read = read_describe_document(&document(&data)).expect("a truncated list reads");
        assert_eq!(read.observations.count, 1);
        assert_eq!(read.observations.total, 3);
        assert!(read.observations.truncated);
        assert_eq!(read.observations.rows[0].state, ObservationState::Reported);
    }

    #[test]
    fn a_document_whose_counts_disagree_with_its_rows_is_refused() {
        let fixture = Fixture::new("describe-counts");
        let mut data = describe(&fixture.request()).expect("describe reads");
        data.volumes.total = 4;
        let value = serde_json::to_value(&data).expect("the data encodes");
        let bytes = serde_json::to_vec(&serde_json::json!({
            "schema": "barge.describe/1",
            "command": "describe",
            "ok": true,
            "data": value,
        }))
        .expect("the document encodes");
        let error = read_describe_document(&bytes).expect_err("inconsistent counts are refused");
        assert_eq!(error.kind(), ErrorKind::UnknownState);

        let inspect = inspect(&fixture.inspect_request()).expect("inspect reads");
        let mut value = serde_json::to_value(&inspect).expect("the data encodes");
        value["oxide"]["pools"]["paths"]["count"] = serde_json::json!(9);
        let bytes = serde_json::to_vec(&serde_json::json!({
            "schema": "barge.inspect/1",
            "command": "inspect",
            "ok": true,
            "data": value,
        }))
        .expect("the document encodes");
        let error = read_inspect_document(&bytes).expect_err("inconsistent counts are refused");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
    }

    #[test]
    fn an_account_home_failure_is_carried_as_its_kind() {
        struct FailingAccount;

        impl AccountFacts for FailingAccount {
            fn account_home(&self) -> Result<PathBuf> {
                Err(Error::new(
                    ErrorKind::UnknownState,
                    "the password database has no entry",
                ))
            }

            fn state_root(&self) -> Result<PathBuf> {
                self.account_home()
            }

            fn tool_state_dir(&self, _tool: &str) -> Result<PathBuf> {
                self.account_home()
            }

            fn tool_cache_dir(&self, _tool: &str) -> Result<PathBuf> {
                self.account_home()
            }
        }

        let platform = crate::os::platform();
        let error = describe(&DescribeRequest {
            account: &FailingAccount,
            environment_home: None,
            state_dir_override: None,
            tool_version: "0.1.0",
            dirs: &platform,
            facts: &platform,
            mounts: &platform,
            space: None,
            observer: &platform,
        })
        .expect_err("an unreadable account home refuses");
        assert_eq!(error.kind(), ErrorKind::UnknownState);

        let error = inspect(&InspectRequest {
            account: &FailingAccount,
            environment_home: None,
            state_dir_override: None,
            tool_version: "0.1.0",
        })
        .expect_err("an unreadable account home refuses");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
    }

    #[test]
    fn the_row_shapes_use_the_contract_names() {
        let row = super::PlacementRow {
            kind: PlacementKind::CargoRegistry,
            original_path: PathBuf::from("/Users/fixture/.cargo/registry"),
            volume_uuid: "A1B2C3D4-E5F6-4A7B-8C9D-0E1F2A3B4C5D".to_owned(),
            recorded_path: PathBuf::from("/fixture/caches/cargo/registry"),
            configured_path: PathBuf::from("/fixture/caches/cargo/registry"),
            link: LinkState::Linked,
            absent: AbsentPolicy::Fail,
        };
        let value = serde_json::to_value(&row).expect("the row encodes");
        assert_eq!(value["kind"], "cargo_registry");
        assert_eq!(value["link"], "linked");
        assert_eq!(value["absent"], "fail");

        let volume = crate::registry::VolumeRecord {
            volume_uuid: crate::os::VolumeUuid::parse(UUID).expect("a UUID"),
            mount_hint: Some(PathBuf::from("/Volumes/Builds")),
            availability: Availability::Ephemeral,
            durability: Durability::Kept,
            root_dir: "barge".to_owned(),
            folders: FolderOverrides {
                scratch: Some("temp".to_owned()),
                caches: None,
            },
            origin: RecordOrigin::ObservedOxide,
            scratch_grants: ScratchGrants::none(),
            space: None,
            generation: 1,
            registered_ms: 0,
        };
        let value =
            serde_json::to_value(super::VolumeRow::from_record(&volume)).expect("the row encodes");
        assert_eq!(value["availability"], "ephemeral");
        assert_eq!(value["durability"], "kept");
        assert_eq!(value["origin"], "observed_oxide");
        assert_eq!(value["folders"]["scratch"], "temp");
    }

    #[test]
    fn a_state_directory_that_is_not_a_directory_is_unknown_state() {
        let fixture = Fixture::new("describe-not-a-directory");
        std::fs::write(fixture.state_override(), b"a file").expect("the fixture file is written");
        let error = describe(&fixture.request()).expect_err("a file is not a state directory");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
        let error = inspect(&fixture.inspect_request()).expect_err("nor for inspect");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
    }

    #[test]
    fn a_relative_state_override_is_refused_by_both_commands() {
        let fixture = Fixture::new("describe-relative-override");
        let error = describe(&DescribeRequest {
            account: &fixture.account,
            environment_home: None,
            state_dir_override: Some(Path::new("relative/state")),
            tool_version: "0.1.0",
            dirs: &fixture.platform,
            facts: &fixture.platform,
            mounts: &fixture.platform,
            space: None,
            observer: &fixture.platform,
        })
        .expect_err("a relative override is refused");
        assert_eq!(error.kind(), ErrorKind::Usage);
        assert_eq!(
            error.detail()["state_dir"],
            "relative/state",
            "the refusal names the path"
        );
        assert!(inspect(&InspectRequest {
            account: &fixture.account,
            environment_home: None,
            state_dir_override: Some(Path::new("relative/state")),
            tool_version: "0.1.0",
        })
        .is_err());
    }
    /// Injected volume facts for one fixture directory standing in for a mounted
    /// volume, so `describe` can walk an area without a real drive.
    struct FixtureVolume {
        root: PathBuf,
        uuid: crate::os::VolumeUuid,
        device: crate::os::DeviceId,
    }

    impl crate::os::VolumeFacts for FixtureVolume {
        fn mount_census(&self, uuid: &crate::os::VolumeUuid) -> Result<Vec<PathBuf>> {
            Ok(if uuid == &self.uuid {
                vec![self.root.clone()]
            } else {
                Vec::new()
            })
        }
        fn identify(&self, _path: &Path) -> Result<crate::os::VolumeIdentity> {
            Ok(crate::os::VolumeIdentity {
                mount_point: self.root.clone(),
                filesystem: crate::os::FileSystem::Apfs,
                device: self.device,
                volume_uuid: Some(self.uuid.clone()),
                boot_volume: false,
            })
        }
    }

    impl crate::os::MountRoot for FixtureVolume {
        fn mount_root(&self, path: &Path) -> Result<crate::os::MountPoint> {
            if path != self.root {
                return Err(Error::new(
                    ErrorKind::VolumeMismatch,
                    "not the fixture mount root",
                ));
            }
            Ok(crate::os::MountPoint::new(self.root.clone()))
        }
    }

    impl crate::os::ProcessObserver for FixtureVolume {
        fn effective_uid(&self) -> Result<u32> {
            crate::os::platform().effective_uid()
        }

        fn boot_identity(&self) -> Result<String> {
            Ok("1700000000.0".to_owned())
        }

        fn process_start(&self, _pid: u32) -> Result<Option<u64>> {
            Ok(None)
        }

        fn verify_platform_binary(
            &self,
            _classified: crate::os::ProcessIdentity,
        ) -> Result<crate::os::PlatformStatus> {
            Ok(crate::os::PlatformStatus::NotVerified {
                reason: "this observer reports no platform binary".to_owned(),
            })
        }

        fn scan_open_descriptors(
            &self,
            _request: &crate::os::DescriptorScanRequest<'_>,
        ) -> Result<crate::os::DescriptorScan> {
            Ok(crate::os::DescriptorScan::Complete {
                processes: 0,
                descriptors: 0,
                foreign: 0,
                retry: crate::os::RetryEvidence::default(),
                os_blind_spot: crate::os::BlindSpotEvidence::default(),
                dead_handles: crate::os::DeadHandleEvidence::default(),
                open: Vec::new(),
            })
        }
    }

    #[test]
    fn describe_reports_each_roots_recorded_and_configured_path() {
        let fixture = Fixture::new("describe-roots");
        let volume_dir = fixture.dir.join("volume");
        std::fs::create_dir(&volume_dir).expect("the fixture volume root is created");
        let platform = &fixture.platform;
        let device = platform
            .open_path(&volume_dir)
            .expect("the fixture volume opens")
            .identity()
            .device;
        let uuid = crate::os::VolumeUuid::parse(UUID).expect("a UUID");
        let record = VolumeRecord {
            volume_uuid: uuid.clone(),
            mount_hint: Some(volume_dir.clone()),
            availability: Availability::Permanent,
            durability: Durability::Expendable,
            root_dir: String::new(),
            folders: FolderOverrides::none(),
            origin: RecordOrigin::Declared,
            scratch_grants: ScratchGrants::none(),
            space: None,
            generation: 1,
            registered_ms: 1_792_525_019_847,
        };
        let state = fixture.state();
        state.initialize().expect("the layout exists");
        {
            let mut locks =
                LockSet::with_deadline(state.clone(), Deadline::after(Duration::from_secs(5)));
            let lock = locks.lock_registry().expect("the registry lock");
            RegistryWriter::new(&state)
                .publish(&lock, &RegistryDocument::new(vec![record.clone()]))
                .expect("the volume is registered");
        }
        let volume = FixtureVolume {
            root: volume_dir.clone(),
            uuid,
            device,
        };
        let declaration = crate::area::AreaDeclaration {
            namespace: "example".to_owned(),
            area: "pool".to_owned(),
            folder: "example/pool".to_owned(),
            overrides: std::collections::BTreeMap::new(),
            absent: crate::area::AbsentPolicy::Fail {
                attach: "the build volume".to_owned(),
            },
            prune: crate::area::PrunePolicy::None,
            reserve: crate::area::ReserveParticipation::Counts,
        };
        crate::area::declare(&state, &declaration, 1_000).expect("the area is declared");
        let resolved = crate::area::resolve(
            platform,
            &volume,
            &volume,
            &volume,
            &record,
            &declaration,
            None,
        )
        .expect("the area resolves");
        let root = crate::area::create_root(
            &state,
            platform,
            &volume,
            &resolved,
            crate::area::RootSpec {
                kind: crate::area::RootKind::AreaRoot,
            },
        )
        .expect("the root is created");

        let request = DescribeRequest {
            account: &fixture.account,
            environment_home: None,
            state_dir_override: Some(fixture.state_override()),
            tool_version: "0.1.0",
            dirs: platform,
            facts: &volume,
            mounts: &volume,
            space: None,
            observer: &volume,
        };
        let data = describe(&request).expect("describe reads");
        assert_eq!(
            data.identity.capability,
            "volume_uuid_root_id_nonce_inode_relative_path"
        );
        assert_eq!(
            data.volumes.rows[0].mounts,
            Some(super::MountCensus::Complete {
                count: 1,
                paths: vec![volume_dir.clone()]
            })
        );
        assert_eq!(data.areas.count, 1, "the declared area is reported");
        assert_eq!(data.roots.count, 1, "the owned root is reported");
        let row = &data.roots.rows[0];
        assert_eq!(row.state, crate::area::MarkerStateName::Owned);
        assert_eq!(row.recorded_path.as_deref(), Some(root.path.as_path()));
        assert_eq!(row.configured_path, root.path);
        assert_eq!(row.root_id.as_deref(), Some(root.root_id.as_str()));

        // Changing root_dir moves nothing: the volume row reports the new value,
        // the old root is still there, and the new configuration finds no root.
        let mut moved = record.clone();
        moved.root_dir = "managed".to_owned();
        let mut locks =
            LockSet::with_deadline(state.clone(), Deadline::after(Duration::from_secs(5)));
        let lock = locks.lock_registry().expect("the registry lock");
        RegistryWriter::new(&state)
            .publish(&lock, &RegistryDocument::new(vec![moved]))
            .expect("the registry is rewritten");
        drop(lock);
        let data = describe(&request).expect("describe reads");
        assert_eq!(data.volumes.rows[0].root_dir, "managed");
        assert_eq!(data.roots.count, 0, "nothing is reported at the new path");
        assert!(
            root.path.join(crate::area::ROOT_MARKER_FILE).is_file(),
            "the root stays where it was created"
        );
    }

    #[test]
    fn a_describe_document_without_roots_still_reads() {
        let fixture = Fixture::new("describe-roots-compatible");
        let data = describe(&fixture.request()).expect("describe reads");
        let bytes = document(&data);
        let mut value: serde_json::Value = serde_json::from_slice(&bytes).expect("the document");
        let data_object = value["data"]
            .as_object_mut()
            .expect("the data is an object");
        data_object.remove("roots");
        let mut without = serde_json::to_vec(&value).expect("the document encodes");
        without.push(b'\n');
        let read = read_describe_document(&without).expect("an older document reads");
        assert_eq!(read.roots.count, 0, "a missing optional list is empty");

        // The same holds for the custody object a later S5 document adds.
        let mut value: serde_json::Value = serde_json::from_slice(&bytes).expect("the document");
        value["data"]
            .as_object_mut()
            .expect("the data is an object")
            .remove("custody");
        let mut without = serde_json::to_vec(&value).expect("the document encodes");
        without.push(b'\n');
        let read = read_describe_document(&without).expect("an older document reads");
        assert_eq!(read.custody.capability, crate::custody::CAPABILITY);
    }
}
