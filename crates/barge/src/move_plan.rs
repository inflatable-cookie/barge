//! Reviewed resource moves on a registered volume.

use std::fs;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::bindings::{observe, BindingRecord, Observed};
use crate::{AreaRecord, Barge, BindingTarget, Class, Error, Placement, Result};

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
    pub link_after: PathBuf,
}

/// A reviewed rename and the state that must still match when applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MovePlan {
    pub schema: String,
    pub volume: String,
    pub volume_spec: crate::VolumeSpec,
    pub source: PathBuf,
    pub source_identity: MoveIdentity,
    pub destination: PathBuf,
    pub destination_parent_identity: MoveIdentity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub create_directory: Option<PathBuf>,
    pub bindings: Vec<MoveBinding>,
    pub layout: Option<MoveLayout>,
    pub areas: Vec<MoveArea>,
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

/// A declared area carried with a moved folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoveArea {
    pub before: AreaRecord,
    pub after: AreaRecord,
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
            &before.volume.clone(),
            &source,
            &destination,
            Some(MoveLayout {
                class,
                before,
                after,
            }),
            None,
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
        if fs::symlink_metadata(source)
            .map_err(|e| Error::io("cannot inspect", source, &e))?
            .file_type()
            .is_symlink()
        {
            return Err(Error::refused("move source must not be a symlink"));
        }
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
        let class_dir = volume.base().join(&placement.path);
        self.plan_move(
            &placement.volume,
            &source,
            &destination,
            None,
            Some(&class_dir),
        )
    }

    fn plan_move(
        &self,
        volume_name: &str,
        source: &Path,
        destination: &Path,
        layout: Option<MoveLayout>,
        creatable: Option<&Path>,
    ) -> Result<MovePlan> {
        let volume = self.volume(volume_name)?;
        volume.require_present()?;
        let base = canonical(&volume.base())?;
        if fs::symlink_metadata(source)
            .map_err(|e| Error::io("cannot inspect", source, &e))?
            .file_type()
            .is_symlink()
        {
            return Err(Error::refused("move source must not be a symlink"));
        }
        let source = canonical(source)?;
        let parent = destination
            .parent()
            .ok_or_else(|| Error::usage("destination needs a parent"))?;
        let (parent, create_directory, identity_parent) = match canonical(parent) {
            Ok(parent) => (parent.clone(), None, parent),
            Err(_error) if creatable == Some(parent) && !parent.exists() => {
                let ancestor = parent
                    .parent()
                    .ok_or_else(|| Error::usage("class folder needs a parent"))?;
                let ancestor = canonical(ancestor)?;
                let name = parent
                    .file_name()
                    .ok_or_else(|| Error::usage("class folder needs a name"))?;
                let parent = ancestor.join(name);
                let class_folder = self.layout()?.classes.values().any(|placement| {
                    placement.volume == volume_name && base.join(&placement.path) == parent
                });
                if !class_folder {
                    return Err(Error::refused(
                        "missing destination parent is not a placed class folder",
                    ));
                }
                (parent.clone(), Some(parent), ancestor)
            }
            Err(error) => return Err(error),
        };
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
        let destination_parent_identity = identity(&identity_parent)?;
        if source_identity.dev != destination_parent_identity.dev {
            return Err(Error::refused(
                "move crosses a filesystem; copy is not supported",
            ));
        }
        for (other_class, placement) in &self.layout()?.classes {
            if placement.volume == volume_name
                && layout
                    .as_ref()
                    .is_none_or(|change| change.class != *other_class)
            {
                let other = base.join(&placement.path);
                if other.starts_with(&source) {
                    return Err(Error::refused(format!(
                        "move contains class {}; move it separately first",
                        other_class.name()
                    )));
                }
            }
        }
        let mut bindings = Vec::new();
        for record in self.bindings().list()? {
            let link = record.link_path();
            let link_parent = link
                .parent()
                .ok_or_else(|| Error::usage("link has no parent"))?;
            let resolved_link = canonical(link_parent)?.join(
                link.file_name()
                    .ok_or_else(|| Error::usage("link has no name"))?,
            );
            let value_suffix = record.applied.value.strip_prefix(&source).ok();
            let link_suffix = resolved_link.strip_prefix(&source).ok();
            if value_suffix.is_some() || link_suffix.is_some() {
                let before = observe(link)?;
                if !matches!(&before, Observed::Link { .. })
                    || canonical(link)? != record.applied.value
                {
                    return Err(Error::refused(format!(
                        "binding {} has drifted",
                        record.consumer
                    )));
                }
                let after = value_suffix.map_or_else(
                    || record.applied.value.clone(),
                    |suffix| destination.join(suffix),
                );
                let link_after = link_suffix
                    .map_or_else(|| link.to_path_buf(), |suffix| destination.join(suffix));
                bindings.push(MoveBinding {
                    record,
                    before,
                    after,
                    link_after,
                });
            }
        }
        let mut areas = Vec::new();
        for before in self.core.state.read_areas()? {
            if before.volume != volume_name {
                continue;
            }
            let area_path = base.join(before.effective_folder());
            if let Ok(suffix) = area_path.strip_prefix(&source) {
                let new_path = destination.join(suffix);
                let relative = new_path
                    .strip_prefix(&base)
                    .map_err(|_| Error::refused("area would leave volume"))?;
                let mut after = before.clone();
                after.folder = Some(
                    relative
                        .to_str()
                        .ok_or_else(|| Error::usage("area path is not UTF-8"))?
                        .to_string(),
                );
                after.validate()?;
                areas.push(MoveArea { before, after });
            }
        }
        let consumers = bindings.iter().map(|b| b.record.consumer.clone()).collect();
        let mut manual_steps = vec![
            "Update any unrecorded settings or workflows that refer to the old path.".to_string(),
        ];
        for area in &areas {
            manual_steps.push(format!(
                "Update the {} area declaration to use folder {}.",
                area.before.key().label(),
                area.after.effective_folder()
            ));
        }
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
            volume_spec: volume.spec().clone(),
            source,
            source_identity,
            destination,
            destination_parent_identity,
            create_directory,
            bindings,
            layout,
            areas,
            consumers,
            manual_steps,
        })
    }
}

/// Result of a move and its subsequent doctor report.
#[derive(Debug, Serialize)]
pub struct MoveReport {
    pub plan: MovePlan,
    pub doctor: crate::DoctorReport,
}

impl Barge {
    /// Apply exactly a reviewed plan, refusing drift and held roots.
    pub fn apply_move(&self, plan: &MovePlan, confirm_stopped: bool) -> Result<MoveReport> {
        self.core.ensure_writable()?;
        if plan.schema != "barge.move-plan/1" {
            return Err(Error::usage("unknown move plan schema"));
        }
        if !plan.consumers.is_empty() && !confirm_stopped {
            return Err(Error::refused(
                "bound consumers must be stopped; pass --confirm-stopped",
            ));
        }
        let _lock = self.core.state.lock()?;
        // Lock each affected area against new roots, then hold its roots without
        // waiting. This order avoids deadlock with eviction, which locks a root first.
        let mut area_locks = Vec::new();
        let mut root_locks = Vec::new();
        let mut partial_area = false;
        for area in self.areas()? {
            if area.volume().name() != plan.volume {
                continue;
            }
            let crate::Location::Volume(path) = area.location()? else {
                continue;
            };
            let base = canonical(&area.volume().base())?;
            let relative = path
                .strip_prefix(area.volume().base())
                .map_err(|_| Error::unknown_state("area is outside its volume"))?;
            let path = base.join(relative);
            if !path.starts_with(&plan.source) && !plan.source.starts_with(&path) {
                continue;
            }
            if plan.source.starts_with(&path) && plan.source != path {
                partial_area = true;
            }
            if path.exists() {
                area.ensure_layout(&path)?;
                area_locks.push(crate::area::AreaLock::acquire(&path)?);
            }
            for root in area.list_without_bytes()?.roots {
                if root.path.starts_with(&plan.source) || plan.source.starts_with(&root.path) {
                    let lock_path = root
                        .path
                        .parent()
                        .ok_or_else(|| Error::unknown_state("root has no area"))?
                        .join(".barge/locks")
                        .join(format!("{}.lock", root.id));
                    let file = crate::area::lock_root(&lock_path, crate::Mode::Exclusive, false)?
                        .ok_or_else(|| {
                        Error::refused(format!("root {} is held", root.path.display()))
                    })?;
                    root_locks.push(file);
                }
            }
        }
        if partial_area {
            return Err(Error::refused(
                "moving part of an area would orphan its roots",
            ));
        }
        let current = self.plan_move(
            &plan.volume,
            &plan.source,
            &plan.destination,
            plan.layout.clone(),
            plan.create_directory.as_deref(),
        )?;
        if &current != plan {
            return Err(Error::refused("move plan changed since review"));
        }
        let before_layout = self.layout()?;
        if let Some(change) = &plan.layout {
            if before_layout.classes.get(&change.class) != Some(&change.before)
                || change.before.volume != plan.volume
                || change.after.volume != plan.volume
            {
                return Err(Error::refused("class placement changed since review"));
            }
            let volume = self.volume(&plan.volume)?;
            if canonical(&volume.base().join(&change.before.path))? != plan.source
                || canonical(&volume.base())?.join(&change.after.path) != plan.destination
            {
                return Err(Error::refused("layout change does not match the rename"));
            }
            if self.layout_disagrees(&plan.volume)? {
                return Err(Error::refused("volume layout disagrees with account"));
            }
        }
        if let Some(directory) = &plan.create_directory {
            fs::DirBuilder::new()
                .mode(0o700)
                .create(directory)
                .map_err(|e| Error::io("cannot create class folder", directory, &e))?;
        }
        let mut records = crate::bindings::read(&self.core)?;
        let original_records = records.clone();
        crate::os::rename_no_replace(&plan.source, &plan.destination)
            .map_err(|e| Error::io("cannot rename", &plan.source, &e))?;
        let mut changed = Vec::new();
        let operation = (|| -> Result<()> {
            for binding in &plan.bindings {
                #[cfg(test)]
                FAIL_AFTER.with(|count| {
                    let remaining = count.get();
                    if remaining > 0 {
                        count.set(remaining - 1);
                    }
                });
                #[cfg(test)]
                if FAIL_AFTER.with(|count| count.get() == 1) {
                    return Err(Error::refused("injected binding failure"));
                }
                replace_link(&binding.link_after, &binding.after)?;
                changed.push(binding);
                let record = records
                    .iter_mut()
                    .find(|r| r.consumer == binding.record.consumer)
                    .ok_or_else(|| Error::unknown_state("binding disappeared"))?;
                record.applied.value = binding.after.clone();
                record.target = BindingTarget::Link {
                    path: binding.link_after.clone(),
                };
            }
            crate::bindings::write(&self.core, &records)?;
            for area in &plan.areas {
                self.core.state.write_area(&area.after)?;
            }
            if let Some(change) = &plan.layout {
                let mut layout = before_layout.clone();
                layout.classes.insert(change.class, change.after.clone());
                let mount = self.volume(&plan.volume)?.require_present()?;
                crate::layout::write(
                    &mount.join(".barge/layout.json"),
                    &layout.for_volume(&plan.volume),
                )?;
                crate::layout::write(&self.state_dir().join("layout.json"), &layout)?;
            }
            Ok(())
        })();
        if let Err(error) = operation {
            let mut failures = Vec::new();
            // Restore the data first, then the links. No rollback removes data.
            if let Err(e) = crate::os::rename_no_replace(&plan.destination, &plan.source) {
                // The source name was occupied or the filesystem refused recovery.
                // Keep links on the surviving destination when possible rather
                // than knowingly pointing them at the absent old source.
                for binding in &plan.bindings {
                    if let Err(problem) = replace_link(&binding.link_after, &binding.after) {
                        failures.push(format!(
                            "link {} could not be repaired: {problem}",
                            binding.link_after.display()
                        ));
                    }
                }
                return Err(Error::new(
                    error.kind(),
                    format!(
                        "{}; rollback rename failed: {e}; data remains at {}; {}",
                        error.message(),
                        plan.destination.display(),
                        failures.join("; ")
                    ),
                ));
            }
            for binding in changed.iter().rev() {
                if let Observed::Link { destination } = &binding.before {
                    if let Err(e) = replace_link(binding.record.link_path(), destination) {
                        failures.push(format!(
                            "link {} could not be restored: {e}",
                            binding.record.link_path().display()
                        ));
                    }
                }
            }
            if let Err(e) = crate::bindings::write(&self.core, &original_records) {
                failures.push(format!("binding records could not be restored: {e}"));
            }
            for area in &plan.areas {
                if let Err(e) = self.core.state.write_area(&area.before) {
                    failures.push(format!("area record could not be restored: {e}"));
                }
            }
            if plan.layout.is_some() {
                if let Ok(volume) = self.volume(&plan.volume) {
                    if let Err(e) = crate::layout::write(
                        &volume.spec().mount.join(".barge/layout.json"),
                        &before_layout.for_volume(&plan.volume),
                    ) {
                        failures.push(format!("volume layout could not be restored: {e}"));
                    }
                }
                if let Err(e) =
                    crate::layout::write(&self.state_dir().join("layout.json"), &before_layout)
                {
                    failures.push(format!("account layout could not be restored: {e}"));
                }
            }
            let detail = if failures.is_empty() {
                "data, links and records restored".to_string()
            } else {
                failures.join("; ")
            };
            return Err(Error::new(
                error.kind(),
                format!("{}; rollback: {detail}", error.message()),
            ));
        }
        drop(_lock);
        drop(root_locks);
        drop(area_locks);
        Ok(MoveReport {
            plan: plan.clone(),
            doctor: self.doctor()?,
        })
    }
}

/// Replace a symlink atomically using a unique sibling in the same directory.
fn replace_link(path: &Path, destination: &Path) -> Result<()> {
    use std::os::unix::fs::symlink;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let parent = path
        .parent()
        .ok_or_else(|| Error::usage("link has no parent"))?;
    for _ in 0..100 {
        let temporary = parent.join(format!(
            ".barge-move-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        match symlink(destination, &temporary) {
            Ok(()) => {
                if let Err(e) = fs::rename(&temporary, path) {
                    let _ = fs::remove_file(&temporary);
                    return Err(Error::io("cannot replace link", path, &e));
                }
                return Ok(());
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(Error::io("cannot create replacement link beside", path, &e)),
        }
    }
    Err(Error::refused("cannot allocate a temporary link name"))
}

#[cfg(test)]
thread_local! { static FAIL_AFTER: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Absent, Binding, FakeOs, ManualClock, VolumeRegistration};
    use std::os::unix::fs::symlink;
    use std::sync::Arc;

    #[test]
    fn binding_failure_restores_data_and_links() {
        let temp = crate::testing::TempDir::new("move-rollback").unwrap();
        let mount = temp.folder("volume").unwrap();
        let state = temp.folder("state").unwrap();
        let os = Arc::new(FakeOs::new());
        os.add_volume(
            mount.clone(),
            Some("11111111-2222-3333-4444-555555555555"),
            1000,
        );
        let b = Barge::open_with(state, os, Arc::new(ManualClock::new(1))).unwrap();
        b.register_volume(VolumeRegistration::new("volume", &mount).floor_bytes(0))
            .unwrap();
        b.set_layout(
            Class::Vms,
            Placement {
                volume: "volume".into(),
                path: "vms".into(),
                absent: Absent::Fail,
            },
        )
        .unwrap();
        fs::create_dir(mount.join("vms")).unwrap();
        for name in ["one", "two"] {
            fs::create_dir(mount.join("vms").join(name)).unwrap();
            let link = temp.path().join(name);
            symlink(mount.join("vms").join(name), &link).unwrap();
            b.bindings()
                .adopt(&Binding::link(name, Class::Vms, link))
                .unwrap();
        }
        let plan = b.plan_class_move(Class::Vms, "new-vms").unwrap();
        FAIL_AFTER.with(|count| count.set(3));
        let error = b.apply_move(&plan, true).unwrap_err();
        FAIL_AFTER.with(|count| count.set(0));
        assert!(error.message().contains("restored"));
        assert!(mount.join("vms").exists());
        assert!(!mount.join("new-vms").exists());
        for name in ["one", "two"] {
            assert_eq!(
                fs::read_link(temp.path().join(name)).unwrap(),
                mount.join("vms").join(name)
            );
        }
        assert!(b.doctor().unwrap().ok);
    }
}
