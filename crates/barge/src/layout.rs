//! Resource classes and the account and volume layout records.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{Absent, Barge, Core, Durability, Error, Result, VolumeState};

const SCHEMA: &str = "barge.layout/1";

/// The resource classes Barge can place.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Class {
    Scratch,
    Worktrees,
    Builds,
    Caches,
    Evidence,
    Vms,
    Archives,
}

impl Class {
    pub const ALL: [Self; 7] = [
        Self::Scratch,
        Self::Worktrees,
        Self::Builds,
        Self::Caches,
        Self::Evidence,
        Self::Vms,
        Self::Archives,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Scratch => "scratch",
            Self::Worktrees => "worktrees",
            Self::Builds => "builds",
            Self::Caches => "caches",
            Self::Evidence => "evidence",
            Self::Vms => "vms",
            Self::Archives => "archives",
        }
    }

    pub fn durability(self) -> Durability {
        match self {
            Self::Vms | Self::Archives => Durability::Kept,
            _ => Durability::Expendable,
        }
    }
}

impl std::str::FromStr for Class {
    type Err = Error;
    fn from_str(value: &str) -> Result<Self> {
        Self::ALL
            .into_iter()
            .find(|class| class.name() == value)
            .ok_or_else(|| Error::usage(format!("unknown resource class {value:?}")))
    }
}

/// Placement and absent policy of a class. Missing policy defaults to Fail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Placement {
    pub volume: String,
    pub path: String,
    #[serde(default = "fail", skip_serializing_if = "is_fail")]
    pub absent: Absent,
}

fn fail() -> Absent {
    Absent::Fail
}
fn is_fail(value: &Absent) -> bool {
    *value == Absent::Fail
}

/// Account authority, or the subset recorded on one volume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Layout {
    pub schema: String,
    pub classes: BTreeMap<Class, Placement>,
}

impl Default for Layout {
    fn default() -> Self {
        Self {
            schema: SCHEMA.to_string(),
            classes: BTreeMap::new(),
        }
    }
}

impl Layout {
    fn validate(&self) -> Result<()> {
        if self.schema != SCHEMA {
            return Err(Error::unknown_state(format!(
                "unknown layout schema {}",
                self.schema
            )));
        }
        for placement in self.classes.values() {
            crate::state::validate_volume_name("volume name", &placement.volume)?;
            crate::state::validate_relative_path("class path", &placement.path)?;
            if placement.path.is_empty() {
                return Err(Error::usage("class path must not be empty"));
            }
            if let Absent::Fallback { path } = &placement.absent {
                if !path.is_absolute() {
                    return Err(Error::usage("fallback path must be absolute"));
                }
            }
        }
        Ok(())
    }
    fn for_volume(&self, volume: &str) -> Self {
        Self {
            schema: self.schema.clone(),
            classes: self
                .classes
                .iter()
                .filter(|(_, p)| p.volume == volume)
                .map(|(c, p)| (*c, p.clone()))
                .collect(),
        }
    }
}

fn read(path: &Path) -> Result<Option<Layout>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error::io("cannot read", path, &e)),
    };
    let layout: Layout = serde_json::from_slice(&bytes)
        .map_err(|e| Error::unknown_state(format!("cannot parse {}: {e}", path.display())))?;
    layout
        .validate()
        .map_err(|e| Error::unknown_state(format!("invalid {}: {e}", path.display())))?;
    Ok(Some(layout))
}

fn write(path: &Path, layout: &Layout) -> Result<()> {
    layout.validate()?;
    let bytes = serde_json::to_vec_pretty(layout)
        .map_err(|e| Error::unknown_state(format!("cannot encode layout: {e}")))?;
    crate::state::publish(path, &bytes)
}

/// A resolved class path and whether its fallback is in use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResourcePath {
    pub path: PathBuf,
    pub fallback: bool,
}

impl Core {
    /// Read the account layout, without creating state.
    pub(crate) fn account_layout(&self) -> Result<Layout> {
        Ok(read(&self.state.root().join("layout.json"))?.unwrap_or_default())
    }
}

impl Barge {
    /// Read the account layout, without creating state.
    pub fn layout(&self) -> Result<Layout> {
        self.core.account_layout()
    }

    /// Read a volume's record, after checking the registered volume identity.
    pub fn volume_layout(&self, volume: &str) -> Result<Option<Layout>> {
        let mount = self.volume(volume)?.require_present()?;
        read(&mount.join(".barge/layout.json"))
    }

    /// Report whether an existing volume record differs from the account copy.
    pub fn layout_disagrees(&self, volume: &str) -> Result<bool> {
        let expected = self.layout()?.for_volume(volume);
        Ok(match self.volume_layout(volume)? {
            Some(record) => record != expected,
            None => !expected.classes.is_empty(),
        })
    }

    /// Import an offered record only when this account has no placements for it.
    pub fn import_volume_layout(&self, volume: &str) -> Result<Layout> {
        self.core.ensure_writable()?;
        let _lock = self.core.state.lock()?;
        let mut account = self.layout()?;
        if !account.for_volume(volume).classes.is_empty() {
            return Err(Error::refused(format!(
                "account already has layout for {volume}"
            )));
        }
        let record = self
            .volume_layout(volume)?
            .ok_or_else(|| Error::not_found(format!("no layout record on {volume}")))?;
        for (class, placement) in record.classes {
            if placement.volume != volume || account.classes.contains_key(&class) {
                return Err(Error::refused(format!(
                    "cannot import conflicting class {}",
                    class.name()
                )));
            }
            account.classes.insert(class, placement);
        }
        write(&self.state_dir().join("layout.json"), &account)?;
        Ok(account)
    }

    /// Change one placement; no resource data is moved.
    pub fn set_layout(&self, class: Class, placement: Placement) -> Result<Layout> {
        self.core.ensure_writable()?;
        let _lock = self.core.state.lock()?;
        let mut account = self.layout()?;
        let mut affected = vec![placement.volume.clone()];
        if let Some(old) = account.classes.get(&class) {
            if !affected.contains(&old.volume) {
                affected.push(old.volume.clone());
            }
        }
        for name in &affected {
            let volume = self.volume(name)?;
            volume.require_present()?;
            let expected = account.for_volume(name);
            let record = read(&volume.spec().mount.join(".barge/layout.json"))?;
            if record
                .as_ref()
                .map_or(!expected.classes.is_empty(), |record| record != &expected)
            {
                return Err(Error::refused(format!("layout record on {name} disagrees with account; resolve it before changing layout")));
            }
        }
        account.classes.insert(class, placement);
        account.validate()?;
        write(&self.state_dir().join("layout.json"), &account)?;
        #[cfg(test)]
        WRITE_HOOK.with(|hook| {
            if let Some(hook) = hook.borrow_mut().take() {
                hook();
            }
        });
        for name in affected {
            let mount = self.volume(&name)?.require_present()?;
            write(
                &mount.join(".barge/layout.json"),
                &account.for_volume(&name),
            )?;
        }
        Ok(account)
    }

    /// Rewrite one volume record from this account's authoritative layout.
    pub fn repair_volume_layout(&self, name: &str) -> Result<()> {
        self.core.ensure_writable()?;
        let _lock = self.core.state.lock()?;
        let account = self.layout()?;
        let mount = self.volume(name)?.require_present()?;
        write(&mount.join(".barge/layout.json"), &account.for_volume(name))
    }

    /// Resolve a class on its volume or its explicit fallback.
    pub fn resource_path(&self, class: Class, project: Option<&str>) -> Result<ResourcePath> {
        if let Some(project) = project {
            crate::state::validate_name("project", project)?;
        }
        let layout = self.layout()?;
        let placement = layout
            .classes
            .get(&class)
            .ok_or_else(|| Error::not_found(format!("class {} has no layout", class.name())))?;
        let volume = self.volume(&placement.volume)?;
        let (mut path, fallback) = match volume.state()? {
            VolumeState::Present => (volume.base().join(&placement.path), false),
            VolumeState::Absent => match &placement.absent {
                Absent::Fail => {
                    return Err(Error::refused(format!(
                        "volume {} is absent; attach it at {}",
                        placement.volume,
                        volume.spec().mount.display()
                    )))
                }
                Absent::Fallback { path } => (path.clone(), true),
            },
            VolumeState::Mismatch { .. } => {
                volume.require_present()?;
                return Err(Error::refused("volume identity mismatch"));
            }
        };
        if let Some(project) = project {
            path.push(project);
        }
        Ok(ResourcePath { path, fallback })
    }
    /// List top-level items, classifying known placement and area paths.
    pub fn survey(&self, name: &str, with_bytes: bool) -> Result<Vec<SurveyItem>> {
        let volume = self.volume(name)?;
        volume.require_present()?;
        let base = volume.base();
        let layout = self.layout()?;
        let areas = self.core.state.read_areas()?;
        let mut items = Vec::new();
        for entry in fs::read_dir(&base).map_err(|e| Error::io("cannot list", &base, &e))? {
            let entry = entry.map_err(|e| Error::io("cannot list", &base, &e))?;
            let path = entry.path();
            let classification = if entry.file_name() == ".barge" {
                SurveyClassification::Barge
            } else if [
                ".DS_Store",
                ".Spotlight-V100",
                ".fseventsd",
                ".TemporaryItems",
                ".DocumentRevisions-V100",
                ".Trashes",
            ]
            .iter()
            .any(|name| entry.file_name() == *name)
            {
                SurveyClassification::System
            } else if let Some((class, _)) = layout.classes.iter().find(|(_, placement)| {
                placement.volume == name && base.join(&placement.path).starts_with(&path)
            }) {
                SurveyClassification::Class(*class)
            } else if let Some(area) = areas.iter().find(|area| {
                area.volume == name
                    && volume
                        .base()
                        .join(area.effective_folder())
                        .starts_with(&path)
            }) {
                SurveyClassification::Area(format!("{}/{}", area.namespace, area.name))
            } else {
                SurveyClassification::Unknown
            };
            let bytes = if with_bytes {
                Some(survey_bytes(&path)?)
            } else {
                None
            };
            items.push(SurveyItem {
                path,
                classification,
                bytes,
            });
        }
        items.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(items)
    }
}

/// The role of an item in a volume survey.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SurveyClassification {
    Barge,
    System,
    Class(Class),
    Area(String),
    Unknown,
}

impl std::fmt::Display for SurveyClassification {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Barge => f.write_str("barge"),
            Self::System => f.write_str("system"),
            Self::Class(class) => write!(f, "class:{}", class.name()),
            Self::Area(area) => write!(f, "area:{area}"),
            Self::Unknown => f.write_str("unknown"),
        }
    }
}

impl Serialize for SurveyClassification {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

/// One item in a read-only volume survey.
#[derive(Debug, Clone, Serialize)]
pub struct SurveyItem {
    pub path: PathBuf,
    pub classification: SurveyClassification,
    pub bytes: Option<u64>,
}

fn survey_bytes(path: &Path) -> Result<u64> {
    let metadata = fs::symlink_metadata(path).map_err(|e| Error::io("cannot inspect", path, &e))?;
    if !metadata.is_dir() {
        return Ok(metadata.len());
    }
    let mut total = 0_u64;
    for entry in fs::read_dir(path).map_err(|e| Error::io("cannot list", path, &e))? {
        let entry = entry.map_err(|e| Error::io("cannot list", path, &e))?;
        total = total.saturating_add(survey_bytes(&entry.path())?);
    }
    Ok(total)
}

#[cfg(test)]
thread_local! {
    static WRITE_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = std::cell::RefCell::new(None);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FakeOs, ManualClock, VolumeRegistration};
    use std::sync::Arc;

    #[test]
    fn invalid_layout_and_unknown_schema_are_unknown_state() {
        let tmp = crate::testing::TempDir::new("invalid-layout").unwrap();
        let path = tmp.child("layout.json");
        std::fs::write(&path, br#"{"schema":"barge.layout/1","classes":{"scratch":{"volume":"Builds","path":"../escape"}}}"#).unwrap();
        assert_eq!(
            read(&path).unwrap_err().kind(),
            crate::ErrorKind::UnknownState
        );
        std::fs::write(&path, br#"{"schema":"barge.layout/99","classes":{}}"#).unwrap();
        assert_eq!(
            read(&path).unwrap_err().kind(),
            crate::ErrorKind::UnknownState
        );
    }

    #[test]
    fn failed_volume_write_can_be_repaired_from_account() {
        let tmp = crate::testing::TempDir::new("layout-repair").unwrap();
        let mount = tmp.folder("mount").unwrap();
        let state = tmp.folder("state").unwrap();
        let os = Arc::new(FakeOs::with_volume(
            &mount,
            "11111111-2222-3333-4444-555555555555",
            100,
        ));
        let barge = Barge::open_with(state, os, Arc::new(ManualClock::new(0))).unwrap();
        barge
            .register_volume(VolumeRegistration::new("Builds", &mount).floor_bytes(0))
            .unwrap();
        let path = mount.join(".barge/layout.json");
        let blocked = path.clone();
        WRITE_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || std::fs::create_dir_all(blocked).unwrap()))
        });
        assert!(barge
            .set_layout(
                Class::Scratch,
                Placement {
                    volume: "Builds".into(),
                    path: "scratch".into(),
                    absent: Absent::Fail
                }
            )
            .is_err());
        std::fs::remove_dir(&path).unwrap();
        assert!(barge.layout_disagrees("Builds").unwrap());
        barge.repair_volume_layout("Builds").unwrap();
        assert!(!barge.layout_disagrees("Builds").unwrap());
        barge
            .set_layout(
                Class::Scratch,
                Placement {
                    volume: "Builds".into(),
                    path: "new".into(),
                    absent: Absent::Fail,
                },
            )
            .unwrap();
    }
}
