//! Cooperative holds: an advisory `flock` on `<area>/.barge/locks/<id>.lock`.
//!
//! Barge never scans processes. A root is idle exactly when its lock can be
//! taken. A consumer that keeps the descriptor inheritable passes the hold to its
//! children, so the root stays held until the whole process tree has exited; a
//! consumer that closes the descriptor, or writes without holding, is outside the
//! guarantee and the architecture says so.

use std::fs::File;
use std::os::unix::io::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::area::{AreaLock, ACTIVITY_DIR, BARGE_DIR, LOCKS_DIR};
use crate::clock::Clock;
use crate::error::{Error, Result};
use crate::root::{read_marker, tree_bytes, MarkerRead, RootMarker};
use crate::state::publish;

/// The schema name of an activity record.
pub const ACTIVITY_SCHEMA: &str = "barge.activity/2";
const PREVIOUS_ACTIVITY_SCHEMA: &str = "barge.activity/1";

/// How a consumer holds a root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Only one exclusive holder; eviction and prune refuse while it is held.
    Exclusive,
    /// Any number of shared holders, for example a seeding donor.
    Shared,
}

/// A held root lock. Dropping it records the release time and releases the lock
/// when this process's last descriptor for it closes.
#[derive(Debug)]
pub struct Hold {
    root_id: String,
    mode: Mode,
    file: Option<File>,
    root_path: PathBuf,
    marker: crate::root::RootMarker,
    root_directory: File,
    clock: Arc<dyn Clock>,
}

impl Hold {
    pub(crate) fn new(
        mode: Mode,
        file: File,
        root_path: PathBuf,
        marker: crate::root::RootMarker,
        root_directory: File,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            root_id: marker.id.clone(),
            mode,
            file: Some(file),
            root_path,
            marker,
            root_directory,
            clock,
        }
    }

    pub fn root_id(&self) -> &str {
        &self.root_id
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// The descriptor a child process can inherit to keep the hold.
    pub fn raw_fd(&self) -> Option<RawFd> {
        self.file.as_ref().map(AsRawFd::as_raw_fd)
    }

    /// Make the descriptor inheritable (clear close-on-exec), so a child that
    /// keeps it keeps the hold after `exec`.
    pub fn set_inheritable(&self, inheritable: bool) -> Result<()> {
        let file = self
            .file
            .as_ref()
            .ok_or_else(|| Error::unknown_state("hold has no descriptor"))?;
        crate::os::set_inheritable(file, inheritable)
    }

    /// Consume the hold and leave its descriptor inheritable, so a child that
    /// keeps it keeps the hold across `exec`.
    ///
    /// Oxide's output holds use this to hand selected roots to a child process.
    pub fn into_inheritable(self) -> Result<Self> {
        self.set_inheritable(true)?;
        Ok(self)
    }
}

impl Drop for Hold {
    fn drop(&mut self) {
        // Release first: a consumer waiting for the lock must not wait for this
        // walk, and an eviction may follow the release immediately. The recorded
        // size is advisory, so a tree that changes under it is not a problem.
        self.file.take();
        #[cfg(test)]
        DROP_HOOK.with(|hook| {
            if let Some(callback) = hook.borrow_mut().take() {
                callback();
            }
        });
        let now = self.clock.now_unix_ms();
        let size = tree_bytes(&self.root_path);
        // Only a complete measurement is cached, and an incomplete one clears any
        // earlier cached size: a stale number must never read back as exact.
        let recorded = if size.approximate {
            RecordedSize::Unknown
        } else {
            RecordedSize::Exact(size.bytes)
        };
        let Some(directory) = self.root_path.parent() else {
            return;
        };
        // The release itself has already happened, so recording the size waits
        // for the area lock only briefly and is skipped, with its reason, rather
        // than hanging: the record is advisory and a release must never block.
        let marker = &self.marker;
        let measurement = Measurement {
            root_id: &self.root_id,
            path: &self.root_path,
            identity: crate::os::file_identity(&self.root_directory).ok(),
            size: recorded,
            unix_ms: now,
        };
        let skipped =
            record_size_if_unchanged(directory, &measurement, |current| current == marker);
        if let Some(reason) = skipped {
            eprintln!(
                "barge: not recording the size of {}: {reason}",
                self.root_path.display()
            );
        }
    }
}

/// Whether another process (or another descriptor here) holds the lock file.
///
/// A missing lock file is an unheld root. A lock file that cannot be opened is
/// an error: unknown is never safe.
///
/// The probe takes the exclusive lock for the moment it reads it, because a
/// shared probe cannot tell a shared holder from an idle root. A `try_hold`
/// racing a probe can therefore fail spuriously once; it retries, and a real
/// holder still reports as held here.
pub(crate) fn is_locked(path: &Path) -> Result<bool> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(source) => return Err(Error::io("cannot open the hold lock", path, &source)),
    };
    Ok(!crate::os::flock(&file, Mode::Exclusive, false)?)
}

/// The hold lock file a root's ID uses inside an area directory.
pub(crate) fn hold_lock_path(directory: &Path, id: &str) -> PathBuf {
    directory
        .join(BARGE_DIR)
        .join(LOCKS_DIR)
        .join(format!("{id}.lock"))
}

/// Open a hold lock and take it, waiting for every other holder to finish.
///
/// Eviction unlinks `<id>.lock` while it holds it, so a waiting hold can wake
/// holding a file nobody else can see; a caller-chosen folder re-adopted with the
/// same ID would then reopen and lock the same root twice. A descriptor that does
/// not match the path is therefore dropped and the attempt repeated once. A
/// waiting `flock` returns only once the lock is held, so that repeat is the only
/// case that reaches a second attempt.
pub(crate) fn lock_root_waiting(lock_path: &Path, mode: Mode) -> Result<File> {
    let mut attempt = 0;
    while attempt < 2 {
        attempt += 1;
        let file = open_hold_lock(lock_path)?;
        crate::os::flock(&file, mode, true)?;
        run_lock_hook();
        if lock_is_current(&file, lock_path)? {
            return Ok(file);
        }
    }
    Err(replaced_lock(lock_path))
}

/// Open a hold lock and take it only if it is free. `None` means another holder
/// has it.
pub(crate) fn lock_root_try(lock_path: &Path, mode: Mode) -> Result<Option<File>> {
    let mut attempt = 0;
    while attempt < 2 {
        attempt += 1;
        let file = open_hold_lock(lock_path)?;
        if !crate::os::flock(&file, mode, false)? {
            return Ok(None);
        }
        run_lock_hook();
        if lock_is_current(&file, lock_path)? {
            return Ok(Some(file));
        }
    }
    Err(replaced_lock(lock_path))
}

/// Open the hold lock file, past the window an eviction exploits: the file is
/// open, the lock is not yet taken, so an eviction may rename the root, unlink
/// this file and a caller may re-adopt the folder with the same ID before the
/// lock lands.
fn open_hold_lock(lock_path: &Path) -> Result<File> {
    let file = crate::os::open_lock(lock_path, "cannot open the hold lock")?;
    #[cfg(test)]
    LOCK_OPEN_HOOK.with(|hooks| {
        let hook = hooks.borrow_mut().pop();
        if let Some(hook) = hook {
            hook();
        }
    });
    Ok(file)
}

/// The test seam after a lock lands, before the descriptor is verified.
#[cfg(test)]
fn run_lock_hook() {
    LOCK_HOOK.with(|hooks| {
        let hook = hooks.borrow_mut().pop();
        if let Some(hook) = hook {
            hook();
        }
    });
}

#[cfg(not(test))]
fn run_lock_hook() {}

fn replaced_lock(lock_path: &Path) -> Error {
    Error::unknown_state(format!(
        "the hold lock {} was replaced while it was being taken; refusing to hold it",
        lock_path.display()
    ))
}

fn lock_is_current(file: &File, path: &Path) -> Result<bool> {
    let held = crate::os::file_identity(file)
        .map_err(|source| Error::io("cannot inspect the hold lock", path, &source))?;
    let now = crate::os::path_identity(path)
        .map_err(|source| Error::io("cannot inspect the hold lock", path, &source))?;
    Ok(now == Some(held))
}

/// What a call records about a root's size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecordedSize {
    /// The caller measured nothing: keep whatever complete size is stored.
    Unchanged,
    /// A measurement that saw the whole tree.
    Exact(u64),
    /// A measurement that did not: forget any stored size, because a later
    /// reader would otherwise report an old number as an exact one.
    Unknown,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ActivityRecord {
    schema: String,
    root_id: String,
    last_used_unix_ms: u64,
    /// The root's size, recorded only when a walk saw the whole tree: a lower
    /// bound is never cached where a later reader would report it as exact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    root_bytes: Option<u64>,
}

/// Record a root's last use. The record is advisory: a failure is not a reason
/// to fail the hold, it only makes the root look older to prune.
pub(crate) fn record_activity(
    activity_dir: &Path,
    root_id: &str,
    unix_ms: u64,
    size: RecordedSize,
) -> Result<()> {
    let root_bytes = match size {
        RecordedSize::Unchanged => {
            read_activity_record(activity_dir, root_id).and_then(|record| record.root_bytes)
        }
        RecordedSize::Exact(bytes) => Some(bytes),
        RecordedSize::Unknown => None,
    };
    let record = ActivityRecord {
        schema: ACTIVITY_SCHEMA.to_string(),
        root_id: root_id.to_string(),
        last_used_unix_ms: unix_ms,
        root_bytes,
    };
    let bytes = serde_json::to_vec_pretty(&record).map_err(|source| {
        Error::unknown_state(format!("cannot encode an activity record: {source}"))
    })?;
    publish(&activity_path(activity_dir, root_id), &bytes)
}

/// Read a root's last use, when a readable record exists.
pub(crate) fn read_activity(activity_dir: &Path, root_id: &str) -> Option<u64> {
    read_activity_record(activity_dir, root_id).map(|record| record.last_used_unix_ms)
}

/// A root's last complete size measurement, when one was recorded.
pub(crate) fn read_activity_bytes(activity_dir: &Path, root_id: &str) -> Option<u64> {
    read_activity_record(activity_dir, root_id).and_then(|record| record.root_bytes)
}

fn read_activity_record(activity_dir: &Path, root_id: &str) -> Option<ActivityRecord> {
    let bytes = std::fs::read(activity_path(activity_dir, root_id)).ok()?;
    let record: ActivityRecord = serde_json::from_slice(&bytes).ok()?;
    if !matches!(
        record.schema.as_str(),
        ACTIVITY_SCHEMA | PREVIOUS_ACTIVITY_SCHEMA
    ) || record.root_id != root_id
    {
        return None;
    }
    Some(record)
}

pub(crate) fn activity_path(activity_dir: &Path, root_id: &str) -> PathBuf {
    activity_dir.join(format!("{root_id}.json"))
}

/// How long a release waits for the area lock before it skips its size record.
/// The record is advisory and the hold is already released, so a release never
/// waits on another process for longer than this.
const RELEASE_LOCK_WAIT: Duration = Duration::from_millis(50);

/// What a measurement saw, so it can be recorded only while it still describes
/// the root it was taken from.
pub(crate) struct Measurement<'a> {
    /// The root's ID, and the folder it was found in.
    pub root_id: &'a str,
    /// The folder that was measured.
    pub path: &'a Path,
    /// The identity read before the measurement, if the folder was there.
    pub identity: Option<(u64, u64)>,
    /// The size, exact or unknown, and when it was taken.
    pub size: RecordedSize,
    pub unix_ms: u64,
}

/// Record a measured root's size, but only while the folder is still the entry
/// that was measured and its marker still names the root.
///
/// This is the one path by which a size is recorded: the area lock serialises it
/// against adoption, eviction and a marker write. Returns `None` when the record
/// was written, or the reason it was skipped, which a release reports rather than
/// hides.
pub(crate) fn record_size_if_unchanged(
    directory: &Path,
    measurement: &Measurement<'_>,
    marker_names_root: impl FnOnce(&RootMarker) -> bool,
) -> Option<&'static str> {
    let _lock = match AreaLock::acquire_within(directory, RELEASE_LOCK_WAIT) {
        Ok(Some(lock)) => lock,
        Ok(None) => return Some("the area lock stayed busy"),
        Err(_) => return Some("the area lock could not be opened"),
    };
    if measurement.identity.is_none()
        || crate::os::path_identity(measurement.path).ok().flatten() != measurement.identity
    {
        return Some("the root was replaced while it was measured");
    }
    if !matches!(read_marker(measurement.path), MarkerRead::Valid(ref marker) if marker_names_root(marker))
    {
        return Some("the root's marker changed while it was measured");
    }
    let activity = directory.join(BARGE_DIR).join(ACTIVITY_DIR);
    match record_activity(
        &activity,
        measurement.root_id,
        measurement.unix_ms,
        measurement.size,
    ) {
        Ok(()) => None,
        Err(_) => Some("the size could not be recorded"),
    }
}

#[cfg(test)]
thread_local! {
    static LOCK_HOOK: std::cell::RefCell<Vec<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(Vec::new()) };
    static LOCK_OPEN_HOOK: std::cell::RefCell<Vec<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// A test hook that runs between opening a hold lock file and taking its lock.
#[cfg(test)]
pub(crate) fn push_lock_open_hook(hook: impl FnOnce() + 'static) {
    LOCK_OPEN_HOOK.with(|hooks| hooks.borrow_mut().push(Box::new(hook)));
}

/// A test hook that runs after a hold lock is taken, before it is verified.
#[cfg(test)]
pub(crate) fn push_lock_hook(hook: impl FnOnce() + 'static) {
    LOCK_HOOK.with(|hooks| hooks.borrow_mut().push(Box::new(hook)));
}

#[cfg(test)]
thread_local! {
    pub(crate) static DROP_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = std::cell::RefCell::new(None);
}
