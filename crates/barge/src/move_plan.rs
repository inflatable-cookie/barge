//! Reviewed resource moves on a registered volume.
//!
//! A plan lists the data, every binding and area it carries, and any link in a
//! documented tool location ([`crate::known_links`]) that points into the moved
//! source and that no binding records. Such a link would be stranded by the
//! move, so applying the plan refuses until the caller passes
//! [`MoveApplyOptions::retarget_unrecorded`]; then those links are retargeted in
//! the same transaction and rolled back with everything else.

use std::fs::{self, File};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::bindings::{
    entry_identity, observe, same_target, BindingRecord, EntryIdentity, Observed,
};
use crate::{AreaRecord, Barge, BindingTarget, Class, Error, Placement, Result};

/// The schema name a reviewed move plan is written with.
const MOVE_PLAN_SCHEMA: &str = "barge.move-plan/2";

/// The filesystem identity captured for an existing entry.
///
/// A plan and a binding both mean the same thing by "the same entry": the
/// device-and-inode pair `EntryIdentity` records, so a move plan reuses it
/// instead of defining a second copy of the same two fields.
pub type MoveIdentity = EntryIdentity;

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
    /// The class the source belongs to, or is being moved into.
    pub class: Class,
    pub source: PathBuf,
    pub source_identity: MoveIdentity,
    pub destination: PathBuf,
    pub destination_parent_identity: MoveIdentity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub create_directory: Option<PathBuf>,
    pub bindings: Vec<MoveBinding>,
    /// Links in a documented tool location that point into the source and that no
    /// binding records. Apply refuses them unless the caller asks to retarget
    /// them.
    pub unrecorded_links: Vec<UnrecordedLink>,
    pub layout: Option<MoveLayout>,
    pub areas: Vec<MoveArea>,
    pub consumers: Vec<String>,
    pub manual_steps: Vec<String>,
}

/// A link in a documented tool location that points inside the moved source and
/// that no binding records, so the move would strand it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnrecordedLink {
    /// The documented location it was found at, for example `colima-disk`.
    pub location: String,
    /// The link itself, as it sits in the tool's own folder.
    pub link: PathBuf,
    /// What the link holds now, exactly as it was read, so a rollback restores
    /// the same bytes.
    pub before: PathBuf,
    /// What the link must hold once the source has moved.
    pub after: PathBuf,
    /// The consumer name an adoption should use.
    pub consumer: String,
    /// The exact command that records the link after the move.
    pub adopt: String,
}

/// How apply treats links that no binding records.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MoveApplyOptions {
    /// The operator confirms the bound consumers are stopped.
    pub confirm_stopped: bool,
    /// Retarget the unrecorded known links the plan listed, in the same
    /// transaction. Without it, a plan that lists any is refused.
    pub retarget_unrecorded: bool,
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

fn move_source(path: &Path) -> Result<PathBuf> {
    if fs::symlink_metadata(path)
        .map_err(|e| Error::io("cannot inspect", path, &e))?
        .file_type()
        .is_symlink()
    {
        return Err(Error::refused("move source must not be a symlink"));
    }
    canonical(path)
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
            class,
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
        let source = move_source(source)?;
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
            class,
            &source,
            &destination,
            None,
            Some(&class_dir),
        )
    }

    fn plan_move(
        &self,
        volume_name: &str,
        class: Class,
        source: &Path,
        destination: &Path,
        layout: Option<MoveLayout>,
        creatable: Option<&Path>,
    ) -> Result<MovePlan> {
        let volume = self.volume(volume_name)?;
        volume.require_present()?;
        let layout_record = self.layout()?;
        let base = canonical(&volume.base())?;
        let source = move_source(source)?;
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
                let class_folder = layout_record.classes.values().any(|placement| {
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
        let source_identity = entry_identity(&source)?;
        let destination_parent_identity = entry_identity(&identity_parent)?;
        if source_identity.dev != destination_parent_identity.dev {
            return Err(Error::refused(
                "move crosses a filesystem; copy is not supported",
            ));
        }
        for (other_class, placement) in &layout_record.classes {
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
        let records = self.bindings().list()?;
        for record in &records {
            // A workflow binding is not a link, and a move does not regenerate
            // its block: doctor reports the block stale until `barge init` runs.
            let BindingTarget::Link { path } = &record.target else {
                continue;
            };
            let link = path.as_path();
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
                    record: record.clone(),
                    before,
                    after,
                    link_after,
                });
            }
        }
        let unrecorded_links = self.unrecorded_links(&records, class, &source, &destination)?;
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
        for unrecorded in &unrecorded_links {
            manual_steps.push(format!(
                "Record the {} link at {}: {}",
                unrecorded.location,
                unrecorded.link.display(),
                unrecorded.adopt
            ));
        }
        for area in &areas {
            manual_steps.push(format!(
                "Update the {} area declaration to use folder {}.",
                area.before.key().label(),
                area.after.effective_folder()
            ));
        }
        manual_steps.extend(crate::tool_steps::for_move(&source, &destination));
        Ok(MovePlan {
            schema: MOVE_PLAN_SCHEMA.into(),
            volume: volume_name.into(),
            volume_spec: volume.spec().clone(),
            class,
            source,
            source_identity,
            destination,
            destination_parent_identity,
            create_directory,
            bindings,
            unrecorded_links,
            layout,
            areas,
            consumers,
            manual_steps,
        })
    }

    /// The known links under the home folder that point into `source` and that no
    /// binding records. Only the documented list is read, never the home folder
    /// at large.
    fn unrecorded_links(
        &self,
        records: &[BindingRecord],
        class: Class,
        source: &Path,
        destination: &Path,
    ) -> Result<Vec<UnrecordedLink>> {
        let Some(home) = self.core.home() else {
            return Ok(Vec::new());
        };
        let mut found = Vec::new();
        for candidate in crate::known_links::candidates(&home)? {
            if records
                .iter()
                .any(|record| same_target(&record.owned_path(), &candidate.path))
            {
                continue;
            }
            let metadata = match fs::symlink_metadata(&candidate.path) {
                Ok(metadata) => metadata,
                Err(source) if source.kind() == std::io::ErrorKind::NotFound => continue,
                Err(source) => return Err(Error::io("cannot inspect", &candidate.path, &source)),
            };
            if !metadata.file_type().is_symlink() {
                continue;
            }
            let before = fs::read_link(&candidate.path)
                .map_err(|source| Error::io("cannot read the link at", &candidate.path, &source))?;
            let resolved = resolve_link(&candidate.path, &before)?;
            let Ok(suffix) = resolved.strip_prefix(source) else {
                continue;
            };
            found.push(UnrecordedLink {
                location: candidate.known.location.to_string(),
                adopt: format!(
                    "barge bind adopt --consumer {} --class {} --link {}",
                    candidate.consumer,
                    class.name(),
                    shell_word(&candidate.path)
                ),
                consumer: candidate.consumer,
                link: candidate.path,
                before,
                after: destination.join(suffix),
            });
        }
        Ok(found)
    }
}

/// What a link points at, resolved when the target exists so an equivalent path
/// still counts, and read literally when it dangles.
fn resolve_link(link: &Path, target: &Path) -> Result<PathBuf> {
    let joined = if target.is_absolute() {
        target.to_path_buf()
    } else {
        let parent = link
            .parent()
            .ok_or_else(|| Error::usage(format!("link {} has no parent", link.display())))?;
        parent.join(target)
    };
    Ok(fs::canonicalize(&joined).unwrap_or(joined))
}

/// A path as one shell word, quoted only when it needs to be.
fn shell_word(path: &Path) -> String {
    let text = path.display().to_string();
    if text
        .chars()
        .any(|character| character.is_whitespace() || matches!(character, '\'' | '"' | '$' | '`'))
    {
        format!("'{}'", text.replace('\'', "'\\''"))
    } else {
        text
    }
}

/// Result of a move and its subsequent doctor report.
#[derive(Debug, Serialize)]
pub struct MoveReport {
    pub plan: MovePlan,
    pub doctor: Option<crate::DoctorReport>,
    pub doctor_error: Option<String>,
}

impl Barge {
    /// Apply exactly a reviewed plan, refusing drift and held roots. A plan that
    /// lists a known link no binding records is refused unless the caller asks to
    /// retarget those links in the same transaction.
    pub fn apply_move(&self, plan: &MovePlan, options: MoveApplyOptions) -> Result<MoveReport> {
        self.core.ensure_writable()?;
        check_move_request(plan, options)?;
        let locks = self.lock_move(plan)?;
        let before_layout = self.layout()?;
        self.revalidate_move(plan, &before_layout)?;
        let mut records = crate::bindings::read(&self.core)?;
        let originals = records.clone();
        let mut changes = MoveChanges::default();
        if let Err(error) = self.commit_move(plan, &before_layout, &mut records, &mut changes) {
            return match self.rollback_move(plan, &before_layout, &originals, &changes, &error) {
                Ok(detail) => Err(Error::new(
                    error.kind(),
                    format!("{}; rollback: {detail}", error.message()),
                )),
                Err(rollback) => Err(rollback),
            };
        }
        drop(locks);
        #[cfg(test)]
        let doctor_result = if FAIL_DOCTOR.with(|flag| flag.replace(false)) {
            Err(Error::refused("injected doctor failure"))
        } else {
            self.doctor()
        };
        #[cfg(not(test))]
        let doctor_result = self.doctor();
        let (doctor, doctor_error) = match doctor_result {
            Ok(report) => (Some(report), None),
            Err(error) => (None, Some(error.to_string())),
        };
        Ok(MoveReport {
            plan: plan.clone(),
            doctor,
            doctor_error,
        })
    }
}

/// The locks an applied plan holds while it commits: the account state, each
/// affected root's hold and each affected area's lock. The field order is the
/// order they are released when the commit ends, so the doctor report that
/// follows runs with none of them held.
struct MoveLocks {
    _state: crate::state::StateLock,
    _roots: Vec<File>,
    _areas: Vec<crate::area::AreaLock>,
}

/// What a commit changed, by reference into the plan it came from, so a
/// rollback can undo every step in reverse.
#[derive(Default)]
struct MoveChanges<'a> {
    bindings: Vec<&'a MoveBinding>,
    links: Vec<&'a UnrecordedLink>,
}

/// The refusals a plan must pass before anything is locked or written.
fn check_move_request(plan: &MovePlan, options: MoveApplyOptions) -> Result<()> {
    if plan.schema != MOVE_PLAN_SCHEMA {
        return Err(Error::usage("unknown move plan schema"));
    }
    if !plan.consumers.is_empty() && !options.confirm_stopped {
        return Err(Error::refused(
            "bound consumers must be stopped; pass --confirm-stopped",
        ));
    }
    if !plan.unrecorded_links.is_empty() && !options.retarget_unrecorded {
        return Err(Error::refused(format!(
            "{} link(s) point into the moved source and no binding records them ({}); \
             pass --retarget-unrecorded to retarget them in the same move",
            plan.unrecorded_links.len(),
            plan.unrecorded_links
                .iter()
                .map(|link| link.link.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    Ok(())
}

/// Test seam: count a link step down and fail when the counter reaches one, so a
/// rollback test can stop a commit at a known step.
#[cfg(test)]
fn fail_injected_binding() -> Result<()> {
    FAIL_AFTER.with(|count| {
        let remaining = count.get();
        if remaining > 0 {
            count.set(remaining - 1);
        }
    });
    if FAIL_AFTER.with(|count| count.get() == 1) {
        return Err(Error::refused("injected binding failure"));
    }
    Ok(())
}

#[cfg(not(test))]
fn fail_injected_binding() -> Result<()> {
    Ok(())
}

impl Barge {
    /// Lock the account state, then each affected area against new roots, then
    /// hold each of their roots without waiting. This order avoids deadlock with
    /// eviction, which locks a root first.
    fn lock_move(&self, plan: &MovePlan) -> Result<MoveLocks> {
        let state = self.core.state.lock()?;
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
                    let directory = root
                        .path
                        .parent()
                        .ok_or_else(|| Error::unknown_state("root has no area"))?;
                    let lock_path = crate::area::hold_lock_path(directory, &root.id);
                    let file = crate::area::lock_root_try(&lock_path, crate::Mode::Exclusive)?
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
        Ok(MoveLocks {
            _state: state,
            _roots: root_locks,
            _areas: area_locks,
        })
    }

    /// Re-plan under the locks and refuse unless the reviewed plan is still
    /// exactly what the account says.
    fn revalidate_move(
        &self,
        plan: &MovePlan,
        before_layout: &crate::layout::Layout,
    ) -> Result<()> {
        let current = self.plan_move(
            &plan.volume,
            plan.class,
            &plan.source,
            &plan.destination,
            plan.layout.clone(),
            plan.create_directory.as_deref(),
        )?;
        if &current != plan {
            return Err(Error::refused("move plan changed since review"));
        }
        let Some(change) = &plan.layout else {
            return Ok(());
        };
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
        Ok(())
    }

    /// Commit the plan: create the class folder, rename the data, then retarget
    /// every link and write the records. `changes` collects what was done so
    /// the rollback can undo it in reverse.
    fn commit_move<'a>(
        &self,
        plan: &'a MovePlan,
        before_layout: &crate::layout::Layout,
        records: &mut [BindingRecord],
        changes: &mut MoveChanges<'a>,
    ) -> Result<()> {
        if let Some(directory) = &plan.create_directory {
            fs::DirBuilder::new()
                .mode(0o700)
                .create(directory)
                .map_err(|e| Error::io("cannot create class folder", directory, &e))?;
        }
        crate::os::rename_no_replace(&plan.source, &plan.destination)
            .map_err(|e| Error::io("cannot rename", &plan.source, &e))?;
        for binding in &plan.bindings {
            fail_injected_binding()?;
            let Observed::Link {
                destination: before,
            } = &binding.before
            else {
                return Err(Error::unknown_state("move binding is not a link"));
            };
            replace_link(&binding.link_after, before, &binding.after)?;
            changes.bindings.push(binding);
            let record = records
                .iter_mut()
                .find(|record| record.consumer == binding.record.consumer)
                .ok_or_else(|| Error::unknown_state("binding disappeared"))?;
            record.applied.value = binding.after.clone();
            record.target = BindingTarget::Link {
                path: binding.link_after.clone(),
            };
        }
        for link in &plan.unrecorded_links {
            fail_injected_binding()?;
            replace_link(&link.link, &link.before, &link.after)?;
            changes.links.push(link);
        }
        crate::bindings::write(&self.core, records)?;
        for area in &plan.areas {
            self.core.state.write_area(&area.after)?;
        }
        if let Some(change) = &plan.layout {
            let mut layout = before_layout.clone();
            layout.classes.insert(change.class, change.after.clone());
            let mount = self.volume(&plan.volume)?.require_present()?;
            crate::layout::write(
                &mount.join(crate::layout::VOLUME_LAYOUT_FILE),
                &layout.for_volume(&plan.volume),
            )?;
            crate::layout::write(
                &self.state_dir().join(crate::layout::ACCOUNT_LAYOUT_FILE),
                &layout,
            )?;
        }
        Ok(())
    }

    /// Undo a failed commit: restore the data first, then the links, then the
    /// records. No rollback removes data.
    ///
    /// Returns the restored detail for the caller's error, or the composed
    /// error when the data could not be renamed back.
    fn rollback_move(
        &self,
        plan: &MovePlan,
        before_layout: &crate::layout::Layout,
        originals: &[BindingRecord],
        changes: &MoveChanges<'_>,
        error: &Error,
    ) -> Result<String> {
        let mut failures = Vec::new();
        if let Err(e) = crate::os::rename_no_replace(&plan.destination, &plan.source) {
            // The source name was occupied or the filesystem refused recovery.
            // Keep links on the surviving destination when possible rather than
            // knowingly pointing them at the absent old source.
            for binding in &plan.bindings {
                if changes.bindings.contains(&binding) {
                    continue;
                }
                let Observed::Link {
                    destination: before,
                } = &binding.before
                else {
                    failures.push(format!("binding {} is not a link", binding.record.consumer));
                    continue;
                };
                if let Err(problem) = replace_link(&binding.link_after, before, &binding.after) {
                    failures.push(format!(
                        "link {} could not be repaired: {problem}",
                        binding.link_after.display()
                    ));
                }
            }
            for link in &plan.unrecorded_links {
                if changes.links.contains(&link) {
                    continue;
                }
                if let Err(problem) = replace_link(&link.link, &link.before, &link.after) {
                    failures.push(format!(
                        "link {} could not be repaired: {problem}",
                        link.link.display()
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
        for binding in changes.bindings.iter().rev() {
            if let Observed::Link { destination } = &binding.before {
                if let Err(e) =
                    replace_link(&binding.record.owned_path(), &binding.after, destination)
                {
                    failures.push(format!(
                        "link {} could not be restored: {e}",
                        binding.record.owned_path().display()
                    ));
                }
            }
        }
        for link in changes.links.iter().rev() {
            if let Err(e) = replace_link(&link.link, &link.after, &link.before) {
                failures.push(format!(
                    "link {} could not be restored: {e}",
                    link.link.display()
                ));
            }
        }
        if let Err(e) = crate::bindings::write(&self.core, originals) {
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
                    &volume.spec().mount.join(crate::layout::VOLUME_LAYOUT_FILE),
                    &before_layout.for_volume(&plan.volume),
                ) {
                    failures.push(format!("volume layout could not be restored: {e}"));
                }
            }
            if let Err(e) = crate::layout::write(
                &self.state_dir().join(crate::layout::ACCOUNT_LAYOUT_FILE),
                before_layout,
            ) {
                failures.push(format!("account layout could not be restored: {e}"));
            }
        }
        Ok(if failures.is_empty() {
            "data, links and records restored".to_string()
        } else {
            failures.join("; ")
        })
    }
}

/// Replace a symlink atomically using a unique sibling in the same directory.
fn replace_link(path: &Path, expected: &Path, destination: &Path) -> Result<()> {
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
                #[cfg(test)]
                RETARGET_HOOK.with(|hook| {
                    if let Some(hook) = hook.borrow_mut().take() {
                        hook(&temporary);
                    }
                });
                let result = (|| {
                    match observe(path)? {
                        Observed::Link { destination } if destination == expected => {}
                        _ => {
                            return Err(Error::refused(format!(
                                "link {} changed before retargeting",
                                path.display()
                            )))
                        }
                    }
                    fs::rename(&temporary, path)
                        .map_err(|e| Error::io("cannot replace link", path, &e))
                })();
                if result.is_err() {
                    let _ = fs::remove_file(&temporary);
                }
                return result;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(Error::io("cannot create replacement link beside", path, &e)),
        }
    }
    Err(Error::refused("cannot allocate a temporary link name"))
}

#[cfg(test)]
type RetargetHook = Option<Box<dyn FnOnce(&Path)>>;
#[cfg(test)]
thread_local! { static RETARGET_HOOK: std::cell::RefCell<RetargetHook> = std::cell::RefCell::new(None); }
#[cfg(test)]
thread_local! { static FAIL_DOCTOR: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }
#[cfg(test)]
thread_local! { static FAIL_AFTER: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Absent, Binding, FakeOs, ManualClock, VolumeRegistration};
    use std::os::unix::fs::symlink;
    use std::sync::Arc;

    #[test]
    fn retarget_refuses_replaced_entries_and_removes_the_temporary_link() {
        let temp = crate::testing::TempDir::new("retarget-race").unwrap();
        let path = temp.child("link");
        let before = temp.child("before");
        let after = temp.child("after");
        for kind in ["file", "directory", "link"] {
            symlink(&before, &path).unwrap();
            let changed = path.clone();
            let other = temp.child("other");
            RETARGET_HOOK.with(|hook| {
                *hook.borrow_mut() = Some(Box::new(move |_| {
                    fs::remove_file(&changed).unwrap();
                    match kind {
                        "file" => fs::write(&changed, b"keep").unwrap(),
                        "directory" => fs::create_dir(&changed).unwrap(),
                        _ => symlink(&other, &changed).unwrap(),
                    }
                }))
            });
            assert!(replace_link(&path, &before, &after).is_err());
            let metadata = fs::symlink_metadata(&path).unwrap();
            match kind {
                "file" => assert_eq!(fs::read(&path).unwrap(), b"keep"),
                "directory" => assert!(metadata.is_dir()),
                _ => assert_eq!(fs::read_link(&path).unwrap(), temp.child("other")),
            }
            assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 1);
            if metadata.is_dir() {
                fs::remove_dir(&path).unwrap();
            } else {
                fs::remove_file(&path).unwrap();
            }
        }
        symlink(&before, &path).unwrap();
        RETARGET_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(|temporary| fs::remove_file(temporary).unwrap()))
        });
        assert!(replace_link(&path, &before, &after).is_err());
        assert_eq!(fs::read_link(&path).unwrap(), before);
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 1);
    }

    #[test]
    fn doctor_failure_does_not_undo_a_committed_move() {
        let temp = crate::testing::TempDir::new("move-doctor").unwrap();
        let mount = temp.folder("volume").unwrap();
        let state = temp.folder("state").unwrap();
        let os = Arc::new(FakeOs::with_volume(
            &mount,
            "11111111-2222-3333-4444-555555555555",
            1000,
        ));
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
        let plan = b.plan_class_move(Class::Vms, "new-vms").unwrap();
        FAIL_DOCTOR.with(|flag| flag.set(true));
        let report = b
            .apply_move(
                &plan,
                MoveApplyOptions {
                    confirm_stopped: true,
                    retarget_unrecorded: false,
                },
            )
            .unwrap();
        assert!(report.doctor.is_none());
        assert!(report.doctor_error.unwrap().contains("injected"));
        assert!(mount.join("new-vms").exists());
    }

    #[test]
    fn binding_failure_restores_data_and_links() {
        let temp = crate::testing::TempDir::new("move-rollback").unwrap();
        let mount = temp.folder("volume").unwrap();
        let state = temp.folder("state").unwrap();
        let home = temp.folder("home").unwrap();
        let os = Arc::new(FakeOs::new());
        os.add_volume(
            mount.clone(),
            Some("11111111-2222-3333-4444-555555555555"),
            1000,
        );
        let b = Barge::open_with(state, os, Arc::new(ManualClock::new(1)))
            .unwrap()
            .with_home(&home);
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
        let error = b
            .apply_move(
                &plan,
                MoveApplyOptions {
                    confirm_stopped: true,
                    retarget_unrecorded: false,
                },
            )
            .unwrap_err();
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

    #[test]
    fn unrecorded_link_failure_restores_data_and_links() {
        let temp = crate::testing::TempDir::new("move-unrecorded-rollback").unwrap();
        let mount = temp.folder("volume").unwrap();
        let state = temp.folder("state").unwrap();
        let home = temp.folder("home").unwrap();
        let os = Arc::new(FakeOs::new());
        os.add_volume(
            mount.clone(),
            Some("11111111-2222-3333-4444-555555555555"),
            1000,
        );
        let b = Barge::open_with(state, os, Arc::new(ManualClock::new(1)))
            .unwrap()
            .with_home(&home);
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
        fs::create_dir(mount.join("vms/colima")).unwrap();
        let disk = mount.join("vms/colima/disk");
        fs::write(&disk, b"disk-image").unwrap();
        let mut links = Vec::new();
        for profile in ["one", "two"] {
            let folder = home.join(".colima/_lima").join(profile);
            fs::create_dir_all(&folder).unwrap();
            let link = folder.join("disk");
            symlink(&disk, &link).unwrap();
            links.push(link);
        }
        let plan = b.plan_class_move(Class::Vms, "machines").unwrap();
        assert_eq!(plan.unrecorded_links.len(), 2);

        // Fail after the first link was retargeted, so the rollback has to put a
        // retargeted link back as well as the data and the records.
        FAIL_AFTER.with(|count| count.set(3));
        let error = b
            .apply_move(
                &plan,
                MoveApplyOptions {
                    confirm_stopped: false,
                    retarget_unrecorded: true,
                },
            )
            .unwrap_err();
        FAIL_AFTER.with(|count| count.set(0));
        assert!(error.message().contains("restored"), "{}", error.message());
        assert!(mount.join("vms/colima/disk").exists());
        assert!(!mount.join("machines").exists());
        for link in &links {
            assert_eq!(fs::read_link(link).unwrap(), disk);
        }
        assert!(b.doctor().unwrap().ok);
    }
}
