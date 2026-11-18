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

use serde::{Deserialize, Serialize};

use crate::clock::Clock;
use crate::error::{Error, Result};
use crate::root::tree_bytes;
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

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Exclusive => "exclusive",
            Self::Shared => "shared",
        }
    }
}

/// A held root lock. Dropping it records the release time and releases the lock
/// when this process's last descriptor for it closes.
#[derive(Debug)]
pub struct Hold {
    root_id: String,
    mode: Mode,
    file: Option<File>,
    activity_dir: PathBuf,
    root_path: PathBuf,
    marker: crate::root::RootMarker,
    root_directory: File,
    clock: Arc<dyn Clock>,
}

impl Hold {
    pub(crate) fn new(
        mode: Mode,
        file: File,
        activity_dir: PathBuf,
        root_path: PathBuf,
        marker: crate::root::RootMarker,
        root_directory: File,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            root_id: marker.id.clone(),
            mode,
            file: Some(file),
            activity_dir,
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

    /// Consume the hold and leave its descriptor inheritable.
    pub fn into_inheritable(self) -> Result<Self> {
        self.set_inheritable(true)?;
        Ok(self)
    }

    /// A human-readable description, for the CLI summary.
    pub fn describe(&self) -> String {
        format!("{} hold on {}", self.mode.as_str(), self.root_id)
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
        if let Some(directory) = self.root_path.parent() {
            if let Ok(_lock) = crate::area::AreaLock::acquire(directory) {
                if crate::os::file_identity(&self.root_directory)
                    .ok()
                    .is_some_and(|identity| {
                        crate::os::path_identity(&self.root_path).ok().flatten() == Some(identity)
                    })
                    && matches!(crate::root::read_marker(&self.root_path), crate::root::MarkerRead::Valid(ref marker) if marker == &self.marker)
                {
                    let _ = record_activity(&self.activity_dir, &self.root_id, now, recorded);
                }
            }
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

#[cfg(test)]
thread_local! {
    pub(crate) static DROP_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = std::cell::RefCell::new(None);
}
