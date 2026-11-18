//! Read-only plans for moving resources on a registered volume.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::bindings::{observe, BindingRecord, Observed};
use crate::{Barge, Class, Error, Placement, Result};

/// The filesystem identity captured for an existing entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoveIdentity {
    pub dev: u64,
    pub ino: u64,
}

/// A recorded link and its exact observed and planned values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoveBinding {
    pub record: BindingRecord,
    pub before: Observed,
    pub after: PathBuf,
}

/// A reviewed rename and the state that must still match when applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MovePlan {
    pub schema: String,
    pub volume: String,
    pub source: PathBuf,
    pub source_identity: MoveIdentity,
    pub destination: PathBuf,
    pub destination_parent_identity: MoveIdentity,
    pub bindings: Vec<MoveBinding>,
    pub layout: Option<MoveLayout>,
    pub consumers: Vec<String>,
    pub manual_steps: Vec<String>,
}

/// A class placement before and after its folder is moved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoveLayout {
    pub class: Class,
    pub before: Placement,
    pub after: Placement,
}

fn identity(path: &Path) -> Result<MoveIdentity> {
    let metadata = fs::symlink_metadata(path).map_err(|e| Error::io("cannot inspect", path, &e))?;
    Ok(MoveIdentity {
        dev: metadata.dev(),
        ino: metadata.ino(),
    })
}

fn canonical(path: &Path) -> Result<PathBuf> {
    fs::canonicalize(path).map_err(|e| Error::io("cannot resolve", path, &e))
}

impl Barge {
    /// Plan a class folder rename. The new path is relative to the volume base.
    pub fn plan_class_move(&self, class: Class, new_path: &str) -> Result<MovePlan> {
        crate::state::validate_relative_path("class path", new_path)?;
        if new_path.is_empty() {
            return Err(Error::usage("class path must not be empty"));
        }
        let layout = self.layout()?;
        let before = layout
            .classes
            .get(&class)
            .cloned()
            .ok_or_else(|| Error::not_found(format!("class {} has no layout", class.name())))?;
        let volume = self.volume(&before.volume)?;
        volume.require_present()?;
        let source = volume.base().join(&before.path);
        let destination = volume.base().join(new_path);
        let mut after = before.clone();
        after.path = new_path.to_string();
        self.plan_move(
            &before.volume,
            &source,
            &destination,
            Some(MoveLayout {
                class,
                before,
                after,
            }),
        )
    }

    /// Plan a top-level volume item into a class, optionally under a relative path.
    pub fn plan_item_move(&self, source: &Path, class: Class, sub: &str) -> Result<MovePlan> {
        crate::state::validate_relative_path("destination subpath", sub)?;
        let placement = self
            .layout()?
            .classes
            .get(&class)
            .cloned()
            .ok_or_else(|| Error::not_found(format!("class {} has no layout", class.name())))?;
        let volume = self.volume(&placement.volume)?;
        volume.require_present()?;
        let base = canonical(&volume.base())?;
        let source = canonical(source)?;
        if source.parent() != Some(base.as_path()) {
            return Err(Error::refused(
                "item must be directly inside the volume base",
            ));
        }
        let name = source
            .file_name()
            .ok_or_else(|| Error::usage("item needs a name"))?;
        let destination = if sub.is_empty() {
            volume.base().join(&placement.path).join(name)
        } else {
            volume.base().join(&placement.path).join(sub)
        };
        self.plan_move(&placement.volume, &source, &destination, None)
    }

    fn plan_move(
        &self,
        volume_name: &str,
        source: &Path,
        destination: &Path,
        layout: Option<MoveLayout>,
    ) -> Result<MovePlan> {
        let volume = self.volume(volume_name)?;
        volume.require_present()?;
        let base = canonical(&volume.base())?;
        let source = canonical(source)?;
        let parent = destination
            .parent()
            .ok_or_else(|| Error::usage("destination needs a parent"))?;
        let parent = canonical(parent)?;
        let name = destination
            .file_name()
            .ok_or_else(|| Error::usage("destination needs a name"))?;
        let destination = parent.join(name);
        if !source.starts_with(&base)
            || source == base
            || !parent.starts_with(&base)
            || parent.starts_with(&source)
        {
            return Err(Error::refused(
                "source and destination must be distinct paths inside the registered volume",
            ));
        }
        match fs::symlink_metadata(&destination) {
            Ok(_) => {
                return Err(Error::refused(format!(
                    "destination {} already exists",
                    destination.display()
                )))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(Error::io("cannot inspect", &destination, &e)),
        }
        let source_identity = identity(&source)?;
        let destination_parent_identity = identity(&parent)?;
        if source_identity.dev != destination_parent_identity.dev {
            return Err(Error::refused(
                "move crosses a filesystem; copy is not supported",
            ));
        }
        let mut bindings = Vec::new();
        for record in self.bindings().list()? {
            if let Ok(suffix) = record.applied.value.strip_prefix(&source) {
                let before = observe(record.link_path())?;
                if !matches!(&before, Observed::Link { .. })
                    || canonical(record.link_path())? != record.applied.value
                {
                    return Err(Error::refused(format!(
                        "binding {} has drifted",
                        record.consumer
                    )));
                }
                let after = destination.join(suffix);
                bindings.push(MoveBinding {
                    record,
                    before,
                    after,
                });
            }
        }
        let consumers = bindings.iter().map(|b| b.record.consumer.clone()).collect();
        let mut manual_steps = vec![
            "Update any unrecorded settings or workflows that refer to the old path.".to_string(),
        ];
        if source
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("pvm"))
        {
            manual_steps.push(format!(
                "Re-register in Parallels: prlctl unregister, then prlctl register {}",
                destination.display()
            ));
        }
        Ok(MovePlan {
            schema: "barge.move-plan/1".into(),
            volume: volume_name.into(),
            source,
            source_identity,
            destination,
            destination_parent_identity,
            bindings,
            layout,
            consumers,
            manual_steps,
        })
    }
}
