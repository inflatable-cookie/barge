//! Deletion units: the thing a plan names, custody proves and the delete
//! journal records (stage 1 design, "Writer custody and safe deletion" and
//! "Whole-unit deletion").
//!
//! S5 first knew one shape of deletable thing: a directory carrying a Barge
//! marker, directly inside an area folder. S4's scratch sweep also meets a
//! delegated Effigy or Oxide run (a directory with that family's marker), a
//! claimed unmarked directory, and a claimed top-level file. This module is the
//! one description of all four, so custody and deletion have a single input and
//! a single journal shape:
//!
//! - the **parent**: a relative path inside one area folder, `""` for a direct
//!   child of it, addressed descriptor-relative and never re-resolved from a
//!   string once opened;
//! - the **unit name**: one entry name inside that parent;
//! - the **kind**: `directory` or `file`;
//! - the **expected identity** the plan recorded: device, inode and kind;
//! - an **optional marker**: its filename and family (`barge`, `effigy_scratch`,
//!   `oxide_scratch`), plus the SHA-256 of the bytes the plan read. A file never
//!   carries one, and a Barge-owned root is exactly a directory unit whose
//!   marker is `.barge-root.json` of the `barge` family;
//! - the **grant** that authorises the deletion (native root, foreign-family
//!   delegation, or legacy claim), which is the design's rule that no delete
//!   path skips a grant; and
//! - the **plan digest** the effect was authorised by.
//!
//! Custody re-reads every one of those facts and refuses unless they still
//! match, so this type is the caller's evidence, never the unit's truth. The
//! custody evidence itself is produced by [`crate::custody::take_custody`] and
//! is deliberately not a field here: a caller cannot pass a verdict, so a
//! boolean or a forged `Safe` cannot reach the effect.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::area;
use crate::error::{Error, ErrorKind, Result};
use crate::os::{DeviceId, DirHandle, DirOps, EntryKind, VolumeUuid};
use crate::registry;

/// Barge's own marker filename, which is what makes a directory a Barge root.
pub const BARGE_MARKER_FILE: &str = area::ROOT_MARKER_FILE;

/// The Effigy scratch marker's pinned filename.
pub const EFFIGY_SCRATCH_MARKER_FILE: &str = ".effigy-scratch.json";

/// The Oxide scratch marker's pinned filename.
pub const OXIDE_SCRATCH_MARKER_FILE: &str = ".oxide-scratch.json";

/// Every marker filename Barge recognises, in a fixed order.
pub const RECOGNISED_MARKER_FILES: &[&str] = &[
    BARGE_MARKER_FILE,
    EFFIGY_SCRATCH_MARKER_FILE,
    OXIDE_SCRATCH_MARKER_FILE,
];

/// How many parent components one unit may have.
///
/// The unit lives inside one area folder; the bound keeps a recorded parent from
/// describing a walk the deletion's own depth bound would stop.
pub const UNIT_PARENT_COMPONENTS: usize = 8;

/// What a unit is: a directory, deleted whole, or one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnitKind {
    /// A directory, deleted with everything inside it.
    Directory,
    /// One regular file.
    File,
}

impl UnitKind {
    /// The kind's name, as a record and a report spell it.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Directory => "directory",
            Self::File => "file",
        }
    }

    /// The platform entry kind this unit must be.
    pub const fn entry_kind(self) -> EntryKind {
        match self {
            Self::Directory => EntryKind::Directory,
            Self::File => EntryKind::File,
        }
    }

    /// The unit kind an observed entry is, or `None` for a link or anything
    /// else, which is never a unit.
    pub const fn from_entry_kind(kind: EntryKind) -> Option<Self> {
        match kind {
            EntryKind::Directory => Some(Self::Directory),
            EntryKind::File => Some(Self::File),
            EntryKind::Symlink | EntryKind::Other => None,
        }
    }

    /// Parse a recorded kind name strictly.
    pub fn parse(kind: &str) -> Option<Self> {
        match kind {
            "directory" => Some(Self::Directory),
            "file" => Some(Self::File),
            _ => None,
        }
    }
}

/// The marker family a unit's marker belongs to.
///
/// `barge` is Barge's own root marker. `effigy_scratch` and `oxide_scratch` are
/// the two foreign families the design pins: Barge reads their marker, and
/// deletes such a unit only under that family's recorded delegation. There is no
/// "none" variant: a unit with no marker is exactly `marker: None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MarkerFamily {
    /// `.barge-root.json`, written by Barge at allocation.
    Barge,
    /// `.effigy-scratch.json`, concluded by Effigy only.
    EffigyScratch,
    /// `.oxide-scratch.json`, concluded by Oxide's supervisor only.
    OxideScratch,
}

impl MarkerFamily {
    /// The family's name, as a record and a grant spell it.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Barge => "barge",
            Self::EffigyScratch => "effigy_scratch",
            Self::OxideScratch => "oxide_scratch",
        }
    }

    /// The one marker filename this family's marker is read from.
    pub const fn marker_file(self) -> &'static str {
        match self {
            Self::Barge => BARGE_MARKER_FILE,
            Self::EffigyScratch => EFFIGY_SCRATCH_MARKER_FILE,
            Self::OxideScratch => OXIDE_SCRATCH_MARKER_FILE,
        }
    }

    /// The family a recognised marker filename belongs to.
    pub fn from_marker_file(name: &str) -> Option<Self> {
        match name {
            BARGE_MARKER_FILE => Some(Self::Barge),
            EFFIGY_SCRATCH_MARKER_FILE => Some(Self::EffigyScratch),
            OXIDE_SCRATCH_MARKER_FILE => Some(Self::OxideScratch),
            _ => None,
        }
    }

    /// The recorded delegation family this family needs, if it needs one.
    ///
    /// Barge's own family needs no foreign delegation: the marker itself is the
    /// native grant. A foreign family is deletable only through that family's
    /// recorded delegation in the volume's `scratch_grants`.
    pub const fn delegated(self) -> Option<registry::MarkerFamily> {
        match self {
            Self::Barge => None,
            Self::EffigyScratch => Some(registry::MarkerFamily::EffigyScratch),
            Self::OxideScratch => Some(registry::MarkerFamily::OxideScratch),
        }
    }

    /// Parse a recorded family name strictly.
    pub fn parse(family: &str) -> Option<Self> {
        match family {
            "barge" => Some(Self::Barge),
            "effigy_scratch" => Some(Self::EffigyScratch),
            "oxide_scratch" => Some(Self::OxideScratch),
            _ => None,
        }
    }
}

impl From<registry::MarkerFamily> for MarkerFamily {
    fn from(family: registry::MarkerFamily) -> Self {
        match family {
            registry::MarkerFamily::EffigyScratch => Self::EffigyScratch,
            registry::MarkerFamily::OxideScratch => Self::OxideScratch,
        }
    }
}

/// The marker one directory unit carries: its filename, its family and the
/// SHA-256 of the bytes the plan read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnitMarker {
    /// The marker's filename inside the unit, exactly as it was observed.
    pub filename: String,
    /// The family that filename belongs to.
    pub family: MarkerFamily,
    /// The SHA-256 of the marker bytes the plan read.
    pub digest: String,
}

impl UnitMarker {
    /// The marker a family's canonical filename and digest describe.
    pub fn new(family: MarkerFamily, digest: impl Into<String>) -> Self {
        Self {
            filename: family.marker_file().to_owned(),
            family,
            digest: digest.into(),
        }
    }

    /// Validate every recorded fact strictly.
    ///
    /// The filename is not free text: it must be the family's own pinned name,
    /// so a record can never claim the Effigy family while read from Barge's
    /// marker (or the reverse), and the digest is a SHA-256 hex string.
    pub fn validate(&self) -> Result<()> {
        if self.filename != self.family.marker_file() {
            return Err(Error::new(
                ErrorKind::UnknownState,
                format!(
                    "the `{}` marker family is read from `{}`, not `{}`",
                    self.family.as_str(),
                    self.family.marker_file(),
                    self.filename
                ),
            )
            .with_detail("marker", self.filename.clone().into()));
        }
        if !is_sha256(&self.digest) {
            return Err(Error::new(
                ErrorKind::UnknownState,
                format!(
                    "the `{}` marker's digest is not a SHA-256 hex string",
                    self.family.as_str()
                ),
            )
            .with_detail("marker", self.filename.clone().into()));
        }
        Ok(())
    }
}

/// Which recorded grant authorises one unit's deletion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrantKind {
    /// A native root: Barge created the unit, so its own marker is the grant.
    NativeRoot,
    /// A foreign marker family's owner delegated that family on this volume.
    Delegation,
    /// A human's legacy claim over one scope of unmarked units.
    Claim,
}

impl GrantKind {
    /// The grant kind's name, as a record and a report spell it.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NativeRoot => "native_root",
            Self::Delegation => "delegation",
            Self::Claim => "claim",
        }
    }

    /// Parse a recorded grant kind strictly.
    pub fn parse(kind: &str) -> Option<Self> {
        match kind {
            "native_root" => Some(Self::NativeRoot),
            "delegation" => Some(Self::Delegation),
            "claim" => Some(Self::Claim),
            _ => None,
        }
    }
}

/// The grant one unit's deletion rests on, recorded in the intent.
///
/// `reference` is what identifies the grant in the volume's `scratch_grants`: the
/// unit's own name for a native root, the family name for a delegation, and the
/// claimed scope (`project:<name>` or `top_level_adhoc`) for a claim. The
/// eligibility predicate behind it is S4's; custody only requires that the grant
/// exists, is of the shape the unit's marker family needs, and is recorded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnitGrant {
    /// Which form of grant this is.
    pub kind: GrantKind,
    /// The recorded reference that names it.
    pub reference: String,
}

impl UnitGrant {
    /// The native-root grant a Barge-owned root carries.
    pub fn native_root(reference: impl Into<String>) -> Self {
        Self {
            kind: GrantKind::NativeRoot,
            reference: reference.into(),
        }
    }

    /// The delegation grant a foreign-family unit carries.
    pub fn delegation(family: MarkerFamily) -> Self {
        Self {
            kind: GrantKind::Delegation,
            reference: family.as_str().to_owned(),
        }
    }

    /// A legacy claim over one scope.
    pub fn claim(reference: impl Into<String>) -> Self {
        Self {
            kind: GrantKind::Claim,
            reference: reference.into(),
        }
    }

    /// Validate that the reference names something a grant of this kind can be.
    pub fn validate(&self) -> Result<()> {
        if self.reference.is_empty() || self.reference.len() > 256 {
            return Err(Error::new(
                ErrorKind::UnknownState,
                "a unit's grant reference must be 1 to 256 bytes".to_owned(),
            ));
        }
        if self.reference.chars().any(char::is_control) {
            return Err(Error::new(
                ErrorKind::UnknownState,
                "a unit's grant reference carries a control character".to_owned(),
            ));
        }
        match self.kind {
            GrantKind::NativeRoot => Ok(()),
            GrantKind::Delegation => {
                if MarkerFamily::parse(&self.reference)
                    .is_some_and(|family| family.delegated().is_some())
                {
                    return Ok(());
                }
                Err(Error::new(
                    ErrorKind::UnknownState,
                    format!(
                        "`{}` is not a foreign marker family a delegation can cover",
                        self.reference
                    ),
                ))
            }
            GrantKind::Claim => {
                if self.reference == "top_level_adhoc" || project_scope(&self.reference).is_some() {
                    return Ok(());
                }
                Err(Error::new(
                    ErrorKind::UnknownState,
                    format!(
                        "`{}` is neither `top_level_adhoc` nor `project:<name>`",
                        self.reference
                    ),
                ))
            }
        }
    }
}

/// The project name of a `project:<name>` claim reference, when it is one.
fn project_scope(reference: &str) -> Option<&str> {
    let name = reference.strip_prefix("project:")?;
    if name.is_empty() {
        return None;
    }
    area::validate_name("project", name).ok()?;
    Some(name)
}

/// One deletion unit: what a plan names and what custody and deletion act on.
///
/// Everything here is the plan's expectation, re-read by custody before the first
/// effect and recorded in the `barge.intent/2` journal before the unit is
/// renamed. `parent` is relative to the area folder `root_dir`/`folder` names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeletionUnit {
    /// The verified volume's UUID.
    pub volume_uuid: VolumeUuid,
    /// The volume's `root_dir` the area folder is resolved under.
    pub root_dir: String,
    /// The area folder the unit lives in.
    pub folder: String,
    /// The parent directory, relative to the area folder; `""` is the folder
    /// itself.
    pub parent: String,
    /// The unit's own name inside that parent.
    pub name: String,
    /// Whether the unit is a directory or one file.
    pub kind: UnitKind,
    /// The device the plan observed.
    pub device: DeviceId,
    /// The inode the plan observed.
    pub inode: u64,
    /// The marker the plan read, when the unit carries one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marker: Option<UnitMarker>,
    /// The recorded grant that authorises the deletion.
    pub grant: UnitGrant,
    /// The SHA-256 of the plan that named this unit.
    pub plan_digest: String,
}

impl DeletionUnit {
    /// The unit an owned root is: a Barge-marked directory directly inside its
    /// area folder, authorised by the native-root grant Barge recorded when it
    /// created the root.
    ///
    /// This is the shape S5 deleted before S4's units existed, expressed as the
    /// general unit every delete path now takes.
    pub fn from_owned(
        owned: &area::OwnedRoot,
        root_dir: &str,
        folder: &str,
        plan_digest: &str,
    ) -> Self {
        Self {
            volume_uuid: owned.volume_uuid.clone(),
            root_dir: root_dir.to_owned(),
            folder: folder.to_owned(),
            parent: String::new(),
            name: owned.root_id.clone(),
            kind: UnitKind::Directory,
            device: owned.identity.device,
            inode: owned.identity.inode,
            marker: Some(UnitMarker::new(
                MarkerFamily::Barge,
                owned.marker_digest.clone(),
            )),
            grant: UnitGrant::native_root(owned.root_id.clone()),
            plan_digest: plan_digest.to_owned(),
        }
    }

    /// The unit a bounded root listing reported, when its marker was owned and
    /// its digest was read.
    pub fn from_listing(
        row: &area::RootObservation,
        volume_uuid: &VolumeUuid,
        root_dir: &str,
        folder: &str,
        plan_digest: &str,
    ) -> Option<Self> {
        let root_id = row.root_id.clone()?;
        Some(Self {
            volume_uuid: volume_uuid.clone(),
            root_dir: root_dir.to_owned(),
            folder: folder.to_owned(),
            parent: String::new(),
            name: root_id.clone(),
            kind: UnitKind::Directory,
            device: DeviceId::new(row.device?),
            inode: row.inode?,
            marker: Some(UnitMarker::new(
                MarkerFamily::Barge,
                row.marker_digest.clone()?,
            )),
            grant: UnitGrant::native_root(root_id),
            plan_digest: plan_digest.to_owned(),
        })
    }

    /// The unit's path inside one area folder.
    pub fn path_in(&self, area_path: &Path) -> PathBuf {
        if self.parent.is_empty() {
            return area_path.join(&self.name);
        }
        area_path.join(&self.parent).join(&self.name)
    }

    /// The platform entry kind this unit must be.
    pub const fn entry_kind(&self) -> EntryKind {
        self.kind.entry_kind()
    }

    /// Open the unit's parent directory, descriptor-relative from its area
    /// folder.
    ///
    /// Each component is opened from the descriptor before it and re-checked to
    /// be on the unit's device, so a parent path can never be re-resolved from a
    /// string, follow a link, or cross onto another volume; custody then decides
    /// the unit by name inside that descriptor.
    /// `Ok(None)` is a component that is positively not there, which is what lets
    /// a replay decide an absent parent without inferring anything from an
    /// unreadable one.
    pub fn open_parent(&self, dirs: &dyn DirOps, area: &DirHandle) -> Result<Option<DirHandle>> {
        let mut current = dirs.duplicate_dir(area)?;
        if self.parent.is_empty() {
            return Ok(Some(current));
        }
        for component in self.parent.split('/') {
            let Some(facts) = dirs.child_facts(&current, component)? else {
                return Ok(None);
            };
            if facts.kind != EntryKind::Directory {
                return Err(Error::new(
                    ErrorKind::UnknownState,
                    format!(
                        "the unit's parent component `{component}` is a {}, not a directory",
                        facts.kind.as_str()
                    ),
                )
                .with_detail("parent", self.parent.clone().into())
                .with_detail("component", component.to_owned().into()));
            }
            if facts.device != self.device {
                return Err(Error::new(
                    ErrorKind::VolumeMismatch,
                    format!(
                        "the unit's parent component `{component}` is on device {}, not the unit's {}",
                        facts.device, self.device
                    ),
                )
                .with_detail("parent", self.parent.clone().into())
                .with_detail("component", component.to_owned().into()));
            }
            let next = dirs.open_child_dir(&current, component)?;
            let opened = next.identity();
            if opened.device != facts.device || opened.inode != facts.inode {
                return Err(Error::new(
                    ErrorKind::UnknownState,
                    format!(
                        "the unit's parent component `{component}` opened as device {} inode {}, not the observed {} / {}",
                        opened.device, opened.inode, facts.device, facts.inode
                    ),
                )
                .with_detail("parent", self.parent.clone().into())
                .with_detail("component", component.to_owned().into()));
            }
            current = next;
        }
        Ok(Some(current))
    }

    /// The marker filename this unit records, when it records one.
    pub fn marker_file(&self) -> Option<&str> {
        self.marker.as_ref().map(|marker| marker.filename.as_str())
    }

    /// Validate every recorded fact strictly.
    ///
    /// A record that does not validate is unknown state, never an instruction to
    /// delete: the plan's digest, the area folder, the parent, the name, the
    /// marker and the grant each have one well-formed shape, and the grant must
    /// be the form the marker family needs.
    pub fn validate(&self) -> Result<()> {
        if self.volume_uuid.to_string().is_empty() {
            return Err(Error::new(
                ErrorKind::UnknownState,
                "the unit records no volume".to_owned(),
            ));
        }
        registry::validate_folder("root_dir", &self.root_dir, true)?;
        registry::validate_folder("folder", &self.folder, false)?;
        validate_parent(&self.parent)?;
        crate::state::validate_entry_name(&self.name)?;
        if self.name == "." || self.name == ".." {
            return Err(Error::new(
                ErrorKind::UnknownState,
                format!("`{}` is not a unit name", self.name),
            ));
        }
        if !is_sha256(&self.plan_digest) {
            return Err(Error::new(
                ErrorKind::UnknownState,
                "the unit's plan digest is not a SHA-256 hex string".to_owned(),
            ));
        }
        self.grant.validate()?;
        match (&self.marker, self.kind) {
            (Some(marker), UnitKind::Directory) => marker.validate()?,
            (Some(_), UnitKind::File) => {
                return Err(Error::new(
                    ErrorKind::UnknownState,
                    "a file unit never carries a marker, which is a file inside a directory"
                        .to_owned(),
                )
                .with_detail("unit", self.name.clone().into()))
            }
            (None, _) => {}
        }
        self.validate_grant_shape()
    }

    /// Whether the recorded grant is the form this unit's marker family needs.
    fn validate_grant_shape(&self) -> Result<()> {
        validate_grant_shape(self.marker.as_ref(), self.kind, &self.grant, &self.name)
    }
}

/// Whether a recorded grant is the form a unit's marker family needs.
///
/// A native root is a Barge-marked directory; a foreign family needs that
/// family's delegation; an unmarked unit needs a legacy claim. This is what
/// keeps a missing or mismatched grant from becoming a deletion: a plan unit and
/// a journal record both refuse before any effect, so neither can claim one
/// family's authority for another family's data.
pub fn validate_grant_shape(
    marker: Option<&UnitMarker>,
    kind: UnitKind,
    grant: &UnitGrant,
    unit_name: &str,
) -> Result<()> {
    let family = marker.map(|marker| marker.family);
    let expected = match (family, kind) {
        (Some(MarkerFamily::Barge), UnitKind::Directory) => GrantKind::NativeRoot,
        (Some(_), UnitKind::Directory) => {
            if grant.kind != GrantKind::Delegation
                || Some(grant.reference.as_str()) != family.map(MarkerFamily::as_str)
            {
                return Err(grant_shape(unit_name, family, grant));
            }
            GrantKind::Delegation
        }
        (Some(_), UnitKind::File) => {
            return Err(Error::new(
                ErrorKind::UnknownState,
                "a file unit never carries a marker, which is a file inside a directory".to_owned(),
            )
            .with_detail("unit", unit_name.to_owned().into()))
        }
        (None, _) => GrantKind::Claim,
    };
    if grant.kind != expected {
        return Err(grant_shape(unit_name, family, grant));
    }
    if expected == GrantKind::NativeRoot && grant.reference != unit_name {
        return Err(Error::new(
            ErrorKind::UnknownState,
            "a native root's grant reference is its own name".to_owned(),
        )
        .with_detail("unit", unit_name.to_owned().into())
        .with_detail("reference", grant.reference.clone().into()));
    }
    Ok(())
}

/// The refusal for a grant whose shape does not cover the unit.
fn grant_shape(unit: &str, family: Option<MarkerFamily>, grant: &UnitGrant) -> Error {
    let family = family.map_or("none", MarkerFamily::as_str);
    Error::new(
        ErrorKind::UnknownState,
        format!(
            "the unit `{unit}` has a `{family}` marker family and needs a `{}` grant, but it records `{}` (`{}`)",
            match family {
                "none" => "claim",
                "barge" => "native_root",
                _ => "delegation",
            },
            grant.kind.as_str(),
            grant.reference
        ),
    )
    .with_detail("unit", unit.to_owned().into())
    .with_detail("grant", grant.kind.as_str().into())
    .with_detail("reference", grant.reference.clone().into())
}

/// Validate one unit's parent: zero or more path components, relative, none
/// empty, `.` or `..`, and no more than [`UNIT_PARENT_COMPONENTS`].
pub fn validate_parent(parent: &str) -> Result<()> {
    if parent.is_empty() {
        return Ok(());
    }
    if parent.starts_with('/') || parent.ends_with('/') || parent.contains('\0') {
        return Err(Error::new(
            ErrorKind::UnknownState,
            format!("`{parent}` is not a relative parent path"),
        )
        .with_detail("parent", parent.to_owned().into()));
    }
    let components: Vec<&str> = parent.split('/').collect();
    if components.len() > UNIT_PARENT_COMPONENTS {
        return Err(Error::new(
            ErrorKind::ResourceBound,
            format!("a unit's parent may have at most {UNIT_PARENT_COMPONENTS} components"),
        )
        .with_detail("parent", parent.to_owned().into()));
    }
    for component in components {
        if component.is_empty() || component == "." || component == ".." {
            return Err(Error::new(
                ErrorKind::UnknownState,
                format!("`{parent}` is not a relative parent path"),
            )
            .with_detail("parent", parent.to_owned().into()));
        }
        crate::state::validate_entry_name(component)?;
    }
    Ok(())
}

/// Whether a value is a lowercase-or-uppercase hexadecimal SHA-256.
pub fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::{
        is_sha256, validate_parent, DeletionUnit, GrantKind, MarkerFamily, UnitGrant, UnitKind,
        UnitMarker, BARGE_MARKER_FILE, EFFIGY_SCRATCH_MARKER_FILE, RECOGNISED_MARKER_FILES,
    };
    use crate::error::ErrorKind;
    use crate::os::{DeviceId, VolumeUuid};

    const PLAN: &str = "abababababababababababababababababababababababababababababababab";
    const UUID: &str = "A6AC3ED7-33EA-46B4-8082-7254CF291ED2";

    fn unit(kind: UnitKind, marker: Option<UnitMarker>) -> DeletionUnit {
        let grant = match (&marker, kind) {
            (Some(marker), UnitKind::Directory) if marker.family == MarkerFamily::Barge => {
                UnitGrant::native_root("run-1")
            }
            (Some(marker), UnitKind::Directory) => UnitGrant::delegation(marker.family),
            _ => UnitGrant::claim("top_level_adhoc"),
        };
        DeletionUnit {
            volume_uuid: VolumeUuid::parse(UUID).expect("a UUID"),
            root_dir: "scratch".to_owned(),
            folder: "example/pool".to_owned(),
            parent: String::new(),
            name: "run-1".to_owned(),
            kind,
            device: DeviceId::new(16_777_219),
            inode: 4_242,
            marker,
            grant,
            plan_digest: PLAN.to_owned(),
        }
    }

    #[test]
    fn the_four_unit_shapes_a_plan_can_name_validate() {
        // A Barge root: a directory with Barge's marker and the native grant.
        let barge = unit(
            UnitKind::Directory,
            Some(UnitMarker::new(MarkerFamily::Barge, "a".repeat(64))),
        );
        barge.validate().expect("a Barge root is a unit");
        assert_eq!(barge.marker_file(), Some(BARGE_MARKER_FILE));
        assert_eq!(barge.entry_kind(), crate::os::EntryKind::Directory);

        // A delegated foreign-marker directory.
        let effigy = unit(
            UnitKind::Directory,
            Some(UnitMarker::new(MarkerFamily::EffigyScratch, "b".repeat(64))),
        );
        effigy.validate().expect("a delegated Effigy run is a unit");
        assert_eq!(effigy.marker_file(), Some(EFFIGY_SCRATCH_MARKER_FILE));
        assert_eq!(
            effigy.grant,
            UnitGrant::delegation(MarkerFamily::EffigyScratch)
        );

        // An unmarked claimed directory, and a claimed top-level file.
        for shape in [unit(UnitKind::Directory, None), unit(UnitKind::File, None)] {
            shape.validate().expect("a claimed unmarked unit is a unit");
            assert_eq!(shape.grant.kind, GrantKind::Claim);
        }
    }

    #[test]
    fn a_grant_whose_shape_does_not_cover_the_unit_is_refused() {
        let mut missing = unit(UnitKind::Directory, None);
        missing.grant = UnitGrant::native_root("run-1");
        assert_eq!(
            missing.validate().expect_err("a claim is required").kind(),
            ErrorKind::UnknownState
        );

        let mut mismatched = unit(
            UnitKind::Directory,
            Some(UnitMarker::new(MarkerFamily::OxideScratch, "c".repeat(64))),
        );
        mismatched.grant = UnitGrant::delegation(MarkerFamily::EffigyScratch);
        assert!(
            mismatched.validate().is_err(),
            "a delegation names its family"
        );

        let mut wrong_native = unit(
            UnitKind::Directory,
            Some(UnitMarker::new(MarkerFamily::Barge, "d".repeat(64))),
        );
        wrong_native.grant = UnitGrant::native_root("another-root");
        assert!(wrong_native.validate().is_err(), "the grant names the unit");

        let mut foreign_marker_with_claim = unit(
            UnitKind::Directory,
            Some(UnitMarker::new(MarkerFamily::Barge, "e".repeat(64))),
        );
        foreign_marker_with_claim.grant = UnitGrant::claim("top_level_adhoc");
        assert!(
            foreign_marker_with_claim.validate().is_err(),
            "a marker family is never covered by a claim"
        );
    }

    #[test]
    fn a_marker_filename_that_is_not_its_family_is_refused() {
        let mut wrong = UnitMarker::new(MarkerFamily::Barge, "a".repeat(64));
        wrong.filename = EFFIGY_SCRATCH_MARKER_FILE.to_owned();
        assert!(wrong.validate().is_err());
        assert_eq!(wrong.family.marker_file(), BARGE_MARKER_FILE);
        // The recognised filenames are exactly the three families'.
        for name in RECOGNISED_MARKER_FILES {
            assert!(MarkerFamily::from_marker_file(name).is_some(), "{name}");
        }
        assert!(MarkerFamily::from_marker_file(".other.json").is_none());
    }

    #[test]
    fn a_file_unit_never_carries_a_marker() {
        let mut file = unit(UnitKind::File, None);
        file.marker = Some(UnitMarker::new(MarkerFamily::Barge, "a".repeat(64)));
        file.grant = UnitGrant::native_root("run-1");
        let error = file
            .validate()
            .expect_err("a file cannot carry a marker inside it");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
    }

    #[test]
    fn a_parent_is_a_relative_path_of_bounded_components() {
        assert!(validate_parent("").is_ok());
        assert!(validate_parent("project").is_ok());
        assert!(validate_parent("effigy/runs").is_ok());
        for bad in ["/project", "project/", "a//b", "a/./b", "a/../b", "."] {
            assert!(validate_parent(bad).is_err(), "{bad}");
        }
        let deep = ["a"; super::UNIT_PARENT_COMPONENTS + 1].join("/");
        assert!(validate_parent(&deep).is_err(), "the component bound holds");
    }

    #[test]
    fn a_plan_digest_that_is_not_sha256_is_refused() {
        let mut unit = unit(UnitKind::Directory, None);
        unit.plan_digest = "not a digest".to_owned();
        assert!(unit.validate().is_err());
        assert!(is_sha256(PLAN));
        assert!(!is_sha256("ab"));
    }

    #[test]
    fn a_parent_and_a_name_compose_the_unit_path() {
        let mut unit = unit(UnitKind::File, None);
        unit.parent = "effigy".to_owned();
        unit.name = "run.abcd".to_owned();
        assert_eq!(
            unit.path_in(std::path::Path::new("/Volumes/Builds/scratch")),
            std::path::PathBuf::from("/Volumes/Builds/scratch/effigy/run.abcd")
        );
        // A name is recorded as one component whatever it holds, and a slash is
        // never one.
        unit.name = "sub/name".to_owned();
        assert!(unit.validate().is_err());
    }
}
