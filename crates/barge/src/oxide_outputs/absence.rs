//! Absence-only recovery for an unavailable caller with no original request.

use std::path::{Component, Path};

use super::{quarantine_leaf, validate_claim, LiveIntent, AREA_NAME, AREA_NAMESPACE};
use crate::area;
use crate::os::{AbsenceVolume, DirOps, MountRoot, ProcessObserver, VolumeUuid};
use crate::registry;
use crate::state::{self, Deadline, LockSet, StatePaths};

/// An absence-only observation. No variant supplies a recovery record or
/// permission to execute, replay, settle or remove either journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AbsenceLookup {
    /// The complete Barge journal contains no record for this caller id and
    /// the verified outputs folder contains no ambiguous quarantine leaf.
    /// Oxide must additionally prove its original root's recorded identity
    /// before rollback; this fact alone is never rollback authority.
    PositivelyAbsent,
    /// A record, quarantine leaf, invalid binding or incomplete observation
    /// prevents a positive absence. Keep the caller's intent pending.
    Unknown {
        /// The reason no positive absence was established.
        reason: String,
    },
}

/// Observe Barge-side absence without manufacturing a [`super::DelegatedRequest`].
///
/// The caller validates its tagged unavailable metadata and supplies its
/// original live claim. This function binds the volume, caller id and exact
/// declared area location, but does not validate Oxide sidecars or prove the
/// original root's identity. Any matching pending or terminal record refuses;
/// obtaining a found record still requires the full checked [`super::lookup`].
/// Every quarantine leaf is ambiguous without a Barge operation binding.
///
/// The existing shared volume observation lock excludes an in-flight Barge
/// writer. Journal and folder observations share the reference wall bound;
/// lock refusal or exhausted bounds are unknown and never absence.
pub fn lookup_absence(
    state_paths: &StatePaths,
    dirs: &dyn DirOps,
    facts: &dyn AbsenceVolume,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    intent: &LiveIntent,
) -> AbsenceLookup {
    match observe_absence(state_paths, dirs, facts, mounts, observer, intent) {
        Ok(()) => AbsenceLookup::PositivelyAbsent,
        Err(reason) => AbsenceLookup::Unknown { reason },
    }
}

fn observe_absence(
    state_paths: &StatePaths,
    dirs: &dyn DirOps,
    facts: &dyn AbsenceVolume,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    intent: &LiveIntent,
) -> std::result::Result<(), String> {
    let uuid = validate_claim(intent)?;
    let deadline = Deadline::after(super::REFERENCE_WALL);
    let mut locks = LockSet::with_deadline(state_paths.clone(), Deadline::after(state::LOCK_WAIT));
    let _observation = locks
        .observe_volume(&uuid)
        .map_err(|error| format!("the volume observation lock could not be taken: {error}"))?;
    validate_location(state_paths, facts, intent, &uuid)?;
    // The generic reader treats a missing directory as an empty store. This
    // recovery query requires an actually readable journal, never that default.
    let metadata = std::fs::symlink_metadata(state_paths.intents_dir())
        .map_err(|error| format!("the intent journal directory cannot be read: {error}"))?;
    if !metadata.is_dir() {
        return Err("the intent journal is not a real directory".to_owned());
    }
    let journal =
        state::read_intents_with_deadline(state_paths, crate::contract::INTENT_ROWS, deadline)
            .map_err(|error| format!("the intent journal cannot be read as a whole: {error}"))?;
    if journal.truncated {
        return Err("the intent journal is truncated, so absence cannot be established".to_owned());
    }
    if journal
        .records
        .iter()
        .any(|record| record.caller_intent_id.as_deref() == Some(intent.id.as_str()))
    {
        return Err("a pending or terminal record names this caller id; checked request recovery is required".to_owned());
    }
    quarantine_leaf(state_paths, dirs, facts, mounts, observer, &uuid, None)?;
    if deadline.expired() {
        return Err("the absence observation deadline expired".to_owned());
    }
    Ok(())
}

/// Bind the original path to exactly one root immediately in the registered
/// outputs folder. A remount may change only the identifiable mount prefix.
fn validate_location(
    state_paths: &StatePaths,
    facts: &dyn AbsenceVolume,
    intent: &LiveIntent,
    uuid: &VolumeUuid,
) -> std::result::Result<(), String> {
    let registry = registry::read(state_paths)
        .map_err(|error| format!("the recovery volume binding cannot be read: {error}"))?;
    let volume = registry
        .volume(uuid)
        .ok_or_else(|| "the recovery volume is not registered".to_owned())?;
    let document = area::show(state_paths, AREA_NAMESPACE, AREA_NAME)
        .map_err(|error| format!("the recovery area binding cannot be read: {error}"))?
        .ok_or_else(|| "the recovery outputs area is not declared".to_owned())?;
    let declaration = document.declaration();
    let folder = declaration
        .folder_for(uuid)
        .ok_or_else(|| "the recovery area has no folder on this volume".to_owned())?;
    let root = &intent.identity.root;
    let prefix = facts
        .volume_prefix(root)
        .ok_or_else(|| "the original root's mount prefix is unidentifiable".to_owned())?;
    let relative = root
        .strip_prefix(prefix)
        .map_err(|_| "the original root is outside its mount prefix".to_owned())?;
    let parent = Path::new(&volume.root_dir).join(folder);
    let name = relative
        .strip_prefix(&parent)
        .map_err(|_| "the original root is outside the declared outputs folder".to_owned())?;
    let mut components = name.components();
    if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
        return Err("the original root is not one direct outputs root".to_owned());
    }
    // Components normalizes `.`; reject those and `..` in the source spelling
    // as well, so a malformed path never acquires a canonical interpretation.
    if root
        .as_os_str()
        .as_encoded_bytes()
        .split(|byte| *byte == b'/')
        .any(|component| component == b"." || component == b"..")
    {
        return Err("the original root has a non-canonical path component".to_owned());
    }
    Ok(())
}
