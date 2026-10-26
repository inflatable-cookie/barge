//! The versioned documents the library and CLI return, starting with
//! `barge.describe/1` (stage 1 design, "Versioned JSON and error contract").
//!
//! `describe` reports what Barge knows and nothing else: it resolves the account
//! state directory, says whether it is initialized, and reports every registered
//! volume, declared area, placement and foreign observation. It performs no
//! mutation and takes no lock. Slice S1 has no registry yet, so the four lists
//! are empty and carry the row-cap shape a later slice fills; the document's
//! reader is already strict, so a consumer written against it keeps working
//! when rows arrive.
//!
//! The reader refuses an unknown major, a document over the size cap, an
//! unknown command and any missing or mistyped required field. An extra field is
//! accepted, because within a major version a new optional field is a compatible
//! addition — reading is strict about what it requires, not about what it may
//! ignore.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::contract::{
    parse_document, FormatVersions, RowSet, DESCRIBE_SCHEMA, DESCRIBE_SCHEMA_PREFIX,
};
use crate::error::{Error, ErrorKind, Result};
use crate::os::AccountFacts;

/// The command name this document belongs to.
pub const DESCRIBE_COMMAND: &str = "describe";

/// The tool name a document reports.
pub const TOOL_NAME: &str = "barge";

/// What `describe` reads. The caller supplies the platform's account facts and
/// the environment it was given; nothing is created, locked or written.
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
    /// Foreign observations, bounded to the observation row cap.
    pub observations: RowSet<ObservationRow>,
}

/// The tool that produced a document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolVersions {
    /// `barge`.
    pub name: String,
    /// The build's version.
    pub version: String,
}

/// The account state directory, as `describe` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateReport {
    /// The state directory path, as resolved. It is not canonicalised: the path
    /// may not exist yet, and `describe` creates nothing.
    pub dir: PathBuf,
    /// Whether the state directory exists as a directory. S1 never creates it.
    pub initialized: bool,
    /// Where the path came from.
    pub source: StateDirSource,
    /// The account home and the environment's own, so a mismatch is visible.
    pub home: HomeReport,
}

/// Where the state directory path came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StateDirSource {
    /// The platform's account state root.
    Account,
    /// `BARGE_STATE_DIR`, which exists for fixtures and proofs only.
    EnvironmentOverride,
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

/// One registered volume, as `describe` reports it (stage 1 design, "Registry of
/// volumes"). Slice S2 fills these rows; S1 reports an empty list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeRow {
    /// The APFS volume UUID from the kernel: the identity that matters.
    pub volume_uuid: String,
    /// The last observed mount point; used to find the volume, never as
    /// identity.
    pub mount_hint: Option<PathBuf>,
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

/// A volume's availability (stage 1 design, "Volume modes").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Availability {
    /// Always attached; absence is abnormal.
    Permanent,
    /// Attached on and off; absence is normal.
    Ephemeral,
}

/// A volume's durability (stage 1 design, "Volume modes").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Durability {
    /// Rebuildable data Barge may prune.
    Expendable,
    /// Data that lives only on that volume and is never pruned.
    Kept,
    /// An imported record that does not say; never treated as expendable.
    Undeclared,
}

/// Barge's own folder overrides for one volume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderOverrides {
    /// The per-volume `scratch/` override, relative to `root_dir`.
    pub scratch: Option<String>,
    /// The per-volume `caches/` override, relative to `root_dir`.
    pub caches: Option<String>,
}

/// Where a volume record came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordOrigin {
    /// An operator declared it.
    Declared,
    /// Imported from Oxide's registration, read-only.
    ObservedOxide,
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

/// One foreign observation: someone else's state, read and reported, never
/// authoritative (stage 1 design, "Read-only inspection versus authoritative
/// writes"). Slice S2 fills these rows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservationRow {
    /// The source family, for example `oxide.removable`.
    pub source: String,
    /// The version the source states.
    pub version: String,
    /// The file that was read.
    pub path: PathBuf,
    /// Whether the observation was reported or is unknown state.
    pub state: ObservationState,
    /// Why the observation is unknown, when it is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// How a foreign observation came out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationState {
    /// Read and valid for its stated version.
    Reported,
    /// Unreadable, oversized, unknown-version or schema-divergent; never a
    /// default.
    UnknownState,
}

/// Build the `barge.describe/1` data.
///
/// No file, directory or lock is created: the state directory is inspected with
/// one metadata read, and every list is empty until a later slice has records to
/// report.
pub fn describe(request: &DescribeRequest<'_>) -> Result<DescribeData> {
    let account_home = request.account.account_home()?;
    let (state_dir, source) = match request.state_dir_override {
        Some(path) => (path.to_path_buf(), StateDirSource::EnvironmentOverride),
        None => (request.account.state_root()?, StateDirSource::Account),
    };
    let initialized = state_directory_is_initialized(&state_dir)?;
    let environment_home = request.environment_home.map(Path::to_path_buf);
    let mismatch = environment_home
        .as_ref()
        .is_some_and(|home| home != &account_home);

    Ok(DescribeData {
        tool: ToolVersions {
            name: TOOL_NAME.to_owned(),
            version: request.tool_version.to_owned(),
        },
        formats: FormatVersions::current(),
        state: StateReport {
            dir: state_dir,
            initialized,
            source,
            home: HomeReport {
                account: account_home,
                environment: environment_home,
                mismatch,
            },
        },
        volumes: RowSet::empty(),
        areas: RowSet::empty(),
        placements: RowSet::empty(),
        observations: RowSet::empty(),
    })
}

/// Whether the state directory exists as a directory. An unreadable path or a
/// non-directory at that path is unknown state, never "not initialized".
fn state_directory_is_initialized(path: &Path) -> Result<bool> {
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_dir() => Ok(true),
        Ok(_) => Err(unknown(format!(
            "the state directory path {} is not a directory",
            path.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(unknown(format!(
            "the state directory {} could not be read: {error}",
            path.display()
        ))),
    }
}

/// Read one `barge.describe/1` document.
///
/// A reader refuses an unknown major, a document over the size cap, an unknown
/// command and any missing or mistyped required field, and refuses a list whose
/// counts disagree with its rows. It accepts an extra field, which is how a new
/// optional field lands inside a major version.
pub fn read_describe_document(bytes: &[u8]) -> Result<DescribeData> {
    let document = parse_document(bytes)?;

    let schema = document
        .get("schema")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| unknown("the document has no schema string"))?;
    if schema != DESCRIBE_SCHEMA {
        let message = if schema.starts_with(DESCRIBE_SCHEMA_PREFIX) {
            format!("`{schema}` is an unknown major; this reader knows {DESCRIBE_SCHEMA}")
        } else {
            format!("`{schema}` is not a {DESCRIBE_SCHEMA_PREFIX} document")
        };
        return Err(unknown(message)
            .with_detail("schema", schema.into())
            .with_detail("supported", DESCRIBE_SCHEMA.into()));
    }

    if document.get("command").and_then(serde_json::Value::as_str) != Some(DESCRIBE_COMMAND) {
        return Err(unknown(format!(
            "the document is not a `{DESCRIBE_COMMAND}` document"
        )));
    }
    if document.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
        return Err(unknown("the document is not a successful command document"));
    }

    let data = document
        .get("data")
        .cloned()
        .ok_or_else(|| unknown("the document has no data"))?;
    let data: DescribeData = serde_json::from_value(data).map_err(|error| {
        unknown(format!(
            "the document does not provide the field set {DESCRIBE_SCHEMA} requires: {error}"
        ))
    })?;
    data.volumes.validate()?;
    data.areas.validate()?;
    data.placements.validate()?;
    data.observations.validate()?;
    Ok(data)
}

fn unknown(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::UnknownState, message)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::path::{Path, PathBuf};

    use super::{
        describe, read_describe_document, AbsentPolicy, DescribeData, DescribeRequest, LinkState,
        ObservationRow, ObservationState, PlacementKind, StateDirSource,
    };
    use crate::contract::{success_bytes, RowSet, DESCRIBE_SCHEMA};
    use crate::error::{Error, ErrorKind, Result};
    use crate::os::AccountFacts;

    /// Injected account facts: the account home is a fixture, so no test reads
    /// or writes a real account's state.
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
    }

    /// A path that does not exist, in the test process's temp directory. Nothing
    /// creates it: `describe` must not.
    fn absent_state_dir() -> PathBuf {
        std::env::temp_dir()
            .join(format!("barge-describe-{}", std::process::id()))
            .join("state")
    }

    fn account() -> FakeAccount {
        FakeAccount {
            home: PathBuf::from("/Users/fixture"),
        }
    }

    fn request<'a>(
        account: &'a FakeAccount,
        state_dir_override: Option<&'a Path>,
    ) -> DescribeRequest<'a> {
        DescribeRequest {
            account,
            environment_home: None,
            state_dir_override,
            tool_version: "0.1.0",
        }
    }

    #[test]
    fn an_uninitialized_state_directory_is_reported_and_not_created() {
        let account = account();
        let state_dir = absent_state_dir();
        assert!(!state_dir.exists(), "the fixture path starts absent");
        let data = describe(&request(&account, Some(&state_dir))).expect("describe reads");
        assert!(!data.state.initialized);
        assert_eq!(data.state.dir, state_dir);
        assert_eq!(data.state.source, StateDirSource::EnvironmentOverride);
        assert!(
            !state_dir.exists(),
            "describe must not create the state directory"
        );
    }

    #[test]
    fn the_account_state_root_is_reported_when_no_override_is_given() {
        let account = account();
        let data = describe(&request(&account, None)).expect("describe reads");
        assert_eq!(
            data.state.dir,
            PathBuf::from("/Users/fixture/Library/Application Support/barge/state")
        );
        assert_eq!(data.state.source, StateDirSource::Account);
        assert!(!data.state.initialized);
    }

    #[test]
    fn a_environment_home_that_differs_is_reported() {
        let account = account();
        let environment = PathBuf::from("/tmp/other-home");
        let data = describe(&DescribeRequest {
            account: &account,
            environment_home: Some(&environment),
            state_dir_override: Some(&absent_state_dir()),
            tool_version: "0.1.0",
        })
        .expect("describe reads");
        assert!(data.state.home.mismatch);
        assert_eq!(data.state.home.environment, Some(environment));

        let same = describe(&DescribeRequest {
            account: &account,
            environment_home: Some(Path::new("/Users/fixture")),
            state_dir_override: Some(&absent_state_dir()),
            tool_version: "0.1.0",
        })
        .expect("describe reads");
        assert!(!same.state.home.mismatch);
    }

    #[test]
    fn slice_one_reports_empty_lists_with_the_row_cap_shape() {
        let account = account();
        let data = describe(&request(&account, Some(&absent_state_dir()))).expect("describe reads");
        for rows in [
            (&data.volumes.count, &data.volumes.total),
            (&data.areas.count, &data.areas.total),
            (&data.placements.count, &data.placements.total),
            (&data.observations.count, &data.observations.total),
        ] {
            assert_eq!(*rows.0, 0);
            assert_eq!(*rows.1, 0);
        }
        assert!(!data.volumes.truncated);
        assert!(!data.areas.truncated);
        assert!(!data.placements.truncated);
        assert!(!data.observations.truncated);
    }

    fn document(data: &DescribeData) -> Vec<u8> {
        success_bytes(DESCRIBE_SCHEMA, super::DESCRIBE_COMMAND, data).expect("the document encodes")
    }

    #[test]
    fn a_document_round_trips_through_its_reader() {
        let account = account();
        let data = describe(&request(&account, Some(&absent_state_dir()))).expect("describe reads");
        let read = read_describe_document(&document(&data)).expect("its own document reads");
        assert_eq!(read, data);
    }

    #[test]
    fn the_reader_refuses_an_unknown_major() {
        let account = account();
        let data = describe(&request(&account, Some(&absent_state_dir()))).expect("describe reads");
        let bytes = String::from_utf8(document(&data)).expect("the document is text");
        let future = bytes.replace("barge.describe/1", "barge.describe/2");
        let error = read_describe_document(future.as_bytes())
            .expect_err("a newer major is refused, never guessed at");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
        assert_eq!(error.detail()["schema"], "barge.describe/2");
        assert_eq!(error.detail()["supported"], "barge.describe/1");
    }

    #[test]
    fn the_reader_refuses_another_document() {
        let bytes = br#"{"schema":"barge.error/1","command":"describe","ok":false,"error":{"kind":"io","message":"x","detail":{}}}"#;
        let error = read_describe_document(bytes).expect_err("another schema is refused");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
    }

    #[test]
    fn the_reader_refuses_a_removed_required_field() {
        let account = account();
        let data = describe(&request(&account, Some(&absent_state_dir()))).expect("describe reads");
        let mut value: serde_json::Value =
            serde_json::from_slice(&document(&data)).expect("the document parses");
        value["data"]
            .as_object_mut()
            .expect("data is an object")
            .remove("state");
        let bytes = serde_json::to_vec(&value).expect("the value encodes");
        let error = read_describe_document(&bytes).expect_err("a missing field is refused");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
    }

    #[test]
    fn the_reader_refuses_a_changed_type() {
        let account = account();
        let data = describe(&request(&account, Some(&absent_state_dir()))).expect("describe reads");
        let mut value: serde_json::Value =
            serde_json::from_slice(&document(&data)).expect("the document parses");
        value["data"]["state"]["initialized"] = serde_json::Value::String("no".to_owned());
        let bytes = serde_json::to_vec(&value).expect("the value encodes");
        let error = read_describe_document(&bytes).expect_err("a changed type is refused");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
    }

    #[test]
    fn the_reader_accepts_an_optional_field_added_inside_the_major() {
        let account = account();
        let data = describe(&request(&account, Some(&absent_state_dir()))).expect("describe reads");
        let mut value: serde_json::Value =
            serde_json::from_slice(&document(&data)).expect("the document parses");
        value["data"]["a_later_optional_field"] = serde_json::Value::Bool(true);
        let bytes = serde_json::to_vec(&value).expect("the value encodes");
        read_describe_document(&bytes).expect("an optional addition stays readable");
    }

    #[test]
    fn the_reader_refuses_an_oversized_document() {
        let bytes = vec![b' '; crate::contract::DOCUMENT_SIZE_CAP_BYTES + 1];
        let error = read_describe_document(&bytes).expect_err("an oversized document is refused");
        assert_eq!(error.kind(), ErrorKind::ResourceBound);
    }

    #[test]
    fn the_reader_refuses_a_document_that_is_not_json() {
        let error = read_describe_document(b"not json").expect_err("a non-document is refused");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
    }

    #[test]
    fn a_truncated_list_round_trips_and_names_its_counts() {
        let account = account();
        let mut data =
            describe(&request(&account, Some(&absent_state_dir()))).expect("describe reads");
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
    fn the_reader_refuses_a_list_whose_counts_disagree_with_its_rows() {
        let account = account();
        let mut data =
            describe(&request(&account, Some(&absent_state_dir()))).expect("describe reads");
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
        }

        let error = describe(&DescribeRequest {
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
    }
}
