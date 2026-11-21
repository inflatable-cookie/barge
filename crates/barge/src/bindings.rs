//! Bindings: how one consumer is pointed at a class.
//!
//! A record lives in `bindings.json` under the account state directory
//! (`barge.bindings/1`). It names the consumer, the class, the kind, the target,
//! the value Barge applied and when, and what was there before, so undo restores
//! the prior state exactly. Writes take the state lock and publish by atomic
//! rename; reads never create anything.
//!
//! The `link` kind owns a symlink at the tool's own path. A real directory or
//! file is never replaced unless the plan showed it and the caller passed the
//! explicit replace; it is renamed aside and undo renames it back. Deleting a
//! link removes the link itself, never what it points at.
//!
//! A link may point at a folder or at a regular file, which is how a tool's disk
//! image is reached. Both kinds of destination are recorded with the device and
//! inode Barge linked, so doctor reports a destination that was replaced, not
//! just one the link no longer reaches.
//!
//! The `workflow` kind records the scratch file `barge init` writes into a
//! repository (`AGENTS.local.md`), so `barge doctor` can report it as current,
//! stale or missing. Its consumer is the repository path. `init` owns the file
//! but never replaces an entry, so the record keeps no earlier copy. Undo
//! forgets that record without changing the workflow file.

use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, ErrorKind, Result};
use crate::layout::Class;
use crate::state::{publish, validate_name};
use crate::{Barge, Core};

/// The schema name of `bindings.json`.
pub const BINDINGS_SCHEMA: &str = "barge.bindings/1";

/// The file that lists recorded bindings.
const BINDINGS_NAME: &str = "bindings.json";

/// The suffix a preserved directory or file is renamed to beside its link.
const PRESERVED_SUFFIX: &str = ".barge-prior";

/// How a consumer is pointed at a class. Only the kinds Barge records today are
/// listed: a stored record naming another kind is refused rather than guessed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BindingKind {
    /// Barge owns a symlink at the consumer's own path.
    Link,
    /// A scratch file `barge init` owns in a repository.
    Workflow,
}

impl BindingKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Link => "link",
            Self::Workflow => "workflow",
        }
    }
}

/// Where a binding points: the link Barge owns, or the repository whose scratch
/// file `init` owns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BindingTarget {
    Link {
        path: PathBuf,
    },
    Workflow {
        repository: PathBuf,
        project: String,
        /// The file in the repository that carries the scratch content. A record
        /// written before this field existed named the committed `AGENTS.md`.
        #[serde(default = "legacy_workflow_file")]
        file: String,
    },
}

/// The file a workflow record names when it predates the `file` field.
fn legacy_workflow_file() -> String {
    crate::workflow::COMMITTED_FILE.to_string()
}

/// The kind of entry a preserved target held.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    Directory,
    File,
}

/// The filesystem identity of a link's destination: the device and inode Barge
/// pointed the link at. A record kept before identities were captured has none,
/// and is then compared by path alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntryIdentity {
    pub dev: u64,
    pub ino: u64,
}

/// What was at the target before Barge applied the link. Undo restores it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Prior {
    /// Nothing was there; undo removes the link.
    Absent,
    /// A symlink with this exact target was there; undo recreates it.
    Link { destination: PathBuf },
    /// A real directory or file was there; it was renamed aside and undo renames
    /// it back.
    Preserved { path: PathBuf, entry: EntryKind },
    /// The file was merged into in place, as a workflow block is, so the record
    /// keeps no earlier copy. Undo refuses it.
    InPlace,
}

impl fmt::Display for Prior {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Absent => formatter.write_str("absent"),
            Self::Link { destination } => write!(formatter, "a link to {}", destination.display()),
            Self::Preserved { path, entry } => write!(
                formatter,
                "a preserved {} at {}",
                match entry {
                    EntryKind::Directory => "directory",
                    EntryKind::File => "file",
                },
                path.display()
            ),
            Self::InPlace => formatter.write_str("edited in place"),
        }
    }
}

/// The value Barge applied and when it applied it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Applied {
    /// The resolved class value the target now points at.
    pub value: PathBuf,
    pub at_unix_ms: u64,
    /// The device and inode the destination had when Barge applied it. Absent in
    /// a record written before identities were captured, and for a workflow
    /// block, which has no destination of its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<EntryIdentity>,
}

/// One recorded binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BindingRecord {
    pub consumer: String,
    pub class: Class,
    pub kind: BindingKind,
    pub target: BindingTarget,
    pub applied: Applied,
    pub prior: Prior,
}

impl BindingRecord {
    /// The path this record owns: the link Barge made, or the file carrying the
    /// workflow block.
    pub fn owned_path(&self) -> PathBuf {
        match &self.target {
            BindingTarget::Link { path } => path.clone(),
            BindingTarget::Workflow {
                repository, file, ..
            } => repository.join(file),
        }
    }
}

/// What is at a link target now. A real entry carries its device and inode, so a
/// plan can tell the reviewed entry from another one that appears later.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Observed {
    Absent,
    Link { destination: PathBuf },
    Directory { dev: u64, ino: u64 },
    File { dev: u64, ino: u64 },
}

impl fmt::Display for Observed {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Absent => formatter.write_str("absent"),
            Self::Link { destination } => write!(formatter, "a link to {}", destination.display()),
            Self::Directory { .. } => formatter.write_str("a directory"),
            Self::File { .. } => formatter.write_str("a file"),
        }
    }
}

/// A link binding the caller asks for, always passed explicitly: Barge never
/// discovers and adopts a binding by itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    consumer: String,
    class: Class,
    link: PathBuf,
    value: Option<PathBuf>,
}

impl Binding {
    /// A link binding for one consumer, class and link path. Apply also needs
    /// the value with [`Binding::to`]; adopt reads it from the existing link.
    pub fn link(consumer: impl Into<String>, class: Class, link: impl Into<PathBuf>) -> Self {
        Self {
            consumer: consumer.into(),
            class,
            link: link.into(),
            value: None,
        }
    }

    /// The class value the link should point at.
    pub fn to(mut self, value: impl Into<PathBuf>) -> Self {
        self.value = Some(value.into());
        self
    }

    pub fn consumer(&self) -> &str {
        &self.consumer
    }

    pub fn class(&self) -> Class {
        self.class
    }

    pub fn path(&self) -> &Path {
        &self.link
    }

    pub fn value(&self) -> Result<&Path> {
        self.value.as_deref().ok_or_else(|| {
            Error::usage(format!(
                "binding {} needs a destination; pass the class value",
                self.consumer
            ))
        })
    }

    fn validate(&self) -> Result<()> {
        validate_name("binding consumer", &self.consumer)?;
        if !self.link.is_absolute() {
            return Err(Error::usage(format!(
                "link path {} must be absolute",
                self.link.display()
            )));
        }
        match self.link.file_name() {
            Some(name) if name != "." && name != ".." => {}
            _ => {
                return Err(Error::usage(format!(
                    "link path {} needs a file name",
                    self.link.display()
                )))
            }
        }
        if let Some(value) = &self.value {
            if !value.is_absolute() {
                return Err(Error::usage(format!(
                    "link destination {} must be absolute",
                    value.display()
                )));
            }
        }
        Ok(())
    }
}

/// The exact before and after of a link binding; applying takes this plan back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkPlan {
    pub consumer: String,
    pub class: Class,
    pub link: PathBuf,
    pub before: Observed,
    pub after: Observed,
    /// The device and inode of the destination the plan resolved, so apply
    /// refuses a destination that was replaced since it was reviewed.
    pub destination_identity: EntryIdentity,
    /// The target is a real directory or file: applying needs the explicit
    /// replace and preserves it beside the link.
    pub replace: bool,
}

/// How apply treats a real directory or file at the target.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ApplyOptions {
    /// Preserve the real entry beside the link and create the link. Undo puts it
    /// back.
    pub replace: bool,
}

/// The recorded bindings of one account.
#[derive(Debug, Clone)]
pub struct Bindings {
    core: Core,
}

impl Barge {
    /// The account's recorded bindings.
    pub fn bindings(&self) -> Bindings {
        Bindings {
            core: self.core.clone(),
        }
    }
}

impl Bindings {
    /// Every recorded binding, in store order.
    pub fn list(&self) -> Result<Vec<BindingRecord>> {
        read(&self.core)
    }

    /// One record by consumer.
    pub fn get(&self, consumer: &str) -> Result<Option<BindingRecord>> {
        Ok(read(&self.core)?
            .into_iter()
            .find(|record| record.consumer == consumer))
    }

    /// The exact before and after of a link binding; writes nothing. The after
    /// value is the resolved destination, because that is what the link will
    /// hold.
    pub fn plan(&self, binding: &Binding) -> Result<LinkPlan> {
        binding.validate()?;
        let destination = self.destination(binding.class, binding.value()?)?;
        let destination_identity = entry_identity(&destination)?;
        require_parent(&binding.link)?;
        self.check_free(&binding.consumer, &binding.link)?;
        let before = observe(&binding.link)?;
        Ok(LinkPlan {
            consumer: binding.consumer.clone(),
            class: binding.class,
            link: binding.link.clone(),
            after: Observed::Link { destination },
            destination_identity,
            replace: matches!(before, Observed::Directory { .. } | Observed::File { .. }),
            before,
        })
    }

    /// Apply a reviewed plan. The target must still be the entry the plan
    /// observed, the destination must still resolve to what the plan recorded,
    /// and a real directory or file is only replaced when the caller passes it
    /// explicitly.
    pub fn apply(
        &self,
        binding: &Binding,
        plan: &LinkPlan,
        options: ApplyOptions,
    ) -> Result<BindingRecord> {
        self.core.ensure_writable()?;
        binding.validate()?;
        require_parent(&binding.link)?;
        let _lock = self.core.state.lock()?;
        let mut records = read(&self.core)?;
        self.check_free_among(&records, &binding.consumer, &binding.link)?;
        let observed = observe(&binding.link)?;
        if observed != plan.before {
            return Err(Error::refused(format!(
                "target {} changed since it was planned: the plan observed {}, found {}",
                binding.link.display(),
                plan.before,
                observed
            )));
        }
        let destination = self.destination(binding.class, binding.value()?)?;
        check_plan(binding, plan, &destination)?;
        let identity = entry_identity(&destination)?;
        if identity != plan.destination_identity {
            return Err(Error::refused(format!(
                "destination {} was replaced since it was planned",
                destination.display()
            )));
        }
        let prior = prior_for(&binding.link, &observed)?;
        if let Prior::Preserved { path, .. } = &prior {
            if !options.replace {
                return Err(Error::refused(format!(
                    "{} is {}; pass the explicit replace to preserve it and create the link",
                    binding.link.display(),
                    observed
                )));
            }
            if fs::symlink_metadata(path).is_ok() {
                return Err(Error::refused(format!(
                    "cannot preserve {}: {} already exists",
                    binding.link.display(),
                    path.display()
                )));
            }
        }
        make_way(&binding.link, &prior)?;
        #[cfg(test)]
        run_before_link_hook();
        if let Err(source) = symlink(&destination, &binding.link) {
            let mut message = format!(
                "cannot create the link at {}: {source}",
                binding.link.display()
            );
            // Nothing was created at the path, so only the prior entry moved
            // aside is put back. Removing a link here could delete one another
            // writer made in the same moment.
            if let Err(problem) = restore_prior(&binding.link, &prior) {
                message.push_str(&format!(
                    "; the prior state could not be restored: {}",
                    problem.message()
                ));
            }
            return Err(Error::io_message(message));
        }
        let record = BindingRecord {
            consumer: binding.consumer.clone(),
            class: binding.class,
            kind: BindingKind::Link,
            target: BindingTarget::Link {
                path: binding.link.clone(),
            },
            applied: Applied {
                value: destination,
                at_unix_ms: self.core.clock.now_unix_ms(),
                identity: Some(identity),
            },
            prior: prior.clone(),
        };
        records.push(record.clone());
        if let Err(error) = write(&self.core, &records) {
            if let Err(problem) = roll_back(&binding.link, &prior) {
                return Err(Error::unknown_state(format!(
                    "{}; the prior state could not be restored: {problem}",
                    error.message()
                )));
            }
            return Err(error);
        }
        Ok(record)
    }

    /// Record a symlink that already points into a class folder, without changing
    /// it. This is how today's `~/.cargo/{registry,git}` links become bindings.
    pub fn adopt(&self, binding: &Binding) -> Result<BindingRecord> {
        self.core.ensure_writable()?;
        binding.validate()?;
        let folder = canonical_dir(&self.class_folder(binding.class)?)?;
        let _lock = self.core.state.lock()?;
        let mut records = read(&self.core)?;
        self.check_free_among(&records, &binding.consumer, &binding.link)?;
        let observed = observe(&binding.link)?;
        let Observed::Link { destination } = observed else {
            return Err(Error::refused(format!(
                "there is no symlink at {} to adopt; it is {}",
                binding.link.display(),
                observed
            )));
        };
        let resolved = fs::canonicalize(&binding.link)
            .map_err(|source| Error::io("cannot resolve the link at", &binding.link, &source))?;
        if !resolved.starts_with(&folder) {
            return Err(Error::refused(format!(
                "the link at {} points at {}, outside the {} class folder {}",
                binding.link.display(),
                resolved.display(),
                binding.class.name(),
                folder.display()
            )));
        }
        if let Some(value) = &binding.value {
            let requested = self.destination(binding.class, value)?;
            if requested != resolved {
                return Err(Error::refused(format!(
                    "the link at {} points at {}, not the requested {}",
                    binding.link.display(),
                    resolved.display(),
                    requested.display()
                )));
            }
        }
        let record = BindingRecord {
            consumer: binding.consumer.clone(),
            class: binding.class,
            kind: BindingKind::Link,
            target: BindingTarget::Link {
                path: binding.link.clone(),
            },
            applied: Applied {
                value: resolved.clone(),
                at_unix_ms: self.core.clock.now_unix_ms(),
                identity: Some(entry_identity(&resolved)?),
            },
            prior: Prior::Link { destination },
        };
        records.push(record.clone());
        write(&self.core, &records)?;
        Ok(record)
    }

    /// Undo one consumer's binding, restoring exactly what was there before and
    /// removing the record. A `workflow` binding has no earlier copy to restore,
    /// so undo only forgets the record and leaves its file untouched.
    pub fn undo(&self, consumer: &str) -> Result<BindingRecord> {
        self.core.ensure_writable()?;
        let _lock = self.core.state.lock()?;
        let mut records = read(&self.core)?;
        let index = match records
            .iter()
            .position(|record| record.consumer == consumer)
        {
            Some(index) => index,
            None => {
                validate_name("binding consumer", consumer)?;
                return Err(Error::not_found(format!(
                    "consumer {consumer} has no binding"
                )));
            }
        };
        let record = records[index].clone();
        if record.kind == BindingKind::Workflow {
            records.remove(index);
            write(&self.core, &records)?;
            return Ok(record);
        }
        let link = record.owned_path();
        match observe(&link)? {
            Observed::Absent => {}
            Observed::Link { destination } => {
                // The record keeps the resolved destination, while the link keeps
                // whatever text it was written with: a relative link, one through
                // a symlinked folder or a macOS `/var` path reads differently.
                // Compare what the link resolves to, as doctor does. A link whose
                // destination is unreachable dangles, so it is matched on the text
                // Barge wrote (`applied`) or the text adopt found (`prior`).
                let resolved = fs::canonicalize(&link).ok();
                let still_ours = resolved.as_deref() == Some(record.applied.value.as_path())
                    || destination == record.applied.value
                    || matches!(&record.prior, Prior::Link { destination: prior } if prior == &destination);
                if !still_ours {
                    return Err(Error::refused(format!(
                        "the link at {} now points at {}, not the applied {}; refusing to undo it",
                        link.display(),
                        resolved
                            .as_deref()
                            .unwrap_or(destination.as_path())
                            .display(),
                        record.applied.value.display()
                    )));
                }
                remove_link(&link)?;
            }
            other => {
                return Err(Error::refused(format!(
                    "the target {} is {} now, not the link Barge applied; refusing to undo it",
                    link.display(),
                    other
                )))
            }
        }
        if let Err(error) = restore_prior(&link, &record.prior) {
            return Err(failed_undo(error, reapply_applied(&link, &record)));
        }
        records.remove(index);
        if let Err(error) = write(&self.core, &records) {
            return Err(failed_undo(error, reapply_applied(&link, &record)));
        }
        Ok(record)
    }

    /// Refuse a consumer that is already recorded, and a link another consumer
    /// already owns; both would make undo ambiguous.
    fn check_free(&self, consumer: &str, link: &Path) -> Result<()> {
        self.check_free_among(&read(&self.core)?, consumer, link)
    }

    fn check_free_among(
        &self,
        records: &[BindingRecord],
        consumer: &str,
        link: &Path,
    ) -> Result<()> {
        if records.iter().any(|record| record.consumer == consumer) {
            return Err(Error::refused(format!(
                "consumer {consumer} is already bound; undo it first"
            )));
        }
        if let Some(record) = records
            .iter()
            .find(|record| same_target(&record.owned_path(), link))
        {
            return Err(Error::refused(format!(
                "target {} is already recorded for consumer {}; undo it first",
                link.display(),
                record.consumer
            )));
        }
        Ok(())
    }

    /// The class folder a binding's value must sit inside, refusing an absent or
    /// mismatched volume rather than guessing.
    fn class_folder(&self, class: Class) -> Result<PathBuf> {
        let layout = self.core.account_layout()?;
        let placement = layout.classes.get(&class).ok_or_else(|| {
            Error::not_found(format!(
                "class {} has no layout; set one with `barge layout set`",
                class.name()
            ))
        })?;
        let volume = self.core.volume(&placement.volume)?;
        volume.require_present()?;
        Ok(volume.base().join(&placement.path))
    }

    /// The resolved class value, refused unless it is a folder or a regular file
    /// inside the class folder. A symlink that leaves the class folder is never
    /// reached: the value is resolved first and checked as it really is.
    fn destination(&self, class: Class, value: &Path) -> Result<PathBuf> {
        let folder = canonical_dir(&self.class_folder(class)?)?;
        let destination = match fs::canonicalize(value) {
            Ok(path) => path,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::refused(format!(
                    "link destination {} does not exist; Barge does not create it",
                    value.display()
                )))
            }
            Err(source) => return Err(Error::io("cannot resolve", value, &source)),
        };
        if !is_folder_or_file(&destination) {
            return Err(Error::refused(format!(
                "link destination {} is not a folder or a regular file",
                value.display()
            )));
        }
        if !destination.starts_with(&folder) {
            return Err(Error::refused(format!(
                "link destination {} is outside the {} class folder {}",
                value.display(),
                class.name(),
                folder.display()
            )));
        }
        Ok(destination)
    }
}

/// Check that a plan is the one made for this binding, observes what its replace
/// flag claims, and still resolves to the destination the plan recorded.
fn check_plan(binding: &Binding, plan: &LinkPlan, destination: &Path) -> Result<()> {
    if plan.consumer != binding.consumer || plan.class != binding.class || plan.link != binding.link
    {
        return Err(Error::usage("the plan was made for a different binding"));
    }
    match &plan.after {
        Observed::Link {
            destination: planned,
        } if planned == destination => {}
        Observed::Link {
            destination: planned,
        } => {
            return Err(Error::refused(format!(
                "destination {} now resolves to {}, not the planned {}; refusing to apply a changed plan",
                binding.value()?.display(),
                destination.display(),
                planned.display()
            )))
        }
        other => {
            return Err(Error::usage(format!(
                "the plan's after is {other}, not a link"
            )))
        }
    }
    if plan.replace
        != matches!(
            plan.before,
            Observed::Directory { .. } | Observed::File { .. }
        )
    {
        return Err(Error::usage(
            "the plan's replace flag does not match what it observed",
        ));
    }
    Ok(())
}

/// A failed undo names what went wrong and whether the applied link is back.
fn failed_undo(error: Error, reapplied: Result<()>) -> Error {
    let mut message = error.message().to_string();
    let mut kind = error.kind();
    if let Err(problem) = reapplied {
        kind = ErrorKind::UnknownState;
        message.push_str(&format!(
            "; the applied link could not be restored: {}",
            problem.message()
        ));
    }
    Error::new(kind, message)
}

/// What was at `link` before it is replaced, with the name a preserved entry is
/// renamed to.
fn prior_for(link: &Path, observed: &Observed) -> Result<Prior> {
    Ok(match observed {
        Observed::Absent => Prior::Absent,
        Observed::Link { destination } => Prior::Link {
            destination: destination.clone(),
        },
        Observed::Directory { .. } => Prior::Preserved {
            path: preserved_path(link)?,
            entry: EntryKind::Directory,
        },
        Observed::File { .. } => Prior::Preserved {
            path: preserved_path(link)?,
            entry: EntryKind::File,
        },
    })
}

/// Move the observed entry out of the way so the link can be created.
fn make_way(link: &Path, prior: &Prior) -> Result<()> {
    match prior {
        Prior::Absent => Ok(()),
        Prior::Link { .. } => remove_link(link),
        Prior::Preserved { path, .. } => crate::os::rename_no_replace(link, path)
            .map_err(|source| Error::io("cannot preserve the entry at", link, &source)),
        Prior::InPlace => Err(Error::refused(
            "an in-place change has no entry to move aside",
        )),
    }
}

/// Restore the prior state after the link has been removed.
fn restore_prior(link: &Path, prior: &Prior) -> Result<()> {
    match prior {
        Prior::Absent => Ok(()),
        Prior::Link { destination } => symlink(destination, link)
            .map_err(|source| Error::io("cannot restore the prior link at", link, &source)),
        Prior::Preserved { path, .. } => {
            match fs::symlink_metadata(path) {
                Ok(_) => {}
                Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                    return Err(Error::refused(format!(
                        "the preserved entry {} is gone; refusing to restore the link",
                        path.display()
                    )))
                }
                Err(source) => return Err(Error::io("cannot inspect", path, &source)),
            }
            crate::os::rename_no_replace(path, link)
                .map_err(|source| Error::io("cannot restore the preserved entry to", link, &source))
        }
        Prior::InPlace => Err(Error::refused(
            "an in-place change has no earlier entry to restore",
        )),
    }
}

/// Remove the link and restore what was there before, after a failure.
fn roll_back(link: &Path, prior: &Prior) -> Result<()> {
    remove_link(link)?;
    restore_prior(link, prior)
}

/// Put the applied link back after an undo could not finish, moving a restored
/// real entry back to where it was preserved. A link or nothing may be removed;
/// a real entry this record does not name is refused, never deleted.
fn reapply_applied(link: &Path, record: &BindingRecord) -> Result<()> {
    match fs::symlink_metadata(link) {
        Ok(metadata) if metadata.file_type().is_symlink() => remove_link(link)?,
        Ok(_) => match &record.prior {
            Prior::Preserved { path, .. } => {
                if fs::symlink_metadata(path).is_ok() {
                    return Err(Error::refused(format!(
                        "both {} and {} hold an entry; refusing to choose",
                        link.display(),
                        path.display()
                    )));
                }
                crate::os::rename_no_replace(link, path).map_err(|source| {
                    Error::io("cannot move the restored entry back to", path, &source)
                })?;
            }
            _ => {
                return Err(Error::refused(format!(
                    "{} holds a real entry the record does not name; refusing to remove it",
                    link.display()
                )))
            }
        },
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => return Err(Error::io("cannot inspect", link, &source)),
    }
    symlink(&record.applied.value, link)
        .map_err(|source| Error::io("cannot restore the applied link at", link, &source))
}

/// Whether two link paths name the same target, resolving the folder above them
/// so `sub/../x` and a symlinked folder still count as one target.
pub(crate) fn same_target(left: &Path, right: &Path) -> bool {
    match (target_identity(left), target_identity(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

fn target_identity(link: &Path) -> Result<PathBuf> {
    let parent = link
        .parent()
        .ok_or_else(|| Error::usage(format!("link path {} has no parent", link.display())))?;
    let name = link
        .file_name()
        .ok_or_else(|| Error::usage(format!("link path {} has no file name", link.display())))?;
    let parent =
        fs::canonicalize(parent).map_err(|source| Error::io("cannot resolve", parent, &source))?;
    Ok(parent.join(name))
}

fn preserved_path(link: &Path) -> Result<PathBuf> {
    let parent = link
        .parent()
        .ok_or_else(|| Error::usage(format!("link path {} has no parent", link.display())))?;
    let name = link
        .file_name()
        .ok_or_else(|| Error::usage(format!("link path {} has no file name", link.display())))?;
    let mut preserved = name.to_os_string();
    preserved.push(PRESERVED_SUFFIX);
    Ok(parent.join(preserved))
}

fn require_parent(link: &Path) -> Result<()> {
    let parent = link.parent().ok_or_else(|| {
        Error::usage(format!("link path {} has no parent folder", link.display()))
    })?;
    match fs::metadata(parent) {
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => Err(Error::refused(format!(
            "the folder above {} is not a directory",
            link.display()
        ))),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            Err(Error::refused(format!(
                "the folder above {} does not exist; Barge does not create it",
                link.display()
            )))
        }
        Err(source) => Err(Error::io("cannot inspect", parent, &source)),
    }
}

/// Remove a symlink itself, never what it points at.
fn remove_link(link: &Path) -> Result<()> {
    match fs::symlink_metadata(link) {
        Ok(metadata) if metadata.file_type().is_symlink() => fs::remove_file(link)
            .map_err(|source| Error::io("cannot remove the link at", link, &source)),
        Ok(_) => Err(Error::refused(format!(
            "{} is not a symlink; refusing to remove it",
            link.display()
        ))),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(Error::io("cannot inspect", link, &source)),
    }
}

fn is_directory(path: &Path) -> bool {
    fs::metadata(path)
        .map(|metadata| metadata.is_dir())
        .unwrap_or(false)
}

fn is_folder_or_file(path: &Path) -> bool {
    fs::metadata(path)
        .map(|metadata| {
            let kind = metadata.file_type();
            kind.is_dir() || kind.is_file()
        })
        .unwrap_or(false)
}

/// The device and inode of an existing entry.
pub(crate) fn entry_identity(path: &Path) -> Result<EntryIdentity> {
    use std::os::unix::fs::MetadataExt;

    let metadata =
        fs::metadata(path).map_err(|source| Error::io("cannot inspect", path, &source))?;
    Ok(EntryIdentity {
        dev: metadata.dev(),
        ino: metadata.ino(),
    })
}

fn canonical_dir(path: &Path) -> Result<PathBuf> {
    let canonical =
        fs::canonicalize(path).map_err(|source| Error::io("cannot resolve", path, &source))?;
    if !is_directory(&canonical) {
        return Err(Error::refused(format!(
            "{} is not a directory",
            path.display()
        )));
    }
    Ok(canonical)
}

/// What is at `path` now, without following a symlink there. A real entry keeps
/// its device and inode, so a later observation tells whether it is the same one.
pub(crate) fn observe(path: &Path) -> Result<Observed> {
    use std::os::unix::fs::MetadataExt;

    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Ok(Observed::Link {
            destination: fs::read_link(path)
                .map_err(|source| Error::io("cannot read the link at", path, &source))?,
        }),
        Ok(metadata) if metadata.is_dir() => Ok(Observed::Directory {
            dev: metadata.dev(),
            ino: metadata.ino(),
        }),
        Ok(metadata) => Ok(Observed::File {
            dev: metadata.dev(),
            ino: metadata.ino(),
        }),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(Observed::Absent),
        Err(source) => Err(Error::io("cannot inspect", path, &source)),
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BindingsDocument {
    schema: String,
    bindings: Vec<BindingRecord>,
}

pub(crate) fn read(core: &Core) -> Result<Vec<BindingRecord>> {
    let path = core.state.root().join(BINDINGS_NAME);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(Error::io("cannot read", &path, &source)),
    };
    let document: BindingsDocument = serde_json::from_slice(&bytes).map_err(|source| {
        Error::unknown_state(format!("cannot parse {}: {source}", path.display()))
    })?;
    if document.schema != BINDINGS_SCHEMA {
        return Err(Error::unknown_state(format!(
            "{} has schema {}, not {BINDINGS_SCHEMA}",
            path.display(),
            document.schema
        )));
    }
    let mut consumers = BTreeSet::new();
    for record in &document.bindings {
        validate_record(record)?;
        if !consumers.insert(record.consumer.clone()) {
            return Err(Error::unknown_state(format!(
                "{} records consumer {} twice",
                path.display(),
                record.consumer
            )));
        }
    }
    Ok(document.bindings)
}

pub(crate) fn write(core: &Core, records: &[BindingRecord]) -> Result<()> {
    let mut consumers = BTreeSet::new();
    for record in records {
        validate_record(record)?;
        if !consumers.insert(record.consumer.clone()) {
            return Err(Error::unknown_state(format!(
                "refusing to record consumer {} twice",
                record.consumer
            )));
        }
    }
    let document = BindingsDocument {
        schema: BINDINGS_SCHEMA.to_string(),
        bindings: records.to_vec(),
    };
    let bytes = serde_json::to_vec_pretty(&document)
        .map_err(|source| Error::unknown_state(format!("cannot encode bindings: {source}")))?;
    publish(&core.state.root().join(BINDINGS_NAME), &bytes)
}

pub(crate) fn validate_record(record: &BindingRecord) -> Result<()> {
    match (&record.kind, &record.target) {
        (BindingKind::Link, BindingTarget::Link { path }) => {
            validate_name("binding consumer", &record.consumer)
                .map_err(|error| Error::unknown_state(error.message()))?;
            if !path.is_absolute() {
                return Err(Error::unknown_state(format!(
                    "binding {} has a relative link path {}",
                    record.consumer,
                    path.display()
                )));
            }
        }
        (
            BindingKind::Workflow,
            BindingTarget::Workflow {
                repository,
                project,
                file,
            },
        ) => {
            if file != crate::workflow::LOCAL_FILE && file != crate::workflow::COMMITTED_FILE {
                return Err(Error::unknown_state(format!(
                    "workflow binding {} names an unknown file {file}",
                    record.consumer
                )));
            }
            if !Path::new(&record.consumer).is_absolute() {
                return Err(Error::unknown_state(format!(
                    "workflow binding consumer {} is not a repository path",
                    record.consumer
                )));
            }
            if !repository.is_absolute() {
                return Err(Error::unknown_state(format!(
                    "workflow binding {} has a relative repository {}",
                    record.consumer,
                    repository.display()
                )));
            }
            validate_name("workflow project", project).map_err(|error| {
                Error::unknown_state(format!(
                    "workflow binding {} has an invalid project: {}",
                    record.consumer,
                    error.message()
                ))
            })?;
            if record.class != Class::Scratch {
                return Err(Error::unknown_state(format!(
                    "workflow binding {} is for class {}, not scratch",
                    record.consumer,
                    record.class.name()
                )));
            }
        }
        (kind, _) => {
            return Err(Error::unknown_state(format!(
                "binding {} has kind {}, which this version does not record",
                record.consumer,
                kind.name()
            )))
        }
    }
    if !record.applied.value.is_absolute() {
        return Err(Error::unknown_state(format!(
            "binding {} has a relative applied value {}",
            record.consumer,
            record.applied.value.display()
        )));
    }
    match &record.prior {
        Prior::Absent => {}
        Prior::Link { destination } => {
            if destination.as_os_str().is_empty() {
                return Err(Error::unknown_state(format!(
                    "binding {} records an empty prior link",
                    record.consumer
                )));
            }
        }
        Prior::Preserved { path, .. } => {
            if !path.is_absolute() {
                return Err(Error::unknown_state(format!(
                    "binding {} records a relative preserved path {}",
                    record.consumer,
                    path.display()
                )));
            }
        }
        Prior::InPlace => {
            if record.kind != BindingKind::Workflow {
                return Err(Error::unknown_state(format!(
                    "binding {} records an in-place change for a {} binding",
                    record.consumer,
                    record.kind.name()
                )));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
thread_local! {
    /// A test seam in `apply`, after the prior entry is moved aside and before
    /// the link is created: it lets a test make that creation fail the way a
    /// racing writer does, which no temporary directory can produce on demand.
    static BEFORE_LINK_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

/// Run the one queued seam, if a test set one.
#[cfg(test)]
fn run_before_link_hook() {
    BEFORE_LINK_HOOK.with(|hook| {
        let callback = hook.borrow_mut().take();
        if let Some(callback) = callback {
            callback();
        }
    });
}

/// Queue one seam for the next link creation.
#[cfg(test)]
fn push_before_link_hook(hook: impl FnOnce() + 'static) {
    BEFORE_LINK_HOOK.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
}

#[cfg(test)]
mod rollback_kind_tests {
    use super::*;

    #[test]
    fn undo_forgets_a_workflow_binding_without_changing_its_file() {
        use crate::{Absent, FakeOs, ManualClock, VolumeRegistration};
        use std::sync::Arc;

        let temp = crate::testing::TempDir::new("binding-workflow-undo").unwrap();
        let mount = temp.folder("volume").unwrap();
        let state = temp.folder("state").unwrap();
        let repository = temp.folder("repository").unwrap();
        let barge = Barge::open_with(
            state,
            Arc::new(FakeOs::with_volume(
                &mount,
                "11111111-2222-3333-4444-555555555555",
                1000,
            )),
            Arc::new(ManualClock::new(1)),
        )
        .unwrap();
        barge
            .register_volume(VolumeRegistration::new("volume", &mount).floor_bytes(0))
            .unwrap();
        barge
            .set_layout(
                Class::Scratch,
                crate::Placement {
                    volume: "volume".into(),
                    path: "scratch".into(),
                    absent: Absent::Fail,
                },
            )
            .unwrap();
        barge.init(&repository, None).unwrap();

        let file = repository.join(crate::workflow::LOCAL_FILE);
        let before = fs::read(&file).unwrap();
        let record = barge
            .bindings()
            .undo(&repository.to_string_lossy())
            .unwrap();

        assert_eq!(record.kind, BindingKind::Workflow);
        assert_eq!(fs::read(&file).unwrap(), before);
        assert!(barge.bindings().list().unwrap().is_empty());
    }

    #[test]
    fn preservation_refuses_a_new_destination() {
        let temp = crate::testing::TempDir::new("binding-race").unwrap();
        let link = temp.child("entry");
        let preserved = temp.child("preserved");
        std::fs::write(&link, b"original").unwrap();
        std::fs::write(&preserved, b"concurrent").unwrap();
        let prior = Prior::Preserved {
            path: preserved.clone(),
            entry: EntryKind::File,
        };
        assert!(make_way(&link, &prior).is_err());
        assert_eq!(std::fs::read(&preserved).unwrap(), b"concurrent");
        assert_eq!(std::fs::read(&link).unwrap(), b"original");
    }
    #[test]
    fn a_failed_link_creation_never_removes_a_link_it_did_not_make() {
        let temp = crate::testing::TempDir::new("binding-failed-create").unwrap();
        let link = temp.child("entry");
        let preserved = temp.child("entry.barge-prior");
        std::fs::write(&preserved, b"original").unwrap();
        // `make_way` moved the prior entry aside and the link creation failed;
        // in that moment another writer put its own link at the path. Restoring
        // the prior entry must refuse rather than delete that link.
        std::fs::write(temp.child("elsewhere"), b"theirs").unwrap();
        symlink(temp.child("elsewhere"), &link).unwrap();
        let prior = Prior::Preserved {
            path: preserved.clone(),
            entry: EntryKind::File,
        };
        assert!(restore_prior(&link, &prior).is_err());
        assert_eq!(fs::read_link(&link).unwrap(), temp.child("elsewhere"));
        assert!(preserved.exists(), "the prior entry is still preserved");
    }

    #[test]
    fn apply_that_cannot_create_the_link_keeps_another_writers_link() {
        use crate::{Absent, FakeOs, ManualClock, Placement, VolumeRegistration};
        use std::sync::Arc;

        let temp = crate::testing::TempDir::new("binding-apply-create").unwrap();
        let mount = temp.folder("volume").unwrap();
        let state = temp.folder("state").unwrap();
        let home = temp.folder("home").unwrap();
        let barge = Barge::open_with(
            state,
            Arc::new(FakeOs::with_volume(
                &mount,
                "11111111-2222-3333-4444-555555555555",
                1000,
            )),
            Arc::new(ManualClock::new(1)),
        )
        .unwrap();
        barge
            .register_volume(VolumeRegistration::new("volume", &mount).floor_bytes(0))
            .unwrap();
        barge
            .set_layout(
                Class::Caches,
                Placement {
                    volume: "volume".into(),
                    path: "caches".into(),
                    absent: Absent::Fail,
                },
            )
            .unwrap();
        let destination = mount.join("caches/cargo");
        fs::create_dir_all(&destination).unwrap();
        let link = home.join("registry");
        let old = home.join("old-place");
        fs::create_dir(&old).unwrap();
        symlink(&old, &link).unwrap();

        let bindings = barge.bindings();
        let binding = Binding::link("cargo-registry", Class::Caches, &link).to(&destination);
        let plan = bindings.plan(&binding).unwrap();
        // `apply` moves the prior link aside, and in that moment another writer
        // puts its own link at the path: creating Barge's link then fails, and
        // the failure path must leave that link alone.
        let theirs = home.join("their-place");
        fs::create_dir(&theirs).unwrap();
        let raced = link.clone();
        let planted = theirs.clone();
        push_before_link_hook(move || symlink(&planted, &raced).unwrap());

        let error = bindings
            .apply(&binding, &plan, ApplyOptions::default())
            .unwrap_err();

        assert!(
            error.message().contains("cannot create the link at"),
            "{}",
            error.message()
        );
        assert_eq!(
            fs::read_link(&link).unwrap(),
            theirs,
            "the other writer's link is never removed"
        );
        assert!(old.is_dir(), "the prior link's destination is untouched");
        assert!(
            bindings.list().unwrap().is_empty(),
            "a failed apply records nothing"
        );
    }

    #[test]
    fn failed_reapply_reports_unknown_state() {
        let error = failed_undo(
            Error::refused("write refused"),
            Err(Error::refused("restore refused")),
        );
        assert_eq!(error.kind(), ErrorKind::UnknownState);
    }
}
