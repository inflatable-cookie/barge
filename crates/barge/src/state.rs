//! Account-wide state: the state directory, its layout, atomic publish, bounded
//! reads and the fixed lock order (stage 1 design, "Account-wide state
//! directory" and "Bounded resources").
//!
//! State is account-wide, keyed by the password-database home for the effective
//! uid and never by `$HOME`, so two shells or a fixture `HOME` cannot produce two
//! authorities. [`StatePaths::resolve`] takes the platform's account facts and,
//! for fixtures and proofs only, the documented `BARGE_STATE_DIR` override.
//!
//! Three mechanics live here, and every later slice uses these rather than its
//! own copy:
//!
//! - [`publish_atomic`]: a private `create_new` temporary file in the target's
//!   own directory, sync, rename, then sync the directory. A failure before the
//!   rename leaves the previous bytes and removes the temporary file.
//! - [`read_bounded`]: a strict, no-follow read with a size cap, an entry cap and
//!   a deadline. A file that is absent, oversized, linked, not a regular file or
//!   out of time is reported as exactly that; nothing is defaulted.
//! - [`LockSet`]: file locks in the one order that cannot deadlock — registry,
//!   then volume, then root, then the intent write. Acquiring out of order is
//!   refused, and [`LockSet::observe_registry`] takes only a shared lock and only
//!   when the lock file already exists, so a read-only command creates nothing.
//! - [`IntentRecord`]: one durable `barge.intent/2` phase record for a deletion
//!   unit. Slice S5 writes every phase before the effect it authorises, so a
//!   crash leaves a pending intent that replay resolves from the recorded unit
//!   identity and never from an inference; `barge.intent/1` stays readable and
//!   maps to a Barge-marked directory unit. Stage 4 F4 adds two optional
//!   documents - the plan's identity evidence and the consumer path's
//!   `barge.absence/1` proof - so a replay says what was proved and
//!   re-establishes it instead of trusting a past verdict.

use std::fs::{File, OpenOptions};
use std::io::{ErrorKind as IoErrorKind, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
// The metadata facts the cfg(test) tape rows record; nothing else in this
// module names the trait.
#[cfg(test)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::{Error, ErrorKind, Result};
use crate::os::{AccountFacts, VolumeUuid};

/// The documented fixture-only override of the state directory. A normal run
/// leaves it unset, so a stray environment cannot move the account's authority.
pub const STATE_DIR_ENV: &str = "BARGE_STATE_DIR";

/// The documented fixture-only override of the account home.
///
/// State and a placement's original paths resolve from the password database's
/// home, never from `$HOME`, so a fixture or a native proof cannot isolate
/// itself by moving `HOME` alone. This variable is that seam's sibling of
/// [`STATE_DIR_ENV`]: it exists for fixtures and the native proof, a normal run
/// leaves it unset, and setting it moves only the account home — never the
/// volume registry, which still needs [`STATE_DIR_ENV`] or a separate account.
pub const ACCOUNT_HOME_ENV: &str = "BARGE_ACCOUNT_HOME";

/// The read deadline for one bounded file (design, "Bounded resources").
pub const READ_DEADLINE: Duration = Duration::from_secs(10);

/// How long a lock acquisition waits for a live owner before refusing `locked`.
pub const LOCK_WAIT: Duration = Duration::from_secs(5);

/// The mode a state directory is created with: private to this account.
const DIRECTORY_MODE: u32 = 0o700;

/// The mode a state file is created with.
const FILE_MODE: u32 = 0o600;

/// How much of a file is read between deadline checks.
const READ_CHUNK_BYTES: usize = 64 * 1024;

/// The pause between lock attempts while waiting for a live owner.
const LOCK_POLL: Duration = Duration::from_millis(20);

/// How many temporary names one publish tries before refusing.
const TEMP_ATTEMPTS: usize = 8;

/// Distinguishes temporary names issued in one process.
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Where a state directory path came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StateDirSource {
    /// The platform's account state root, derived from the password database.
    Account,
    /// `BARGE_STATE_DIR`, which exists for fixtures and proofs only.
    EnvironmentOverride,
}

/// The account's state directory and, with it, every path inside the layout.
///
/// Resolution reads the account facts and nothing else, so the environment can
/// only move state through the documented override. The directory itself is not
/// created here: [`StatePaths::initialize`] is the one creating call, and only a
/// mutating command reaches it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatePaths {
    root: PathBuf,
    source: StateDirSource,
}

impl StatePaths {
    /// Resolve the account's state directory.
    ///
    /// `override_dir` is `BARGE_STATE_DIR`, honoured exactly as the design says:
    /// it exists for a fixture or a proof and is never consulted in a normal run.
    /// A relative override is refused rather than resolved against a working
    /// directory, because a relative authority is not an authority.
    pub fn resolve(account: &dyn AccountFacts, override_dir: Option<&Path>) -> Result<Self> {
        match override_dir {
            Some(dir) if !dir.is_absolute() => Err(Error::new(
                ErrorKind::Usage,
                format!(
                    "{STATE_DIR_ENV} must be an absolute path; `{}` is relative",
                    dir.display()
                ),
            )
            .with_detail("state_dir", dir.display().to_string().into())),
            Some(dir) => Ok(Self {
                root: dir.to_path_buf(),
                source: StateDirSource::EnvironmentOverride,
            }),
            None => Ok(Self {
                root: account.state_root()?,
                source: StateDirSource::Account,
            }),
        }
    }

    /// The state directory path, as resolved and not canonicalised: it may not
    /// exist yet, and resolving it creates nothing.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where the path came from.
    pub fn source(&self) -> StateDirSource {
        self.source
    }

    /// The account volume registry.
    pub fn registry_path(&self) -> PathBuf {
        self.root.join("registry.json")
    }

    /// The area declarations, one directory per namespace.
    pub fn areas_dir(&self) -> PathBuf {
        self.root.join("areas")
    }

    /// The placement records.
    pub fn placements_dir(&self) -> PathBuf {
        self.root.join("placements")
    }

    /// The hold records, one directory per root.
    pub fn holds_dir(&self) -> PathBuf {
        self.root.join("holds")
    }

    /// The activity records, one file per root (stage 3).
    pub fn activity_dir(&self) -> PathBuf {
        self.root.join("activity")
    }

    /// One root's activity record path. A malformed root id is refused: a record
    /// file is never named from an unvalidated string.
    pub fn activity_path(&self, root_id: &str) -> Result<PathBuf> {
        validate_identifier("root_id", root_id)?;
        Ok(self.activity_dir().join(format!("{root_id}.json")))
    }

    /// The intent journal.
    pub fn intents_dir(&self) -> PathBuf {
        self.root.join("intents")
    }

    /// The lock files.
    pub fn locks_dir(&self) -> PathBuf {
        self.root.join("locks")
    }

    /// The bounded append-only prune and delete observations.
    pub fn journal_dir(&self) -> PathBuf {
        self.root.join("journal")
    }

    /// The registry lock file, first in the fixed order.
    pub fn registry_lock_path(&self) -> PathBuf {
        self.locks_dir().join("registry.lock")
    }

    /// One volume's lock file, second in the fixed order.
    pub fn volume_lock_path(&self, volume_uuid: &VolumeUuid) -> PathBuf {
        self.locks_dir().join(format!("volume-{volume_uuid}.lock"))
    }

    /// One root's lock file, third in the fixed order. A malformed root id is
    /// refused: a lock file is never named from an unvalidated string.
    pub fn root_lock_path(&self, root_id: &str) -> Result<PathBuf> {
        validate_identifier("root_id", root_id)?;
        Ok(self.locks_dir().join(format!("root-{root_id}.lock")))
    }

    /// One deletion unit's lock file, third in the fixed order.
    ///
    /// A unit whose name is a Barge root id takes that root's lock, which is the
    /// lock a hold on it takes too; any other unit is locked under the key
    /// [`unit_lock_key`] derives. The key is a SHA-256, so a unit name can never
    /// name a path outside `locks/`.
    pub fn unit_lock_path(&self, key: &str) -> Result<PathBuf> {
        if key.len() != 64 || !key.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(Error::new(
                ErrorKind::UnknownState,
                format!("`{key}` is not a 64-hex-digit unit lock key"),
            )
            .with_detail("unit_lock_key", key.to_owned().into()));
        }
        Ok(self.locks_dir().join(format!("unit-{key}.lock")))
    }

    /// Whether the state directory exists as a directory. An unreadable path or
    /// a non-directory at that path is unknown state, never "not initialized".
    pub fn is_initialized(&self) -> Result<bool> {
        match std::fs::metadata(&self.root) {
            Ok(metadata) if metadata.is_dir() => Ok(true),
            Ok(_) => Err(Error::new(
                ErrorKind::UnknownState,
                format!(
                    "the state directory path {} is not a directory",
                    self.root.display()
                ),
            )
            .with_detail("state_dir", self.root.display().to_string().into())),
            Err(error) if error.kind() == IoErrorKind::NotFound => Ok(false),
            Err(error) => Err(Error::new(
                ErrorKind::UnknownState,
                format!(
                    "the state directory {} could not be read: {error}",
                    self.root.display()
                ),
            )
            .with_detail("state_dir", self.root.display().to_string().into())
            .with_detail("os_error", error.to_string().into())),
        }
    }

    /// Create the state directory and its layout, privately.
    ///
    /// Only a mutating call reaches this: every read-only command reports an
    /// absent directory instead of creating one. An existing directory is left
    /// as it is, apart from being checked for being a real directory. The
    /// account's default root also gets its `barge` parent created when that is
    /// missing; an explicit `BARGE_STATE_DIR` never does.
    pub fn initialize(&self) -> Result<()> {
        if self.source == StateDirSource::Account {
            ensure_account_parent(&self.root)?;
        }
        ensure_directory(&self.root)?;
        for directory in [
            self.areas_dir(),
            self.placements_dir(),
            self.holds_dir(),
            self.activity_dir(),
            self.intents_dir(),
            self.journal_dir(),
            self.locks_dir(),
        ] {
            ensure_directory(&directory)?;
        }
        Ok(())
    }
}

/// Create the `barge` parent of the account's default state root, privately.
///
/// A fresh account has no `barge` folder yet, so the leaf alone cannot be
/// created. Only that one level is made, and only when `Application Support`
/// already exists: nothing above `barge` is ever created. An existing `barge`
/// must be a real directory, so a symbolic link there is refused, not followed.
fn ensure_account_parent(root: &Path) -> Result<()> {
    let (Some(parent), Some(support)) = (root.parent(), root.parent().and_then(Path::parent))
    else {
        return Ok(());
    };
    match std::fs::symlink_metadata(parent) {
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => Err(Error::new(
            ErrorKind::UnknownState,
            format!("{} exists and is not a directory", parent.display()),
        )
        .with_detail("path", parent.display().to_string().into())),
        Err(error) if error.kind() == IoErrorKind::NotFound => {
            if !std::fs::metadata(support).is_ok_and(|metadata| metadata.is_dir()) {
                return Err(Error::new(
                    ErrorKind::Io,
                    format!(
                        "cannot create {}: {} is not an existing directory",
                        parent.display(),
                        support.display()
                    ),
                )
                .with_detail("path", parent.display().to_string().into())
                .with_detail("parent", support.display().to_string().into()));
            }
            ensure_directory(parent)
        }
        Err(error) => Err(Error::new(
            ErrorKind::Io,
            format!("cannot read {}: {error}", parent.display()),
        )
        .with_detail("path", parent.display().to_string().into())),
    }
}

/// Create one private directory, or accept an existing directory unchanged.
fn ensure_directory(path: &Path) -> Result<()> {
    match std::fs::create_dir(path) {
        Ok(()) => set_mode(path, DIRECTORY_MODE),
        Err(error) if error.kind() == IoErrorKind::AlreadyExists => {
            let metadata = std::fs::metadata(path).map_err(|error| {
                Error::new(
                    ErrorKind::Io,
                    format!("cannot read {}: {error}", path.display()),
                )
                .with_detail("path", path.display().to_string().into())
            })?;
            if metadata.is_dir() {
                Ok(())
            } else {
                Err(Error::new(
                    ErrorKind::UnknownState,
                    format!("{} exists and is not a directory", path.display()),
                )
                .with_detail("path", path.display().to_string().into()))
            }
        }
        Err(error) => Err(Error::new(
            ErrorKind::Io,
            format!("cannot create {}: {error}", path.display()),
        )
        .with_detail("path", path.display().to_string().into())),
    }
}

fn set_mode(path: &Path, mode: u32) -> Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).map_err(|error| {
        Error::new(
            ErrorKind::Io,
            format!("cannot set the mode of {}: {error}", path.display()),
        )
        .with_detail("path", path.display().to_string().into())
    })
}

/// Whether a value is one of Barge's 128-bit identifiers: exactly 32 hex digits.
pub fn is_identifier(value: &str) -> bool {
    value.len() == 32 && value.chars().all(|character| character.is_ascii_hexdigit())
}

/// The lock key a deletion unit whose name is not a Barge root id locks under.
///
/// It is the SHA-256 of the unit's volume, parent and name, so the same unit locks
/// under the same key on every run and two units never collide on one lock file.
pub fn unit_lock_key(volume_uuid: &VolumeUuid, parent: &str, name: &str) -> String {
    crate::digest::sha256_hex(
        format!("barge.lock.unit/1\n{volume_uuid}\n{parent}\n{name}").as_bytes(),
    )
}

/// Validate one identifier used in a file name: the design's 128-bit ids are
/// lowercase hexadecimal, so anything else is refused rather than encoded.
pub fn validate_identifier(field: &str, value: &str) -> Result<()> {
    if is_identifier(value) {
        return Ok(());
    }
    Err(Error::new(
        ErrorKind::UnknownState,
        format!("`{value}` is not a 32-hex-digit {field}"),
    )
    .with_detail(field, value.into()))
}

/// The moment a bounded operation must stop starting new work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Deadline {
    at: Instant,
}

impl Deadline {
    /// A deadline `budget` from now. A zero budget is already expired, which is
    /// how a test proves the refusal without waiting.
    pub fn after(budget: Duration) -> Self {
        Self {
            at: Instant::now() + budget,
        }
    }

    /// Whether the deadline has passed.
    pub fn expired(self) -> bool {
        Instant::now() >= self.at
    }

    /// The time left, saturating at zero. It is reported so a refused command
    /// can say what it ran out of.
    pub fn remaining(self) -> Duration {
        self.at.saturating_duration_since(Instant::now())
    }
}

/// One bounded read's limits: how many bytes, how many entries, how much time.
#[derive(Debug, Clone, Copy)]
pub struct Bounds {
    /// The largest file Barge will read for this purpose.
    pub max_bytes: usize,
    /// The most entries a document this read produces may hold. The reader
    /// applies it to the rows it parsed, so a small file cannot describe an
    /// unbounded list.
    pub max_entries: usize,
    /// When the read must have finished.
    pub deadline: Deadline,
}

impl Bounds {
    /// A bound with the design's default read deadline.
    pub fn new(max_bytes: usize, max_entries: usize, deadline: Deadline) -> Self {
        Self {
            max_bytes,
            max_entries,
            deadline,
        }
    }
}

/// What a bounded read found.
#[derive(Debug)]
pub enum ReadOutcome {
    /// The file is not there. Absence is a fact, not an error.
    Absent,
    /// The bytes that were read, at most the bound's size cap.
    Present(Vec<u8>),
    /// The file could not be read within its bounds. The error names the file,
    /// the bound that was hit and why; it is never a default.
    Refused(Error),
}

impl ReadOutcome {
    /// Fold the outcome into `Result<Option<Vec<u8>>>`: a refusal becomes the
    /// error, and absence stays distinguishable from empty bytes.
    pub fn bytes(self) -> Result<Option<Vec<u8>>> {
        match self {
            Self::Absent => Ok(None),
            Self::Present(bytes) => Ok(Some(bytes)),
            Self::Refused(error) => Err(error),
        }
    }
}

/// Read one file strictly, within its size cap and deadline.
///
/// A symbolic link at the path is refused rather than followed, a non-regular
/// file is refused, and a file over the cap is refused before its body is read.
/// An expired deadline refuses before the file is opened.
pub fn read_bounded(path: &Path, bounds: &Bounds) -> ReadOutcome {
    match read_bounded_inner(path, bounds) {
        Ok(outcome) => outcome,
        Err(error) => ReadOutcome::Refused(error),
    }
}

fn read_bounded_inner(path: &Path, bounds: &Bounds) -> Result<ReadOutcome> {
    if bounds.deadline.expired() {
        #[cfg(test)]
        crate::tape::read_refused("state::read_bounded_inner", path, "deadline_expired");
        return Err(bound_error(
            path,
            "the read deadline had already expired before the file was opened".to_owned(),
        ));
    }
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        #[cfg(not(test))]
        Err(error) if error.kind() == IoErrorKind::NotFound => return Ok(ReadOutcome::Absent),
        #[cfg(test)]
        Err(error) if error.kind() == IoErrorKind::NotFound => {
            crate::tape::stat("state::read_bounded_inner::precheck", path, None);
            crate::tape::read_absent("state::read_bounded_inner", path);
            return Ok(ReadOutcome::Absent);
        }
        Err(error) => {
            #[cfg(test)]
            crate::tape::stat_refused(
                "state::read_bounded_inner::precheck",
                path,
                &crate::tape::errno_class(&error),
            );
            return Err(Error::new(
                ErrorKind::Io,
                format!("cannot read {}: {error}", path.display()),
            )
            .with_detail("path", path.display().to_string().into())
            .with_detail("os_error", error.to_string().into()));
        }
    };
    #[cfg(test)]
    crate::tape::stat(
        "state::read_bounded_inner::precheck",
        path,
        Some((
            metadata.dev(),
            metadata.ino(),
            if metadata.file_type().is_symlink() {
                "symlink"
            } else if metadata.is_file() {
                "file"
            } else if metadata.is_dir() {
                "directory"
            } else {
                "other"
            },
        )),
    );
    if metadata.file_type().is_symlink() {
        #[cfg(test)]
        crate::tape::read_refused("state::read_bounded_inner", path, "symlink");
        return Err(Error::new(
            ErrorKind::UnknownState,
            format!(
                "{} is a symbolic link; Barge reads the file it was given, never through a link",
                path.display()
            ),
        )
        .with_detail("path", path.display().to_string().into()));
    }
    if !metadata.is_file() {
        #[cfg(test)]
        crate::tape::read_refused("state::read_bounded_inner", path, "not_regular_file");
        return Err(Error::new(
            ErrorKind::UnknownState,
            format!("{} is not a regular file", path.display()),
        )
        .with_detail("path", path.display().to_string().into()));
    }
    if metadata.len() > bounds.max_bytes as u64 {
        #[cfg(test)]
        crate::tape::read_refused("state::read_bounded_inner", path, "over_size_cap");
        return Err(bound_error(
            path,
            format!(
                "the file is {} bytes, over the {}-byte cap",
                metadata.len(),
                bounds.max_bytes
            ),
        )
        .with_detail("bytes", metadata.len().into())
        .with_detail("cap", bounds.max_bytes.into()));
    }

    let mut file = File::open(path).map_err(|error| {
        #[cfg(test)]
        crate::tape::read_refused(
            "state::read_bounded_inner",
            path,
            &crate::tape::errno_class(&error),
        );
        Error::new(
            ErrorKind::Io,
            format!("cannot open {}: {error}", path.display()),
        )
        .with_detail("path", path.display().to_string().into())
        .with_detail("os_error", error.to_string().into())
    })?;
    // The opened file is what is read, so the size checked above and the bytes
    // read below are the same file even if the path was replaced in between.
    let opened = file.metadata().map_err(|error| {
        #[cfg(test)]
        crate::tape::stat_refused(
            "state::read_bounded_inner::opened",
            path,
            &crate::tape::errno_class(&error),
        );
        Error::new(
            ErrorKind::Io,
            format!("cannot read the size of {}: {error}", path.display()),
        )
        .with_detail("path", path.display().to_string().into())
    })?;
    if !opened.is_file() {
        #[cfg(test)]
        crate::tape::read_refused("state::read_bounded_inner", path, "changed_non_file");
        return Err(Error::new(
            ErrorKind::UnknownState,
            format!(
                "{} changed into a non-file while it was opened",
                path.display()
            ),
        )
        .with_detail("path", path.display().to_string().into()));
    }
    #[cfg(test)]
    crate::tape::stat(
        "state::read_bounded_inner",
        path,
        Some((opened.dev(), opened.ino(), "file")),
    );
    if opened.len() > bounds.max_bytes as u64 {
        #[cfg(test)]
        crate::tape::read_refused("state::read_bounded_inner", path, "over_size_cap");
        return Err(bound_error(
            path,
            format!(
                "the file is {} bytes, over the {}-byte cap",
                opened.len(),
                bounds.max_bytes
            ),
        )
        .with_detail("bytes", opened.len().into())
        .with_detail("cap", bounds.max_bytes.into()));
    }

    let mut buffer = Vec::new();
    let mut chunk = [0u8; READ_CHUNK_BYTES];
    loop {
        if bounds.deadline.expired() {
            #[cfg(test)]
            crate::tape::read_refused("state::read_bounded_inner", path, "deadline_expired");
            return Err(bound_error(
                path,
                "the read deadline expired before the file was read".to_owned(),
            ));
        }
        let read = file.read(&mut chunk).map_err(|error| {
            #[cfg(test)]
            crate::tape::read_refused(
                "state::read_bounded_inner",
                path,
                &crate::tape::errno_class(&error),
            );
            Error::new(
                ErrorKind::Io,
                format!("cannot read {}: {error}", path.display()),
            )
            .with_detail("path", path.display().to_string().into())
            .with_detail("os_error", error.to_string().into())
        })?;
        if read == 0 {
            break;
        }
        if buffer.len() + read > bounds.max_bytes {
            #[cfg(test)]
            crate::tape::read_refused("state::read_bounded_inner", path, "grew_past_cap");
            return Err(bound_error(
                path,
                format!(
                    "the file grew past its {}-byte cap while it was read",
                    bounds.max_bytes
                ),
            )
            .with_detail("cap", bounds.max_bytes.into()));
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    #[cfg(test)]
    crate::tape::read_present(
        "state::read_bounded_inner",
        path,
        Some((opened.dev(), opened.ino())),
        &buffer,
        Some(bounds.max_bytes as u64),
    );
    Ok(ReadOutcome::Present(buffer))
}

fn bound_error(path: &Path, message: String) -> Error {
    Error::new(ErrorKind::ResourceBound, message)
        .with_detail("path", path.display().to_string().into())
}

/// Publish `bytes` at `target` atomically.
///
/// The temporary file is `create_new` in the target's own directory — so the
/// rename is same-directory and atomic — private, synced before the rename and
/// the directory synced after it. A failure before the rename leaves the target
/// exactly as it was and cleans the temporary file up.
pub fn publish_atomic(target: &Path, bytes: &[u8]) -> Result<()> {
    publish_atomic_with(target, bytes, |_| Ok(()))
}

/// The body of [`publish_atomic`] with a seam before the rename, so a test can
/// prove an interrupted publish leaves the previous bytes in place.
pub(crate) fn publish_atomic_with<F>(target: &Path, bytes: &[u8], before_rename: F) -> Result<()>
where
    F: FnOnce(&Path) -> Result<()>,
{
    let parent = target.parent().ok_or_else(|| {
        Error::new(
            ErrorKind::Usage,
            format!("{} has no parent directory", target.display()),
        )
        .with_detail("path", target.display().to_string().into())
    })?;
    let name = target
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            Error::new(
                ErrorKind::Usage,
                format!("{} has no usable file name", target.display()),
            )
            .with_detail("path", target.display().to_string().into())
        })?;
    if !parent.is_dir() {
        return Err(Error::new(
            ErrorKind::Io,
            format!(
                "{} is not a directory, so nothing can be published in it",
                parent.display()
            ),
        )
        .with_detail("path", parent.display().to_string().into()));
    }

    let mut before_rename = Some(before_rename);
    for _ in 0..TEMP_ATTEMPTS {
        let temporary = parent.join(temporary_name(name));
        #[cfg(not(test))]
        let mut file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(FILE_MODE)
            .open(&temporary)
        {
            Ok(file) => file,
            Err(error) if error.kind() == IoErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(Error::new(
                    ErrorKind::Io,
                    format!(
                        "cannot create a temporary file in {}: {error}",
                        parent.display()
                    ),
                )
                .with_detail("path", parent.display().to_string().into())
                .with_detail("os_error", error.to_string().into()));
            }
        };
        #[cfg(test)]
        let mut file = match crate::tape::io_effect("create", &temporary, || {
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(FILE_MODE)
                .open(&temporary)
        }) {
            Ok(file) => file,
            Err(error) if error.kind() == IoErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(Error::new(
                    ErrorKind::Io,
                    format!(
                        "cannot create a temporary file in {}: {error}",
                        parent.display()
                    ),
                )
                .with_detail("path", parent.display().to_string().into())
                .with_detail("os_error", error.to_string().into()));
            }
        };

        let written = write_and_publish(&mut file, &temporary, target, bytes, &mut before_rename);
        if written.is_err() {
            // Best effort: a leftover temporary file is never read as the target,
            // and reporting the original failure matters more than this cleanup.
            #[cfg(not(test))]
            let _ = std::fs::remove_file(&temporary);
            #[cfg(test)]
            let _ =
                crate::tape::io_effect("unlink", &temporary, || std::fs::remove_file(&temporary));
        }
        written?;
        return sync_directory(parent);
    }

    Err(Error::new(
        ErrorKind::Io,
        format!(
            "could not find a fresh temporary name in {} after {TEMP_ATTEMPTS} attempts",
            parent.display()
        ),
    )
    .with_detail("path", parent.display().to_string().into()))
}

fn write_and_publish<F>(
    file: &mut File,
    temporary: &Path,
    target: &Path,
    bytes: &[u8],
    before_rename: &mut Option<F>,
) -> Result<()>
where
    F: FnOnce(&Path) -> Result<()>,
{
    #[cfg(not(test))]
    file.write_all(bytes).map_err(|error| {
        Error::new(
            ErrorKind::Io,
            format!("cannot write {}: {error}", temporary.display()),
        )
        .with_detail("path", temporary.display().to_string().into())
        .with_detail("os_error", error.to_string().into())
    })?;
    #[cfg(test)]
    crate::tape::io_effect("save", temporary, || file.write_all(bytes)).map_err(|error| {
        Error::new(
            ErrorKind::Io,
            format!("cannot write {}: {error}", temporary.display()),
        )
        .with_detail("path", temporary.display().to_string().into())
        .with_detail("os_error", error.to_string().into())
    })?;
    #[cfg(not(test))]
    file.sync_all().map_err(|error| {
        Error::new(
            ErrorKind::Io,
            format!("cannot sync {}: {error}", temporary.display()),
        )
        .with_detail("path", temporary.display().to_string().into())
    })?;
    #[cfg(test)]
    {
        let result = file.sync_all();
        crate::tape::sync_path("state::write_and_publish", temporary, result.as_ref().err());
        result.map_err(|error| {
            Error::new(
                ErrorKind::Io,
                format!("cannot sync {}: {error}", temporary.display()),
            )
            .with_detail("path", temporary.display().to_string().into())
        })?;
    }
    if let Some(seam) = before_rename.take() {
        seam(temporary)?;
    }
    #[cfg(test)]
    {
        crate::tape::io_rename(temporary, target, || std::fs::rename(temporary, target)).map_err(
            |error| {
                Error::new(
                    ErrorKind::Io,
                    format!(
                        "cannot publish {} over {}: {error}",
                        temporary.display(),
                        target.display()
                    ),
                )
                .with_detail("path", target.display().to_string().into())
                .with_detail("os_error", error.to_string().into())
            },
        )
    }
    #[cfg(not(test))]
    std::fs::rename(temporary, target).map_err(|error| {
        Error::new(
            ErrorKind::Io,
            format!(
                "cannot publish {} over {}: {error}",
                temporary.display(),
                target.display()
            ),
        )
        .with_detail("path", target.display().to_string().into())
        .with_detail("os_error", error.to_string().into())
    })
}

fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(test)]
    {
        let result = File::open(path).and_then(|directory| directory.sync_all());
        crate::tape::sync_path("state::sync_directory", path, result.as_ref().err());
        result.map_err(|error| {
            Error::new(
                ErrorKind::Io,
                format!("cannot sync {}: {error}", path.display()),
            )
            .with_detail("path", path.display().to_string().into())
        })
    }
    #[cfg(not(test))]
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| {
            Error::new(
                ErrorKind::Io,
                format!("cannot sync {}: {error}", path.display()),
            )
            .with_detail("path", path.display().to_string().into())
        })
}

fn temporary_name(name: &str) -> String {
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.subsec_nanos());
    format!(".{name}.tmp.{}.{sequence}.{nanos}", std::process::id())
}

/// Milliseconds since the Unix epoch, as every record's timestamp uses.
#[cfg_attr(test, track_caller)]
pub fn now_ms() -> u64 {
    #[cfg(test)]
    {
        let value = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| {
                u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
            });
        crate::tape::now_ms_at(value, std::panic::Location::caller());
        value
    }
    #[cfg(not(test))]
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

/// The one order file locks may be taken in (design, "Account-wide state
/// directory"). Two processes that follow it cannot deadlock, so it is enforced
/// rather than documented and hoped for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LockRank {
    /// The registry: first, and never held across a deletion or a volume walk.
    Registry,
    /// One volume.
    Volume,
    /// One owned root.
    Root,
    /// Writing an intent: last, and it takes no file lock of its own.
    IntentWrite,
}

impl LockRank {
    /// The rank's name, as a refusal reports it.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Registry => "registry",
            Self::Volume => "volume",
            Self::Root => "root",
            Self::IntentWrite => "intent write",
        }
    }
}

/// Whether a lock is shared (observers) or exclusive (one writer).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockMode {
    /// Several readers may hold it at once.
    Shared,
    /// One writer, and no reader.
    Exclusive,
}

/// A held lock file. Dropping it releases the lock and closes the descriptor.
#[derive(Debug)]
pub struct StateLock {
    path: PathBuf,
    rank: LockRank,
    mode: LockMode,
    file: File,
}

impl StateLock {
    /// Take an exclusive lock, creating the lock file if it is not there yet.
    /// Creation needs its directory, which only [`StatePaths::initialize`]
    /// makes; a missing directory is reported rather than created here.
    pub fn exclusive(path: &Path, rank: LockRank, deadline: Deadline) -> Result<Self> {
        #[cfg(not(test))]
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(FILE_MODE)
            .open(path)
            .map_err(|error| lock_open_error(path, &error))?;
        #[cfg(test)]
        let file = crate::tape::io_effect("lock-create", path, || {
            OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(FILE_MODE)
                .open(path)
        })
        .map_err(|error| lock_open_error(path, &error))?;
        #[cfg(not(test))]
        wait_for_lock(&file, path, LockMode::Exclusive, deadline)?;
        #[cfg(test)]
        wait_for_lock(&file, path, LockMode::Exclusive, deadline).inspect_err(|error| {
            crate::tape::lock(
                "state::StateLock::exclusive",
                path,
                crate::tape::LockOutcome::Refused(error.kind().as_str().to_owned()),
            );
        })?;
        #[cfg(test)]
        crate::tape::lock(
            "state::StateLock::exclusive",
            path,
            crate::tape::LockOutcome::Held(
                lock_observed_identity(&file, path),
                std::os::fd::AsRawFd::as_raw_fd(&file),
            ),
        );
        Ok(Self {
            path: path.to_path_buf(),
            rank,
            mode: LockMode::Exclusive,
            file,
        })
    }

    /// Take a shared lock, but only on a lock file that already exists.
    ///
    /// A read-only command must create nothing, so an absent lock file means
    /// "nothing has ever written here" and yields `None` instead of a new file.
    pub fn shared_if_present(
        path: &Path,
        rank: LockRank,
        deadline: Deadline,
    ) -> Result<Option<Self>> {
        let file = match OpenOptions::new().read(true).open(path) {
            Ok(file) => file,
            #[cfg(not(test))]
            Err(error) if error.kind() == IoErrorKind::NotFound => return Ok(None),
            #[cfg(test)]
            Err(error) if error.kind() == IoErrorKind::NotFound => {
                #[cfg(test)]
                crate::tape::lock(
                    "state::StateLock::shared_if_present",
                    path,
                    crate::tape::LockOutcome::Absent,
                );
                return Ok(None);
            }
            #[cfg(not(test))]
            Err(error) => return Err(lock_open_error(path, &error)),
            #[cfg(test)]
            Err(error) => {
                #[cfg(test)]
                crate::tape::lock(
                    "state::StateLock::shared_if_present",
                    path,
                    crate::tape::LockOutcome::Refused(crate::tape::errno_class(&error)),
                );
                return Err(lock_open_error(path, &error));
            }
        };
        #[cfg(not(test))]
        wait_for_lock(&file, path, LockMode::Shared, deadline)?;
        #[cfg(test)]
        wait_for_lock(&file, path, LockMode::Shared, deadline).inspect_err(|error| {
            crate::tape::lock(
                "state::StateLock::shared_if_present",
                path,
                crate::tape::LockOutcome::Refused(error.kind().as_str().to_owned()),
            );
        })?;
        #[cfg(test)]
        crate::tape::lock(
            "state::StateLock::shared_if_present",
            path,
            crate::tape::LockOutcome::Held(
                lock_observed_identity(&file, path),
                std::os::fd::AsRawFd::as_raw_fd(&file),
            ),
        );
        Ok(Some(Self {
            path: path.to_path_buf(),
            rank,
            mode: LockMode::Shared,
            file,
        }))
    }

    /// The lock file this lock is held on.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The rank this lock was taken at.
    pub fn rank(&self) -> LockRank {
        self.rank
    }

    /// Whether the lock is shared or exclusive.
    pub fn mode(&self) -> LockMode {
        self.mode
    }
}

/// L1: observe the already held lock descriptor without changing the lock result.
#[cfg(test)]
fn lock_observed_identity(file: &File, path: &Path) -> Option<(u64, u64)> {
    let observed = LOCK_METADATA_FAIL.with(|fail| {
        if fail.replace(false) {
            Err(std::io::Error::from_raw_os_error(libc::EIO))
        } else {
            file.metadata()
        }
    });
    match observed {
        Ok(metadata) => {
            let facts = (metadata.dev(), metadata.ino());
            crate::tape::stat(
                "state::StateLock::metadata",
                path,
                Some((facts.0, facts.1, "file")),
            );
            Some(facts)
        }
        Err(error) => {
            crate::tape::stat_refused(
                "state::StateLock::metadata",
                path,
                &crate::tape::errno_class(&error),
            );
            None
        }
    }
}

#[cfg(test)]
thread_local! {
    static LOCK_METADATA_FAIL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Inject one L1 error for the recorder's failure control.
#[cfg(test)]
pub(crate) fn fail_next_lock_metadata() {
    LOCK_METADATA_FAIL.with(|fail| fail.set(true));
}

impl Drop for StateLock {
    fn drop(&mut self) {
        // Closing releases the lock anyway; unlocking first keeps the release
        // explicit and independent of descriptor lifetime.
        let _ = self.file.unlock();
    }
}

fn wait_for_lock(file: &File, path: &Path, mode: LockMode, deadline: Deadline) -> Result<()> {
    loop {
        let attempt = match mode {
            LockMode::Shared => file.try_lock_shared(),
            LockMode::Exclusive => file.try_lock(),
        };
        match attempt {
            Ok(()) => return Ok(()),
            Err(std::fs::TryLockError::WouldBlock) => {
                if deadline.expired() {
                    #[cfg(test)]
                    crate::tape::lock(
                        "state::wait_for_lock",
                        path,
                        crate::tape::LockOutcome::Refused("locked".to_owned()),
                    );
                    return Err(Error::new(
                        ErrorKind::Locked,
                        format!(
                            "{} is locked by another live owner; it was not taken over",
                            path.display()
                        ),
                    )
                    .with_detail("path", path.display().to_string().into())
                    .with_detail("mode", lock_mode_name(mode).into()));
                }
                std::thread::sleep(LOCK_POLL);
            }
            Err(std::fs::TryLockError::Error(error)) => {
                #[cfg(test)]
                crate::tape::lock(
                    "state::wait_for_lock",
                    path,
                    crate::tape::LockOutcome::Refused(crate::tape::errno_class(&error)),
                );
                return Err(Error::new(
                    ErrorKind::Io,
                    format!("cannot lock {}: {error}", path.display()),
                )
                .with_detail("path", path.display().to_string().into())
                .with_detail("os_error", error.to_string().into()));
            }
        }
    }
}

const fn lock_mode_name(mode: LockMode) -> &'static str {
    match mode {
        LockMode::Shared => "shared",
        LockMode::Exclusive => "exclusive",
    }
}

fn lock_open_error(path: &Path, error: &std::io::Error) -> Error {
    Error::new(
        ErrorKind::Io,
        format!("cannot open the lock file {}: {error}", path.display()),
    )
    .with_detail("path", path.display().to_string().into())
    .with_detail("os_error", error.to_string().into())
}

/// The token a mutating command holds while it writes an intent.
///
/// It takes no file lock: the intent write is last in the fixed order, and
/// holding this token is the type-level proof that it was ordered last.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IntentWrite {
    rank: LockRank,
}

impl IntentWrite {
    /// The rank of this step.
    pub fn rank(&self) -> LockRank {
        self.rank
    }
}

/// A command's set of locks over one state directory.
///
/// The set remembers the highest rank it has acquired, so a lock taken out of
/// order is refused in every build rather than only under a debug assertion.
#[derive(Debug)]
pub struct LockSet {
    state: StatePaths,
    highest: Option<LockRank>,
    deadline: Deadline,
}

impl LockSet {
    /// A lock set over `state`, waiting [`LOCK_WAIT`] for a live owner.
    pub fn new(state: StatePaths) -> Self {
        Self::with_deadline(state, Deadline::after(LOCK_WAIT))
    }

    /// A lock set with an explicit wait bound, which a test shortens.
    pub fn with_deadline(state: StatePaths, deadline: Deadline) -> Self {
        Self {
            state,
            highest: None,
            deadline,
        }
    }

    /// The state directory this set locks.
    pub fn state(&self) -> &StatePaths {
        &self.state
    }

    /// The highest rank acquired so far, in order.
    pub fn highest(&self) -> Option<LockRank> {
        self.highest
    }

    /// Take the registry lock: the first rank, exclusive.
    pub fn lock_registry(&mut self) -> Result<StateLock> {
        let path = self.state.registry_lock_path();
        self.acquire_exclusive(LockRank::Registry, &path)
    }

    /// Take one volume's lock: the second rank, exclusive.
    pub fn lock_volume(&mut self, volume_uuid: &VolumeUuid) -> Result<StateLock> {
        let path = self.state.volume_lock_path(volume_uuid);
        self.acquire_exclusive(LockRank::Volume, &path)
    }

    /// Take one root's lock: the third rank, exclusive.
    pub fn lock_root(&mut self, root_id: &str) -> Result<StateLock> {
        let path = self.state.root_lock_path(root_id)?;
        self.acquire_exclusive(LockRank::Root, &path)
    }

    /// Take one deletion unit's lock by its [`unit_lock_key`]: the third rank,
    /// exclusive, so two deletions or replays of one unit never interleave.
    pub fn lock_unit(&mut self, key: &str) -> Result<StateLock> {
        let path = self.state.unit_lock_path(key)?;
        self.acquire_exclusive(LockRank::Root, &path)
    }

    /// Observe the registry: a shared lock, and only when the lock file exists.
    /// A read-only command creates no lock, no file and no directory.
    pub fn observe_registry(&mut self) -> Result<Option<StateLock>> {
        let path = self.state.registry_lock_path();
        self.acquire_shared(LockRank::Registry, &path)
    }

    /// Observe one volume: a shared lock, and only when the lock file exists.
    ///
    /// A preview takes it after the registry's observation lock and holds it
    /// while it walks, so no real prune of that volume can act under it; the
    /// wait is this set's own deadline, and a lock a live owner holds past it is
    /// refused rather than taken over. A lock file that does not exist is
    /// nothing to wait for: the read-only command creates no file.
    pub fn observe_volume(&mut self, volume_uuid: &VolumeUuid) -> Result<Option<StateLock>> {
        let path = self.state.volume_lock_path(volume_uuid);
        self.acquire_shared(LockRank::Volume, &path)
    }

    /// Observe one deletion unit by its [`unit_lock_key`]: a shared lock, and
    /// only when the lock file exists.
    ///
    /// It is the third rank, like the root lock, and it is the lock a delete or a
    /// hold of a unit whose name is not a Barge root id takes. A preview of such a
    /// unit holds it shared so it cannot read one mid-delete.
    pub fn observe_unit(&mut self, key: &str) -> Result<Option<StateLock>> {
        let path = self.state.unit_lock_path(key)?;
        self.acquire_shared(LockRank::Root, &path)
    }

    /// Observe one root: a shared lock, and only when the lock file exists.
    ///
    /// It is the third rank, so a caller may take one root at a time under the
    /// volume's observation lock and must release it before the next: a consumer's
    /// hold and a delete both take the root lock exclusively, and neither may
    /// interleave with a plan that is reading that root.
    pub fn observe_root(&mut self, root_id: &str) -> Result<Option<StateLock>> {
        let path = self.state.root_lock_path(root_id)?;
        self.acquire_shared(LockRank::Root, &path)
    }

    /// Enter the intent-write step: the last rank, ordered after the root lock.
    pub fn begin_intent_write(&mut self) -> Result<IntentWrite> {
        self.check(LockRank::IntentWrite)?;
        self.highest = Some(LockRank::IntentWrite);
        Ok(IntentWrite {
            rank: LockRank::IntentWrite,
        })
    }

    fn acquire_exclusive(&mut self, rank: LockRank, path: &Path) -> Result<StateLock> {
        self.check(rank)?;
        let lock = StateLock::exclusive(path, rank, self.deadline)?;
        debug_assert_eq!(lock.rank(), rank, "a lock reports the rank it was taken at");
        self.highest = Some(rank);
        Ok(lock)
    }

    fn acquire_shared(&mut self, rank: LockRank, path: &Path) -> Result<Option<StateLock>> {
        self.check(rank)?;
        let lock = StateLock::shared_if_present(path, rank, self.deadline)?;
        if lock.is_some() {
            self.highest = Some(rank);
        }
        Ok(lock)
    }

    fn check(&self, rank: LockRank) -> Result<()> {
        let Some(held) = self.highest else {
            return Ok(());
        };
        if held < rank {
            return Ok(());
        }
        Err(Error::new(
            ErrorKind::Usage,
            format!(
                "the lock order is registry, volume, root, intent write; {} was requested after {}",
                rank.as_str(),
                held.as_str()
            ),
        )
        .with_detail("held", held.as_str().into())
        .with_detail("requested", rank.as_str().into()))
    }
}

/// One whole-root deletion's phase, in the order the phases happen.
///
/// Every phase is durable before the effect it authorises: `Intent` before the
/// quarantine rename, `Renamed` before the first unlink, `MarkerLast` before the
/// quarantine leaf is removed, and `Deleted` once the leaf is gone. Only
/// `Deleted` is settled; every other phase is a pending intent and a replay
/// question.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntentPhase {
    /// The intent is written; the root is still in place.
    Intent,
    /// The root was renamed to its quarantine name; its contents are being
    /// deleted.
    Renamed,
    /// The marker was unlinked last and the leaf may be removed.
    MarkerLast,
    /// Barge's own final `unlinkat` of a verified captured private name
    /// succeeded. It is the durable completion evidence: a replay may settle the
    /// intent only after it, and an absence without it is unknown, never a
    /// completion.
    Removed,
    /// The leaf is gone and the intent is settled.
    Deleted,
}

impl IntentPhase {
    /// The phase's name, as a document reports it.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Intent => "intent",
            Self::Renamed => "renamed",
            Self::MarkerLast => "marker_last",
            Self::Removed => "removed",
            Self::Deleted => "deleted",
        }
    }

    /// Whether the intent still needs a replay decision.
    pub const fn is_pending(self) -> bool {
        !matches!(self, Self::Deleted)
    }
}

/// The unit one `barge.intent/2` record acts on: the plan's expectation, exactly
/// as the deletion verified it, plus the private name the unit is captured under.
///
/// It is the durable half of [`crate::unit::DeletionUnit`]: the unit's name and
/// kind, the optional marker with its filename and family, the expected device
/// and inode, the grant the deletion rests on, and the planned private capture
/// name - recorded **before** the capture, so the private name a replay may later
/// unlink is never one the journal does not name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntentUnit {
    /// The parent directory relative to the area folder; `""` is the folder
    /// itself.
    pub parent: String,
    /// The unit's name inside that parent.
    pub name: String,
    /// Whether the unit is a directory or one file.
    pub kind: crate::unit::UnitKind,
    /// The device the plan recorded.
    pub device: u64,
    /// The inode the plan recorded.
    pub inode: u64,
    /// The marker the plan read, when the unit carries one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marker: Option<crate::unit::UnitMarker>,
    /// The marker's root id, when the unit's directory name is not it.
    ///
    /// An optional addition inside this major: a `hold` takes the root-id lock,
    /// while a unit whose name is not a Barge root id takes its own key, so a
    /// replay of an **adopted** root's intent must take the root-id lock as well or
    /// it could act on a root its holder believes it holds. A record written before
    /// this field existed carries none, and its name is its root id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_id: Option<String>,
    /// The private `.barge-prune-<intent-id>` name the unit was captured to
    /// inside its parent, recorded before the capture.
    pub capture_name: String,
    /// The recorded grant the deletion rests on.
    pub grant: crate::unit::UnitGrant,
}

impl IntentUnit {
    /// Validate every recorded fact strictly.
    ///
    /// The record's field set is the v2 major's, so a reader refuses a v1 record
    /// read as v2 and the reverse. `capture_name` must be exactly this intent's
    /// quarantine name, so a record can never name another operation's private
    /// name.
    pub fn validate(&self, intent_id: &str) -> Result<()> {
        crate::unit::validate_parent(&self.parent)?;
        validate_entry_name(&self.name)?;
        if self.name == "." || self.name == ".." {
            return Err(Error::new(
                ErrorKind::UnknownState,
                format!("`{}` is not a unit name", self.name),
            )
            .with_detail("intent_id", intent_id.to_owned().into()));
        }
        let expected = format!("{QUARANTINE_PREFIX}{intent_id}");
        if self.capture_name != expected {
            return Err(Error::new(
                ErrorKind::UnknownState,
                format!(
                    "the intent's private capture name is `{}`, not `{expected}`",
                    self.capture_name
                ),
            )
            .with_detail("intent_id", intent_id.to_owned().into()));
        }
        self.grant.validate()?;
        if let Some(root_id) = &self.root_id {
            if !is_identifier(root_id) {
                return Err(Error::new(
                    ErrorKind::UnknownState,
                    "the intent's marker root id is not 32 lowercase hex digits".to_owned(),
                )
                .with_detail("intent_id", intent_id.to_owned().into()));
            }
            if *root_id == self.name {
                return Err(Error::new(
                    ErrorKind::UnknownState,
                    "the intent records a marker root id equal to its unit name, which is one lock, not two"
                        .to_owned(),
                )
                .with_detail("intent_id", intent_id.to_owned().into()));
            }
            if self.kind != crate::unit::UnitKind::Directory
                || !matches!(
                    self.marker.as_ref().map(|marker| marker.family),
                    Some(crate::unit::MarkerFamily::Barge)
                )
            {
                return Err(Error::new(
                    ErrorKind::UnknownState,
                    "only a Barge-marked directory root can name a marker root id".to_owned(),
                )
                .with_detail("intent_id", intent_id.to_owned().into()));
            }
        }
        match (&self.marker, self.kind) {
            (Some(marker), crate::unit::UnitKind::Directory) => marker.validate()?,
            (Some(_), crate::unit::UnitKind::File) => {
                return Err(Error::new(
                    ErrorKind::UnknownState,
                    "a file unit never carries a marker, which is a file inside a directory"
                        .to_owned(),
                )
                .with_detail("intent_id", intent_id.to_owned().into()))
            }
            (None, _) => {}
        }
        crate::unit::validate_grant_shape(self.marker.as_ref(), self.kind, &self.grant, &self.name)
    }
}

/// One durable `barge.intent/2` record: the unit a deletion acts on, and the
/// phase it has reached.
///
/// The record never states a conclusion about data it has not observed. A replay
/// resolves it only from the recorded unit - its kind, name, parent, device,
/// inode, marker bytes and grant - which is why they are recorded before the
/// first effect. It also carries the captured objects the deletion could not put
/// back, so a replay never mistakes one of them for an ordinary entry it may
/// delete.
///
/// `barge.intent/1` stays readable: [`read_intent`] maps a v1 record to a
/// Barge-marked directory unit, which is exactly the shape v1 could describe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntentRecord {
    /// Always `barge.intent/2`.
    pub schema: String,
    /// The intent's fresh 128-bit id, which also names the quarantine leaf.
    pub intent_id: String,
    /// The verified volume's UUID.
    pub volume_uuid: VolumeUuid,
    /// The mount point observed when the intent was written.
    pub mount_hint: PathBuf,
    /// The volume's `root_dir` the unit was resolved under.
    pub root_dir: String,
    /// The area folder the unit was resolved in.
    pub folder: String,
    /// The unit the deletion acts on.
    pub unit: IntentUnit,
    /// The SHA-256 of the plan that authorised the deletion.
    pub plan_digest: String,
    /// The phase the deletion reached.
    pub phase: IntentPhase,
    /// Increases with every phase published; an intent starts at 1.
    pub generation: u64,
    /// When the intent was first written.
    pub recorded_ms: u64,
    /// When this phase was written.
    pub phase_ms: u64,
    /// The captures this operation recorded and verified, which are the only
    /// private names it may unlink. The list is the in-flight stack, so it is
    /// bounded by the deletion's depth bound.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub captures: Vec<CaptureRecord>,
    /// The captures whose verification failed, which no run and no replay ever
    /// unlinks. It is written before their put-back.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub foreign_captures: Vec<ForeignCapture>,
    /// The plan's condition 2 evidence, when the deletion came through a release
    /// gate that proved a durable identity (stage 4 F4).
    ///
    /// It holds what the plan proved, never a permission: the replay re-establishes
    /// the identity from the durable record and the marker bytes, and a device
    /// number that changed with a remount is tolerated rather than trusted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<crate::evidence::IdentityEvidence>,
    /// The plan's condition 6 evidence: the `barge.absence/1` proof of the consumer
    /// path whose absence the release rested on (stage 4 F4).
    ///
    /// It is the proof as the plan established it, with its own `proved_ms`. A
    /// replay re-establishes it before it lets any further effect happen, because a
    /// path can reappear and a proof is never a cached permission.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub absence: Option<crate::evidence::AbsenceEvidence>,
}

/// The plan's proved evidence a deletion carries into its intent.
///
/// Both halves are optional because a delete path that is not the release gate has
/// neither: a scratch sweep or a space prune deletes a unit the area already
/// declared expendable, and it carries no consumer reference to prove absent. A
/// release run supplies what its row proved, and the record keeps it so a replay
/// after a reboot can say what was proved and re-establish it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IntentEvidence {
    /// Condition 2's evidence, when a release gate proved a durable identity.
    pub identity: Option<crate::evidence::IdentityEvidence>,
    /// Condition 6's evidence, when a reference required the path's absence.
    pub absence: Option<crate::evidence::AbsenceEvidence>,
}

impl IntentEvidence {
    /// No evidence: the form a delete path that is not the release gate carries.
    pub fn none() -> Self {
        Self::default()
    }
}

impl IntentRecord {
    /// Whether the intent is written before its first effect.
    pub const fn is_pending(&self) -> bool {
        self.phase.is_pending()
    }

    /// The unit's name, as a report and a lock name it.
    pub fn unit_name(&self) -> &str {
        &self.unit.name
    }

    /// The planned private capture name the unit is held under inside its parent.
    pub fn capture_name(&self) -> &str {
        &self.unit.capture_name
    }

    /// The deletion unit the record describes, with the plan digest and area
    /// folder it recorded.
    pub fn deletion_unit(&self) -> crate::unit::DeletionUnit {
        crate::unit::DeletionUnit {
            volume_uuid: self.volume_uuid.clone(),
            root_dir: self.root_dir.clone(),
            folder: self.folder.clone(),
            parent: self.unit.parent.clone(),
            name: self.unit.name.clone(),
            kind: self.unit.kind,
            device: crate::os::DeviceId::new(self.unit.device),
            inode: self.unit.inode,
            marker: self.unit.marker.clone(),
            root_id: self.unit.root_id.clone(),
            grant: self.unit.grant.clone(),
            plan_digest: self.plan_digest.clone(),
        }
    }

    /// The deletion unit a replay acts on: the recorded unit, carrying the device
    /// the live volume reports now.
    ///
    /// The recorded device is the plan's within-run observation, so it is never
    /// durable identity and a replay never compares it to a live one. The unit's
    /// identity is its inode and its marker bytes, both of which this keeps
    /// unchanged; only the device - which the walk needs to prove it stays on one
    /// filesystem - is the one the replay verified.
    pub fn replay_unit(&self, device: crate::os::DeviceId) -> crate::unit::DeletionUnit {
        let mut unit = self.deletion_unit();
        unit.device = device;
        unit
    }

    /// Validate every recorded fact strictly. A record that does not validate is
    /// unknown, never an intent to act on.
    pub fn validate(&self) -> Result<()> {
        if self.schema != crate::contract::INTENT_SCHEMA {
            return Err(Error::new(
                ErrorKind::UnknownState,
                format!(
                    "`{}` is not the `{}` major this build reads",
                    self.schema,
                    crate::contract::INTENT_SCHEMA
                ),
            )
            .with_detail("intent_id", self.intent_id.clone().into()));
        }
        validate_identifier("intent_id", &self.intent_id)?;
        self.unit.validate(&self.intent_id)?;
        crate::registry::validate_folder("root_dir", &self.root_dir, true)?;
        crate::registry::validate_folder("folder", &self.folder, false)?;
        if !is_hex(self.plan_digest.as_str(), 64) {
            return Err(Error::new(
                ErrorKind::UnknownState,
                "the intent's plan digest is not a SHA-256 hex string".to_owned(),
            )
            .with_detail("intent_id", self.intent_id.clone().into()));
        }
        if self.volume_uuid.to_string().is_empty() {
            return Err(Error::new(
                ErrorKind::UnknownState,
                "the intent records no volume".to_owned(),
            ));
        }
        if self.generation == 0 {
            return Err(Error::new(
                ErrorKind::UnknownState,
                "an intent's generation starts at 1".to_owned(),
            )
            .with_detail("intent_id", self.intent_id.clone().into()));
        }
        if self.phase_ms < self.recorded_ms {
            return Err(Error::new(
                ErrorKind::UnknownState,
                "the intent's phase is older than the intent".to_owned(),
            )
            .with_detail("intent_id", self.intent_id.clone().into()));
        }
        if self.captures.len() > INTENT_CAPTURES {
            return Err(Error::new(
                ErrorKind::UnknownState,
                format!("the intent records more than {INTENT_CAPTURES} captures"),
            )
            .with_detail("intent_id", self.intent_id.clone().into()));
        }
        for capture in &self.captures {
            capture.validate(self.capture_name())?;
        }
        if self.foreign_captures.len() > INTENT_FOREIGN_CAPTURES {
            return Err(Error::new(
                ErrorKind::UnknownState,
                format!("the intent records more than {INTENT_FOREIGN_CAPTURES} foreign captures"),
            )
            .with_detail("intent_id", self.intent_id.clone().into()));
        }
        for foreign in &self.foreign_captures {
            foreign.validate()?;
        }
        self.validate_evidence()
    }

    /// Validate the two plan-and-intent evidence documents, and that they agree
    /// with the unit they were proved for.
    ///
    /// An intent is written only after every condition of its gate held, so a
    /// recorded identity is `verified` and a recorded absence is `absent`: a
    /// document that says otherwise is not one this reader understands. The
    /// identity evidence must also name this unit — its inode and the exact marker
    /// bytes the record carries — and a unit that carries identity evidence is a
    /// Barge-marked directory, because that is the only shape a durable root
    /// identity describes.
    fn validate_evidence(&self) -> Result<()> {
        let evidence = |message: &str| {
            Error::new(ErrorKind::UnknownState, message.to_owned())
                .with_detail("intent_id", self.intent_id.clone().into())
        };
        if let Some(identity) = &self.identity {
            identity.validate()?;
            if identity.verdict != "verified" {
                return Err(evidence(
                    "an intent records identity evidence only for a verified root",
                ));
            }
            if self.unit.kind != crate::unit::UnitKind::Directory
                || self
                    .unit
                    .marker
                    .as_ref()
                    .is_none_or(|marker| marker.family != crate::unit::MarkerFamily::Barge)
            {
                return Err(evidence(
                    "identity evidence describes a Barge-marked directory unit",
                ));
            }
            if identity.inode != Some(self.unit.inode) {
                return Err(evidence(
                    "the intent's identity evidence names another inode",
                ));
            }
            let marker_digest = self
                .unit
                .marker
                .as_ref()
                .map(|marker| marker.digest.as_str());
            if identity.marker_digest.as_deref() != marker_digest {
                return Err(evidence(
                    "the intent's identity evidence names other marker bytes",
                ));
            }
        }
        if let Some(absence) = &self.absence {
            absence.validate()?;
            if absence.verdict != "absent" {
                return Err(evidence(
                    "an intent records an absence proof only for an absent path",
                ));
            }
        }
        Ok(())
    }
}

/// One `barge.intent/1` record, exactly as S5 first wrote it.
///
/// It is the field set of the previous major, `deny_unknown_fields` included, so
/// a v2 document cannot be read as v1 (it carries `unit` and no `root_id`) and a
/// v1 document cannot be read as v2. [`read_intent`] maps it to the general
/// record, where a v1 root is a Barge-marked directory unit directly inside its
/// area folder - the only shape v1 could describe.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct IntentV1 {
    schema: String,
    intent_id: String,
    root_id: String,
    volume_uuid: VolumeUuid,
    mount_hint: PathBuf,
    root_dir: String,
    folder: String,
    root_name: String,
    quarantine_name: String,
    device: u64,
    inode: u64,
    marker_digest: String,
    plan_digest: String,
    phase: IntentPhase,
    generation: u64,
    recorded_ms: u64,
    phase_ms: u64,
    #[serde(default)]
    captures: Vec<CaptureRecord>,
    #[serde(default)]
    foreign_captures: Vec<ForeignCapture>,
}

impl IntentV1 {
    /// Map a v1 record to the general record, refusing one that does not hold
    /// v1's own invariants.
    fn upgrade(self) -> Result<IntentRecord> {
        if self.schema != crate::contract::INTENT_SCHEMA_V1 {
            return Err(Error::new(
                ErrorKind::UnknownState,
                format!(
                    "`{}` is not the `{}` major",
                    self.schema,
                    crate::contract::INTENT_SCHEMA_V1
                ),
            )
            .with_detail("intent_id", self.intent_id.clone().into()));
        }
        // v1 named a Barge-owned root, and a root id is a Barge identifier. The
        // quarantine leaf and the marker are v1's, unchanged, so the mapping
        // invents nothing the record did not already say.
        validate_identifier("root_id", &self.root_id)?;
        if self.root_name != self.root_id {
            return Err(Error::new(
                ErrorKind::UnknownState,
                "the intent's root name is not the root id".to_owned(),
            )
            .with_detail("intent_id", self.intent_id.clone().into()));
        }
        let unit = IntentUnit {
            parent: String::new(),
            name: self.root_id.clone(),
            kind: crate::unit::UnitKind::Directory,
            device: self.device,
            inode: self.inode,
            marker: Some(crate::unit::UnitMarker::new(
                crate::unit::MarkerFamily::Barge,
                self.marker_digest,
            )),
            root_id: None,
            capture_name: self.quarantine_name,
            grant: crate::unit::UnitGrant::native_root(self.root_id.clone()),
        };
        Ok(IntentRecord {
            schema: crate::contract::INTENT_SCHEMA.to_owned(),
            intent_id: self.intent_id,
            volume_uuid: self.volume_uuid,
            mount_hint: self.mount_hint,
            root_dir: self.root_dir,
            folder: self.folder,
            unit,
            plan_digest: self.plan_digest,
            phase: self.phase,
            generation: self.generation,
            recorded_ms: self.recorded_ms,
            phase_ms: self.phase_ms,
            captures: self.captures,
            foreign_captures: self.foreign_captures,
            // A v1 record was written before either document existed, and it
            // records nothing about identity or absence to map.
            identity: None,
            absence: None,
        })
    }
}

/// The quarantine leaf's name prefix: `.barge-prune-<intent-id>`.
pub const QUARANTINE_PREFIX: &str = ".barge-prune-";

/// The private name one entry is held under between its capture and its unlink:
/// `.barge-del-<128-bit nonce>`.
pub const CAPTURE_PREFIX: &str = ".barge-del-";

/// How many recorded captures one intent keeps.
///
/// It is the in-flight stack of private names - one per depth level, the
/// quarantine leaf included - with headroom, so it never grows with the tree. A
/// snapshot that would not fit the intent's own cap is refused when it is
/// published, and the deletion stops pending rather than unlink something it could
/// not record.
pub const INTENT_CAPTURES: usize = 20;

/// The verdict one capture's post-capture verification reached.
///
/// Only a capture with a verdict is ever unlinked: the object is this operation's
/// own captured object, verified under its private name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureVerdict {
    /// The captured object reported the device, inode and kind the walk observed
    /// at its name.
    Observed,
    /// The captured object is Finder metadata by class: a regular file of this
    /// user on this device, inside the size bound.
    Finder,
}

/// One capture the intent durably records, with its verification.
///
/// A `.barge-del-*` name is deletable only while the intent records it here with a
/// verdict: a name with no record, a record whose identity no longer matches, or a
/// record for another operation's capture is never unlinked and keeps the intent
/// pending.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureRecord {
    /// The private `.barge-del-<nonce>` name the object is held under.
    pub private: String,
    /// The name the object was captured from.
    pub original: String,
    /// The device the captured object reported.
    pub device: u64,
    /// The inode the captured object reported.
    pub inode: u64,
    /// The kind the captured object reported: `file` or `directory`.
    pub kind: String,
    /// The verification verdict the capture reached.
    pub verdict: CaptureVerdict,
    /// The object's original full `st_mode` - the file-type bits included -
    /// recorded before Barge changed it, so an abort and a replay restore exactly
    /// what `stat` reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<u32>,
}

impl CaptureRecord {
    /// Validate every recorded fact strictly.
    ///
    /// `quarantine` is the intent's own leaf name: the leaf is a captured object
    /// too, so its record legitimately names it instead of a `.barge-del-*` name.
    pub fn validate(&self, quarantine: &str) -> Result<()> {
        if !is_capture_name(&self.private) && self.private != quarantine {
            return Err(Error::new(
                ErrorKind::UnknownState,
                format!(
                    "`{}` is not a private `{CAPTURE_PREFIX}<nonce>` name",
                    self.private
                ),
            )
            .with_detail("private", self.private.clone().into()));
        }
        validate_entry_name(&self.original)?;
        if self.kind != "file" && self.kind != "directory" {
            return Err(Error::new(
                ErrorKind::UnknownState,
                format!(
                    "a capture records the kind `{}`, which is never deletable",
                    self.kind
                ),
            )
            .with_detail("private", self.private.clone().into()));
        }
        if let Some(mode) = self.mode {
            // A full `st_mode` is a 16-bit `mode_t`; its file-type bits must agree
            // with the recorded kind, so a record cannot claim a directory mode for
            // a file.
            let type_bits = mode & 0o170000;
            let expected = if self.kind == "directory" {
                0o040000
            } else {
                0o100000
            };
            if mode > u32::from(u16::MAX) || (mode != 0 && type_bits != expected) {
                return Err(Error::new(
                    ErrorKind::UnknownState,
                    "a recorded mode is not the object's full st_mode".to_owned(),
                )
                .with_detail("private", self.private.clone().into()));
            }
        }
        Ok(())
    }
}

/// One entry name as a record keeps it: exactly one component.
///
/// It is the shape a deletion unit's name and any recorded entry name must have:
/// non-empty UTF-8, at most the platform's 255 bytes, and no separator, so a name
/// never escapes the directory a descriptor-relative walk reached.
pub fn validate_entry_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 255 || name == "." || name == ".." || name.contains('/') {
        return Err(Error::new(
            ErrorKind::UnknownState,
            "a record does not name one entry".to_owned(),
        ));
    }
    Ok(())
}

/// One recorded kind. A foreign capture records whatever the walk saw, including
/// a link or another object, because that is what it did not expect.
fn validate_entry_kind(kind: &str, private: &str) -> Result<()> {
    if !matches!(kind, "file" | "directory" | "symlink" | "other") {
        return Err(Error::new(
            ErrorKind::UnknownState,
            format!("a record names the kind `{kind}`"),
        )
        .with_detail("private", private.to_owned().into()));
    }
    Ok(())
}

/// How many unverified captures one intent records.
///
/// A deletion stops at the first one, so a run adds at most one; the bound keeps
/// repeated runs' evidence from growing the record without limit.
pub const INTENT_FOREIGN_CAPTURES: usize = 4;

/// The longest reason one unverified-capture record keeps, in bytes.
pub const FOREIGN_REASON_BYTES: usize = 160;

/// Whether a name is one of Barge's private capture names.
///
/// Only the form is recognised: the object under it is still verified before
/// anything unlinks it, and a private name a replay did not create itself is
/// never unlinked at all.
pub fn is_capture_name(name: &str) -> bool {
    let Some(nonce) = name.strip_prefix(CAPTURE_PREFIX) else {
        return false;
    };
    nonce.len() == 32 && nonce.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// What a deletion's put-back of one unverified capture answered.
///
/// [`Pending`](Self::Pending) is the answer a record carries when it was written
/// before the put-back was attempted and the process stopped before recording the
/// outcome. It is never read as success: the record blocks a replay either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PutBackOutcome {
    /// The record was written before the put-back, and no later answer was
    /// recorded.
    Pending,
    /// The object is back under its original name, and it is still not the
    /// observed one.
    Renamed,
    /// The original name was taken, so the object stays under its private name.
    Taken,
    /// The put-back was not attempted or failed, so the object stays too.
    Failed,
}

/// The identity the captured object actually had, when it could be read.
///
/// It is absent when the object could not be read under its private name — it had
/// been moved away again, or the read failed — which is itself a mismatch worth
/// recording.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedObject {
    /// The device the captured object was on.
    pub device: u64,
    /// The inode the captured object had.
    pub inode: u64,
    /// The kind the captured object had.
    pub kind: String,
}

impl CapturedObject {
    /// Validate every recorded fact strictly.
    pub fn validate(&self) -> Result<()> {
        if self.kind.is_empty() || self.kind.len() > 16 {
            return Err(Error::new(
                ErrorKind::UnknownState,
                "a captured object does not record its kind".to_owned(),
            ));
        }
        Ok(())
    }
}

/// One capture whose verification failed: a foreign object the deletion captured
/// by mistake and must never unlink.
///
/// It records the private name the object was captured to, the name it came from,
/// the identity the walk observed there and the identity the captured object
/// actually had, what the put-back answered and why the capture did not verify.
/// The record is written **before** the put-back is attempted, so a later run and
/// a replay know an object Barge never observed is somewhere in the leaf — under
/// its private name or back under its original one — and never unlink it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForeignCapture {
    /// The private `.barge-del-<nonce>` name the object was captured to.
    pub private: String,
    /// The name the object was captured from.
    pub original: String,
    /// The device the observation at the original name reported.
    pub device: u64,
    /// The inode that observation reported.
    pub inode: u64,
    /// The kind that observation reported: `file` or `directory`.
    pub kind: String,
    /// What the captured object actually was, when it could be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub captured: Option<CapturedObject>,
    /// What the put-back attempt answered.
    pub put_back: PutBackOutcome,
    /// Why the captured object was not verified, bounded.
    pub reason: String,
}

impl ForeignCapture {
    /// Validate every recorded fact strictly.
    pub fn validate(&self) -> Result<()> {
        if !is_capture_name(&self.private) {
            return Err(Error::new(
                ErrorKind::UnknownState,
                format!(
                    "`{}` is not a private `{CAPTURE_PREFIX}<nonce>` name",
                    self.private
                ),
            )
            .with_detail("private", self.private.clone().into()));
        }
        validate_entry_name(&self.original)?;
        validate_entry_kind(&self.kind, &self.private)?;
        if let Some(captured) = &self.captured {
            captured.validate()?;
        }
        if self.reason.is_empty() || self.reason.len() > FOREIGN_REASON_BYTES {
            return Err(Error::new(
                ErrorKind::UnknownState,
                format!("an unverified capture's reason must be 1 to {FOREIGN_REASON_BYTES} bytes"),
            )
            .with_detail("private", self.private.clone().into()));
        }
        Ok(())
    }
}

fn is_hex(value: &str, length: usize) -> bool {
    value.len() == length && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Where one intent record lives.
pub fn intent_path(state: &StatePaths, intent_id: &str) -> Result<PathBuf> {
    validate_identifier("intent_id", intent_id)?;
    Ok(state.intents_dir().join(format!("{intent_id}.json")))
}

/// Every intent record the state directory holds, bounded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntentSet {
    /// The records read, sorted by intent id.
    pub records: Vec<IntentRecord>,
    /// How many files the directory held, at least `records.len()`.
    pub total: usize,
    /// Whether records were left out because of the row cap.
    pub truncated: bool,
}

impl IntentSet {
    /// No intents.
    pub fn empty() -> Self {
        Self {
            records: Vec::new(),
            total: 0,
            truncated: false,
        }
    }

    /// The intents that still need a replay decision.
    pub fn pending(&self) -> impl Iterator<Item = &IntentRecord> {
        self.records.iter().filter(|record| record.is_pending())
    }
}

/// Publish one intent phase durably.
///
/// The publish is atomic — a private temporary file, sync, rename, directory
/// sync — so the phase is on disk before the caller performs the effect it
/// authorises. The [`IntentWrite`] token is the type-level proof that the write
/// came last in the fixed lock order.
pub fn publish_intent(
    _write: &IntentWrite,
    state: &StatePaths,
    record: &IntentRecord,
) -> Result<()> {
    record.validate()?;
    let mut bytes = serde_json::to_vec_pretty(record).map_err(|error| {
        Error::new(
            ErrorKind::Io,
            format!("could not encode the intent record: {error}"),
        )
    })?;
    bytes.push(b'\n');
    if bytes.len() > crate::contract::INTENT_CAP_BYTES {
        return Err(Error::new(
            ErrorKind::ResourceBound,
            format!(
                "the intent record would be {} bytes, over the {}-byte cap",
                bytes.len(),
                crate::contract::INTENT_CAP_BYTES
            ),
        )
        .with_detail("intent_id", record.intent_id.clone().into()));
    }
    let target = intent_path(state, &record.intent_id)?;
    #[cfg(test)]
    if let Some(error) = FAIL_INTENT_PUBLISH.with(|cell| cell.borrow_mut().take()) {
        return Err(
            Error::new(ErrorKind::Io, format!("cannot publish intent: {error}"))
                .with_detail("errno", serde_json::json!(error.raw_os_error()))
                .with_detail("path", target.display().to_string().into()),
        );
    }
    publish_atomic(&target, &bytes)
}

#[cfg(test)]
thread_local! {
    static FAIL_INTENT_PUBLISH: std::cell::RefCell<Option<std::io::Error>> = const { std::cell::RefCell::new(None) };
}

/// Inject an actual OS error at the next intent publication, before any effect.
#[cfg(test)]
pub(crate) fn fail_next_intent_publish(error: std::io::Error) {
    FAIL_INTENT_PUBLISH.with(|cell| *cell.borrow_mut() = Some(error));
}

// A test seam: fail the next publish of one phase on this thread, so a test can
// stop the deletion exactly between an effect and the durable step that follows
// it - the real unlink and the `removed` phase, for instance.
#[cfg(test)]
thread_local! {
    static FAIL_PHASE_PUBLISH: std::cell::RefCell<Option<IntentPhase>> =
        const { std::cell::RefCell::new(None) };
}

/// Fail the next publish that advances an intent to `phase`, on this thread only.
#[cfg(test)]
pub(crate) fn fail_next_phase_publish(phase: IntentPhase) {
    FAIL_PHASE_PUBLISH.with(|cell| *cell.borrow_mut() = Some(phase));
}

/// Whether this publish is the one a test asked to fail, consuming the request.
#[cfg(test)]
fn take_phase_failure(phase: IntentPhase) -> bool {
    FAIL_PHASE_PUBLISH.with(|cell| {
        let mut cell = cell.borrow_mut();
        if *cell == Some(phase) {
            *cell = None;
            true
        } else {
            false
        }
    })
}

/// Advance one intent to `phase`, publishing the new generation durably before
/// its effect. A phase may only move forward, so a stale writer cannot rewind an
/// intent it raced.
pub fn advance_intent(
    write: &IntentWrite,
    state: &StatePaths,
    record: &IntentRecord,
    phase: IntentPhase,
) -> Result<IntentRecord> {
    if phase <= record.phase {
        return Err(Error::new(
            ErrorKind::Usage,
            format!(
                "an intent at `{}` cannot advance to `{}`",
                record.phase.as_str(),
                phase.as_str()
            ),
        )
        .with_detail("intent_id", record.intent_id.clone().into()));
    }
    #[cfg(test)]
    {
        if take_phase_failure(phase) {
            return Err(Error::new(
                ErrorKind::Io,
                format!(
                    "the `{}` phase publish did not happen: the test stopped the process there",
                    phase.as_str()
                ),
            ));
        }
    }
    let mut next = record.clone();
    next.phase = phase;
    next.generation = record.generation.saturating_add(1);
    next.phase_ms = now_ms().max(record.phase_ms);
    publish_intent(write, state, &next)?;
    Ok(next)
}

/// Record the captures whose verification failed.
///
/// A record names the private name a replay must never unlink and the name the
/// object came from, with both the observation the capture failed to match and
/// the identity the captured object actually had. It is written **before** the
/// put-back is attempted, so a process that stops in between still leaves the
/// evidence. The phase does not change, but the generation still increases, so a
/// stale writer cannot drop a record it raced. A caller that records a foreign
/// capture stops the deletion, so no phase advances on a record it has not read
/// back, and a replay that reads one refuses to resume.
pub fn record_foreign_capture(
    write: &IntentWrite,
    state: &StatePaths,
    record: &IntentRecord,
    foreign: &ForeignCapture,
) -> Result<IntentRecord> {
    let mut next = record.clone();
    match next
        .foreign_captures
        .iter_mut()
        .find(|held| held.private == foreign.private)
    {
        // A second record of one capture is its later evidence - the put-back
        // answer - and replaces the first rather than duplicating it.
        Some(held) => *held = foreign.clone(),
        None => next.foreign_captures.push(foreign.clone()),
    }
    if next.foreign_captures.len() > INTENT_FOREIGN_CAPTURES {
        return Err(Error::new(
            ErrorKind::ResourceBound,
            format!("the intent would record more than {INTENT_FOREIGN_CAPTURES} foreign captures"),
        )
        .with_detail("intent_id", record.intent_id.clone().into()));
    }
    next.generation = record.generation.saturating_add(1);
    next.phase_ms = now_ms().max(record.phase_ms);
    publish_intent(write, state, &next)?;
    Ok(next)
}

/// Record the captures this operation has verified and may still unlink.
///
/// The list is a snapshot of the in-flight stack, so it is published before each
/// unlink and stays bounded by the depth bound. A capture must be recorded here,
/// with its verdict, before its private name is unlinked; a replay re-verifies the
/// recorded device, inode and kind before it deletes anything.
pub fn record_captures(
    write: &IntentWrite,
    state: &StatePaths,
    record: &IntentRecord,
    captures: &[CaptureRecord],
) -> Result<IntentRecord> {
    if captures.len() > INTENT_CAPTURES {
        return Err(Error::new(
            ErrorKind::ResourceBound,
            format!("the intent would record more than {INTENT_CAPTURES} captures"),
        )
        .with_detail("intent_id", record.intent_id.clone().into()));
    }
    let mut next = record.clone();
    next.captures = captures.to_vec();
    next.generation = record.generation.saturating_add(1);
    next.phase_ms = now_ms().max(record.phase_ms);
    publish_intent(write, state, &next)?;
    Ok(next)
}

/// Read one intent record strictly, bounded by its own cap.
pub fn read_intent(state: &StatePaths, intent_id: &str) -> Result<IntentRecord> {
    read_intent_with_deadline(state, intent_id, Deadline::after(READ_DEADLINE))
}

fn read_intent_with_deadline(
    state: &StatePaths,
    intent_id: &str,
    deadline: Deadline,
) -> Result<IntentRecord> {
    let path = intent_path(state, intent_id)?;
    let bytes = match read_bounded(
        &path,
        &Bounds::new(crate::contract::INTENT_CAP_BYTES, 1, deadline),
    ) {
        ReadOutcome::Present(bytes) => bytes,
        ReadOutcome::Absent => {
            return Err(Error::new(
                ErrorKind::UnknownState,
                format!("the intent {} is not there", path.display()),
            )
            .with_detail("path", path.display().to_string().into()))
        }
        ReadOutcome::Refused(error) => return Err(error),
    };
    // The major decides the field set: a v1 record is read as v1 and mapped, a
    // current record as itself, and an unknown major refuses rather than being
    // guessed at. Both shapes deny an unknown field, so a document whose fields
    // do not match its major is refused too.
    // The document is parsed twice, and the second parse is the one that decides:
    // the first reads only the major, because it decides which field set is the
    // document's. The record itself is then deserialized **from the original
    // bytes**, never from the first parse's value: `serde_json::Value` keeps the
    // last of two identical members, and a reader that reused it would let a
    // duplicate `foreign_captures` (or `schema`, or any nested field) erase the
    // evidence the earlier member recorded. The version-specific structs deny
    // unknown fields and refuse a repeated one, so a duplicated member is an
    // unreadable record rather than an ambiguous one.
    let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|error| {
        Error::new(
            ErrorKind::UnknownState,
            format!(
                "{} is not a `{}` document: {error}",
                path.display(),
                crate::contract::INTENT_SCHEMA
            ),
        )
        .with_detail("path", path.display().to_string().into())
    })?;
    let schema = value
        .get("schema")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| {
            Error::new(
                ErrorKind::UnknownState,
                format!(
                    "{} records no schema, so its field set is unknown",
                    path.display()
                ),
            )
            .with_detail("path", path.display().to_string().into())
        })?;
    let parse = |error: serde_json::Error| {
        Error::new(
            ErrorKind::UnknownState,
            format!("{} is not a `{schema}` document: {error}", path.display()),
        )
        .with_detail("path", path.display().to_string().into())
        .with_detail("schema", schema.to_owned().into())
    };
    let record = match schema.as_str() {
        crate::contract::INTENT_SCHEMA => {
            serde_json::from_slice::<IntentRecord>(&bytes).map_err(parse)?
        }
        crate::contract::INTENT_SCHEMA_V1 => serde_json::from_slice::<IntentV1>(&bytes)
            .map_err(parse)?
            .upgrade()?,
        other => {
            return Err(Error::new(
                ErrorKind::UnknownState,
                format!(
                    "`{other}` is not an intent major this build reads; it reads `{}` and `{}`",
                    crate::contract::INTENT_SCHEMA,
                    crate::contract::INTENT_SCHEMA_V1
                ),
            )
            .with_detail("path", path.display().to_string().into())
            .with_detail("schema", other.to_owned().into()))
        }
    };
    record.validate()?;
    Ok(record)
}

/// Read every intent record, bounded. A stray name, an unreadable record and a
/// truncated directory are unknown state: a pending intent is never skipped
/// because it was inconvenient to read.
pub fn read_intents(state: &StatePaths, max: usize) -> Result<IntentSet> {
    read_intents_with_deadline(state, max, Deadline::after(READ_DEADLINE))
}

/// Read a journal using the caller's remaining budget.
pub(crate) fn read_intents_with_deadline(
    state: &StatePaths,
    max: usize,
    deadline: Deadline,
) -> Result<IntentSet> {
    if deadline.expired() {
        return Err(Error::new(
            ErrorKind::ResourceBound,
            "the intent journal read deadline expired",
        ));
    }
    let directory = state.intents_dir();
    let names = match std::fs::read_dir(&directory) {
        Ok(entries) => entries,
        #[cfg(not(test))]
        Err(error) if error.kind() == IoErrorKind::NotFound => return Ok(IntentSet::empty()),
        #[cfg(test)]
        Err(error) if error.kind() == IoErrorKind::NotFound => {
            #[cfg(test)]
            crate::tape::read_absent("state::read_intents_with_deadline", &directory);
            return Ok(IntentSet::empty());
        }
        Err(error) => {
            #[cfg(test)]
            crate::tape::listing_refused(
                "state::read_intents_with_deadline",
                &directory,
                &crate::tape::errno_class(&error),
            );
            #[cfg(test)]
            return Err(Error::new(
                ErrorKind::UnknownState,
                format!("cannot read the intent journal: {error}"),
            )
            .with_detail("path", directory.display().to_string().into()));
            #[cfg(not(test))]
            return Err(Error::new(
                ErrorKind::UnknownState,
                format!("cannot read the intent journal: {error}"),
            )
            .with_detail("path", directory.display().to_string().into()));
        }
    };
    if deadline.expired() {
        return Err(Error::new(
            ErrorKind::ResourceBound,
            "the intent journal read deadline expired",
        ));
    }
    let mut records = Vec::new();
    let mut total = 0usize;
    for entry in names {
        let entry = entry.map_err(|error| {
            Error::new(
                ErrorKind::UnknownState,
                format!("cannot read the intent journal: {error}"),
            )
            .with_detail("path", directory.display().to_string().into())
        })?;
        total = total.saturating_add(1);
        if total > max || deadline.expired() {
            return Err(Error::new(
                ErrorKind::ResourceBound,
                format!("the intent journal holds more than {max} records or ran out of time"),
            )
            .with_detail("path", directory.display().to_string().into())
            .with_detail("rows", total.into())
            .with_detail("cap", max.into()));
        }
        let name = entry.file_name();
        #[cfg(test)]
        crate::tape::dir_entry("state::read_intents_with_deadline", None, &name);
        let Some(stem) = name.to_str().and_then(|name| name.strip_suffix(".json")) else {
            return Err(Error::new(
                ErrorKind::UnknownState,
                format!(
                    "the intent journal holds `{}`, which is not an intent record",
                    name.to_string_lossy()
                ),
            )
            .with_detail("path", directory.display().to_string().into()));
        };
        match read_intent_with_deadline(state, stem, deadline) {
            Ok(record) => records.push(record),
            Err(error) if error.kind() == ErrorKind::UnknownState => {
                // A record that vanished between the listing and the read was
                // settled by another process; nothing is inferred from it.
                if entry.path().exists() {
                    return Err(error);
                }
            }
            Err(error) => return Err(error),
        }
    }
    records.sort_by(|left, right| left.intent_id.cmp(&right.intent_id));
    #[cfg(test)]
    crate::tape::listing(
        "state::read_intents_with_deadline",
        &directory,
        total,
        records.len(),
        false,
    );
    Ok(IntentSet {
        records,
        total,
        truncated: false,
    })
}

/// Remove one intent record. Only a rolled-back `intent` phase is removed: a
/// settled intent keeps its `deleted` phase as the journal's own record.
pub fn remove_intent(state: &StatePaths, intent_id: &str) -> Result<bool> {
    let path = intent_path(state, intent_id)?;
    match std::fs::remove_file(&path) {
        Ok(()) => {
            let _ = sync_directory(
                path.parent()
                    .ok_or_else(|| Error::new(ErrorKind::Usage, "the intent has no parent"))?,
            );
            Ok(true)
        }
        Err(error) if error.kind() == IoErrorKind::NotFound => Ok(false),
        Err(error) => Err(Error::new(
            ErrorKind::Io,
            format!("cannot remove {}: {error}", path.display()),
        )
        .with_detail("path", path.display().to_string().into())),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use std::os::unix::fs::PermissionsExt;

    use super::{
        now_ms, publish_atomic, publish_atomic_with, read_bounded, validate_identifier, Bounds,
        Deadline, LockMode, LockRank, LockSet, ReadOutcome, StateDirSource, StateLock, StatePaths,
    };
    use crate::error::{Error, ErrorKind, Result};
    use crate::os::{AccountFacts, VolumeUuid};
    use crate::testkit::TempDir;

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

        fn tool_state_dir(&self, tool: &str) -> Result<PathBuf> {
            Ok(self
                .home
                .join("Library/Application Support")
                .join(tool)
                .join("state"))
        }

        fn tool_cache_dir(&self, tool: &str) -> Result<PathBuf> {
            Ok(self.home.join("Library/Caches").join(tool))
        }
    }

    fn account() -> FakeAccount {
        FakeAccount {
            home: PathBuf::from("/Users/a-fixture-account"),
        }
    }

    fn deadline() -> Deadline {
        Deadline::after(Duration::from_secs(10))
    }

    fn bounds(max_bytes: usize) -> Bounds {
        Bounds::new(max_bytes, 1, deadline())
    }

    #[test]
    fn state_resolves_from_the_account_and_never_from_home() {
        let account = account();
        let state = StatePaths::resolve(&account, None).expect("the account root resolves");
        assert_eq!(
            state.root(),
            Path::new("/Users/a-fixture-account/Library/Application Support/barge/state")
        );
        assert_eq!(state.source(), StateDirSource::Account);
    }

    #[test]
    fn the_fixture_override_moves_the_state_directory_and_says_so() {
        let account = account();
        let fixture = TempDir::new("state-override");
        let state =
            StatePaths::resolve(&account, Some(fixture.path())).expect("an absolute override");
        assert_eq!(state.root(), fixture.path());
        assert_eq!(state.source(), StateDirSource::EnvironmentOverride);
    }

    #[test]
    fn a_relative_override_is_refused() {
        let account = account();
        let error = StatePaths::resolve(&account, Some(Path::new("relative/state")))
            .expect_err("a relative authority is refused");
        assert_eq!(error.kind(), ErrorKind::Usage);
    }

    #[test]
    fn the_layout_names_read_the_design() {
        let state = StatePaths::resolve(&account(), None).expect("resolves");
        let volume = VolumeUuid::parse("A1B2C3D4-E5F6-4A7B-8C9D-0E1F2A3B4C5D").expect("a UUID");
        assert_eq!(state.registry_path(), state.root().join("registry.json"));
        assert_eq!(
            state.registry_lock_path(),
            state.root().join("locks/registry.lock")
        );
        assert_eq!(
            state.volume_lock_path(&volume),
            state
                .root()
                .join("locks/volume-A1B2C3D4-E5F6-4A7B-8C9D-0E1F2A3B4C5D.lock")
        );
        assert_eq!(
            state
                .root_lock_path("0123456789abcdef0123456789abcdef")
                .expect("a well-formed root id"),
            state
                .root()
                .join("locks/root-0123456789abcdef0123456789abcdef.lock")
        );
        assert!(state.root_lock_path("../escape").is_err());
    }

    #[test]
    fn an_identifier_is_thirty_two_hexadecimal_digits() {
        for value in [
            "../x",
            "",
            "0123456789abcdef",
            "z123456789abcdef0123456789abcdef",
        ] {
            assert!(validate_identifier("root_id", value).is_err(), "{value}");
        }
        validate_identifier("root_id", "0123456789ABCDEF0123456789abcdef").expect("hex digits");
    }

    #[test]
    fn state_is_created_lazily_and_privately() {
        let fixture = TempDir::new("state-initialize");
        let state =
            StatePaths::resolve(&account(), Some(&fixture.path().join("state"))).expect("resolves");
        assert!(!state
            .is_initialized()
            .expect("an absent directory is initialised: false"));
        state.initialize().expect("initialising creates the layout");
        assert!(state.is_initialized().expect("the directory reads"));
        for directory in [
            state.areas_dir(),
            state.placements_dir(),
            state.holds_dir(),
            state.activity_dir(),
            state.intents_dir(),
            state.journal_dir(),
            state.locks_dir(),
        ] {
            assert!(directory.is_dir(), "{} exists", directory.display());
        }
        state
            .initialize()
            .expect("initialising twice is not an error");
    }

    #[test]
    fn a_file_where_the_state_directory_belongs_is_unknown_state() {
        let fixture = TempDir::new("state-not-a-directory");
        let path = fixture.path().join("state");
        std::fs::write(&path, b"not a directory").expect("the fixture file is written");
        let state = StatePaths::resolve(&account(), Some(&path)).expect("resolves");
        let error = state
            .is_initialized()
            .expect_err("a file is not an uninitialised directory");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
    }

    #[test]
    fn a_publish_writes_the_bytes_privately_and_replaces_them() {
        let fixture = TempDir::new("state-publish");
        let target = fixture.path().join("registry.json");
        publish_atomic(&target, b"first").expect("the first publish");
        assert_eq!(std::fs::read(&target).expect("the file reads"), b"first");
        let mode = std::fs::metadata(&target)
            .expect("the file has metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "the published file is private");
        publish_atomic(&target, b"second").expect("the second publish");
        assert_eq!(std::fs::read(&target).expect("the file reads"), b"second");
        let temporaries = std::fs::read_dir(fixture.path())
            .expect("the fixture lists")
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp."))
            .count();
        assert_eq!(
            temporaries, 0,
            "a completed publish leaves no temporary file"
        );
    }

    #[test]
    fn an_interrupted_publish_leaves_the_previous_bytes() {
        let fixture = TempDir::new("state-publish-interrupted");
        let target = fixture.path().join("registry.json");
        publish_atomic(&target, b"the previous bytes").expect("the first publish");
        let error = publish_atomic_with(&target, b"the new bytes", |temporary| {
            assert!(
                temporary.exists(),
                "the interruption happens after the temporary file was synced"
            );
            Err(Error::new(
                ErrorKind::Io,
                "an injected failure before the rename",
            ))
        })
        .expect_err("the interrupted publish fails");
        assert_eq!(error.kind(), ErrorKind::Io);
        assert_eq!(
            std::fs::read(&target).expect("the file reads"),
            b"the previous bytes",
            "the previous bytes are intact"
        );
        let leftovers = std::fs::read_dir(fixture.path())
            .expect("the fixture lists")
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp."))
            .count();
        assert_eq!(
            leftovers, 0,
            "the refused publish cleaned its temporary file up"
        );
    }

    #[test]
    fn a_bounded_read_reports_absence_rather_than_empty_bytes() {
        let fixture = TempDir::new("state-read-absent");
        let outcome = read_bounded(&fixture.path().join("absent.json"), &bounds(16));
        let bytes = outcome.bytes().expect("absence is not a failure");
        assert_eq!(bytes, None);
    }

    #[test]
    fn a_bounded_read_returns_the_bytes_it_was_given() {
        let fixture = TempDir::new("state-read-present");
        let path = fixture.path().join("registry.json");
        std::fs::write(&path, b"{}").expect("the fixture file is written");
        match read_bounded(&path, &bounds(16)) {
            ReadOutcome::Present(bytes) => assert_eq!(bytes, b"{}"),
            other => panic!("expected the bytes, got {other:?}"),
        }
    }

    #[test]
    fn an_oversized_file_is_refused_before_its_body_is_read() {
        let fixture = TempDir::new("state-read-oversized");
        let path = fixture.path().join("registry.json");
        std::fs::write(&path, vec![b'x'; 32]).expect("the fixture file is written");
        let error = read_bounded(&path, &bounds(16))
            .bytes()
            .expect_err("an oversized file is refused");
        assert_eq!(error.kind(), ErrorKind::ResourceBound);
        assert_eq!(error.detail()["cap"], 16);
    }

    #[test]
    fn a_link_or_a_directory_is_not_read_as_a_file() {
        let fixture = TempDir::new("state-read-not-a-file");
        let target = fixture.path().join("real.json");
        std::fs::write(&target, b"{}").expect("the fixture file is written");
        let link = fixture.path().join("link.json");
        std::os::unix::fs::symlink(&target, &link).expect("the fixture link is made");
        let error = read_bounded(&link, &bounds(16))
            .bytes()
            .expect_err("a link is refused rather than followed");
        assert_eq!(error.kind(), ErrorKind::UnknownState);

        let directory = fixture.path().join("directory.json");
        std::fs::create_dir(&directory).expect("the fixture directory is made");
        let error = read_bounded(&directory, &bounds(16))
            .bytes()
            .expect_err("a directory is not a file");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
    }

    #[test]
    fn an_expired_deadline_refuses_before_the_file_is_opened() {
        let fixture = TempDir::new("state-read-deadline");
        let path = fixture.path().join("registry.json");
        std::fs::write(&path, b"{}").expect("the fixture file is written");
        let expired = Bounds::new(16, 1, Deadline::after(Duration::from_secs(0)));
        let error = read_bounded(&path, &expired)
            .bytes()
            .expect_err("an expired deadline refuses");
        assert_eq!(error.kind(), ErrorKind::ResourceBound);
    }

    #[test]
    fn the_now_stamp_is_a_plausible_millisecond_clock() {
        // 2020-01-01 in milliseconds; a clock before that is reported as-is, not
        // corrected, but it must still be a real clock reading.
        assert!(now_ms() > 1_577_836_800_000);
    }

    fn mode_of(path: &Path) -> u32 {
        std::fs::metadata(path)
            .expect("the path exists")
            .permissions()
            .mode()
            & 0o777
    }

    fn fixture_account_at(home: &Path) -> FakeAccount {
        FakeAccount {
            home: home.to_path_buf(),
        }
    }

    #[test]
    fn a_fresh_account_gets_its_barge_parent_privately() {
        let fixture = TempDir::new("state-fresh-account");
        std::fs::create_dir_all(fixture.path().join("Library/Application Support"))
            .expect("the fixture's Application Support");
        let state = StatePaths::resolve(&fixture_account_at(fixture.path()), None)
            .expect("the account root resolves");
        assert!(!state.is_initialized().expect("an absent root reads"));
        state.initialize().expect("the barge parent and layout");
        let parent = fixture.path().join("Library/Application Support/barge");
        assert_eq!(mode_of(&parent), 0o700);
        assert_eq!(mode_of(state.root()), 0o700);
        assert!(state.is_initialized().expect("the root reads"));
        assert!(state.locks_dir().is_dir());
    }

    #[test]
    fn an_absent_application_support_is_refused_and_nothing_is_created() {
        let fixture = TempDir::new("state-no-application-support");
        let state = StatePaths::resolve(&fixture_account_at(fixture.path()), None)
            .expect("the account root resolves");
        let error = state
            .initialize()
            .expect_err("Application Support is never created");
        assert!(error.to_string().contains("Application Support"), "{error}");
        assert!(!fixture.path().join("Library").exists());
    }

    #[test]
    fn a_symlinked_barge_parent_is_refused_and_not_followed() {
        let fixture = TempDir::new("state-symlinked-parent");
        std::fs::create_dir_all(fixture.path().join("Library/Application Support"))
            .expect("the fixture's Application Support");
        let target = fixture.path().join("elsewhere");
        std::fs::create_dir(&target).expect("the link target");
        std::os::unix::fs::symlink(
            &target,
            fixture.path().join("Library/Application Support/barge"),
        )
        .expect("the symbolic link");
        let state = StatePaths::resolve(&fixture_account_at(fixture.path()), None)
            .expect("the account root resolves");
        let error = state.initialize().expect_err("a symbolic link is refused");
        assert!(error.to_string().contains("not a directory"), "{error}");
        assert!(
            !target.join("state").exists(),
            "nothing is written through the link"
        );
    }

    #[test]
    fn an_explicit_state_dir_with_an_absent_parent_still_refuses() {
        let fixture = TempDir::new("state-explicit-absent-parent");
        let state = StatePaths::resolve(&account(), Some(&fixture.path().join("absent/state")))
            .expect("an absolute override resolves");
        assert!(state.initialize().is_err());
        assert!(!fixture.path().join("absent").exists());
    }

    #[test]
    fn a_read_only_check_on_a_fresh_account_creates_nothing() {
        let fixture = TempDir::new("state-read-only-fresh");
        let state = StatePaths::resolve(&fixture_account_at(fixture.path()), None)
            .expect("the account root resolves");
        assert!(!state.is_initialized().expect("an absent root reads"));
        assert!(!fixture.path().join("Library").exists());
    }

    fn initialized_state(fixture: &TempDir) -> StatePaths {
        let state =
            StatePaths::resolve(&account(), Some(&fixture.path().join("state"))).expect("resolves");
        state.initialize().expect("the layout is created");
        state
    }

    #[test]
    fn the_lock_order_is_registry_then_volume_then_root_then_intent() {
        let fixture = TempDir::new("state-locks-order");
        let state = initialized_state(&fixture);
        let volume = VolumeUuid::parse("A1B2C3D4-E5F6-4A7B-8C9D-0E1F2A3B4C5D").expect("a UUID");
        let mut locks = LockSet::with_deadline(state, deadline());
        let registry = locks.lock_registry().expect("the registry lock is first");
        assert_eq!(registry.rank(), LockRank::Registry);
        assert_eq!(registry.mode(), LockMode::Exclusive);
        let volume_lock = locks
            .lock_volume(&volume)
            .expect("the volume lock is second");
        assert_eq!(volume_lock.rank(), LockRank::Volume);
        let root = locks
            .lock_root("0123456789abcdef0123456789abcdef")
            .expect("the root lock is third");
        assert_eq!(root.rank(), LockRank::Root);
        let intent = locks
            .begin_intent_write()
            .expect("the intent write is last");
        assert_eq!(intent.rank(), LockRank::IntentWrite);
        assert_eq!(locks.highest(), Some(LockRank::IntentWrite));
    }

    #[test]
    fn an_out_of_order_lock_is_refused() {
        let fixture = TempDir::new("state-locks-out-of-order");
        let state = initialized_state(&fixture);
        let volume = VolumeUuid::parse("A1B2C3D4-E5F6-4A7B-8C9D-0E1F2A3B4C5D").expect("a UUID");

        let mut locks = LockSet::with_deadline(state.clone(), deadline());
        let _root = locks
            .lock_root("0123456789abcdef0123456789abcdef")
            .expect("the root lock is first in this set");
        let error = locks
            .lock_registry()
            .expect_err("the registry lock cannot follow a root lock");
        assert_eq!(error.kind(), ErrorKind::Usage);
        assert_eq!(error.detail()["held"], "root");
        assert_eq!(error.detail()["requested"], "registry");

        let mut locks = LockSet::with_deadline(state.clone(), deadline());
        let _volume = locks
            .lock_volume(&volume)
            .expect("the volume lock is first");
        assert!(
            locks.lock_volume(&volume).is_err(),
            "a rank is not re-entered"
        );
        assert!(locks.begin_intent_write().is_ok());

        let mut locks = LockSet::with_deadline(state, deadline());
        assert!(locks
            .observe_registry()
            .expect("an absent lock file is not a lock")
            .is_none());
        assert_eq!(
            locks.highest(),
            None,
            "an absent lock file is not recorded as held"
        );
    }

    #[test]
    fn a_lock_held_by_another_owner_is_refused_at_the_deadline() {
        let fixture = TempDir::new("state-locks-contended");
        let state = initialized_state(&fixture);
        let path = state.registry_lock_path();
        let first = StateLock::exclusive(&path, LockRank::Registry, deadline())
            .expect("the first owner takes the lock");
        let short = Deadline::after(Duration::from_millis(50));
        let error = StateLock::exclusive(&path, LockRank::Registry, short)
            .expect_err("a live owner is never taken over");
        assert_eq!(error.kind(), ErrorKind::Locked);
        drop(first);
        StateLock::exclusive(&path, LockRank::Registry, deadline())
            .expect("the lock is free once the owner dropped it");
    }

    #[test]
    fn an_observation_takes_a_shared_lock_on_an_existing_file_and_creates_nothing() {
        let fixture = TempDir::new("state-locks-observe");
        let state = initialized_state(&fixture);
        let path = state.registry_lock_path();
        let mut locks = LockSet::with_deadline(state.clone(), deadline());
        assert!(locks
            .observe_registry()
            .expect("no file, no lock")
            .is_none());
        assert!(!path.exists(), "observing creates no lock file");

        StateLock::exclusive(&path, LockRank::Registry, deadline())
            .expect("a writer creates the lock file");
        let mut locks = LockSet::with_deadline(state, deadline());
        let lock = locks
            .observe_registry()
            .expect("an existing lock file is shared")
            .expect("the shared lock is taken");
        assert_eq!(lock.mode(), LockMode::Shared);
    }

    #[test]
    fn a_mutation_cannot_create_the_state_directory_through_the_lock_path() {
        let fixture = TempDir::new("state-locks-uninitialized");
        let state =
            StatePaths::resolve(&account(), Some(&fixture.path().join("state"))).expect("resolves");
        let mut locks = LockSet::with_deadline(state, deadline());
        let error = locks
            .lock_registry()
            .expect_err("a missing locks directory is reported, not created");
        assert_eq!(error.kind(), ErrorKind::Io);
    }
}

#[cfg(test)]
mod intent_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::path::PathBuf;

    use super::{
        advance_intent, publish_intent, read_intent, read_intents, record_foreign_capture,
        CapturedObject, ForeignCapture, IntentPhase, IntentRecord, IntentUnit, LockSet,
        PutBackOutcome, StatePaths, CAPTURE_PREFIX, INTENT_FOREIGN_CAPTURES, QUARANTINE_PREFIX,
    };
    use crate::error::ErrorKind;
    use crate::os::VolumeUuid;
    use crate::testkit::{FakeAccount, TempDir};

    const UUID: &str = "A6AC3ED7-33EA-46B4-8082-7254CF291ED2";

    fn fixture(label: &str) -> (TempDir, StatePaths, VolumeUuid) {
        let dir = TempDir::new(label);
        let state = StatePaths::resolve(
            &FakeAccount {
                home: dir.path().to_path_buf(),
            },
            Some(&dir.join("state")),
        )
        .expect("the fixture state directory resolves");
        state.initialize().expect("the layout exists");
        (dir, state, VolumeUuid::parse(UUID).expect("a UUID"))
    }

    fn record(uuid: &VolumeUuid) -> IntentRecord {
        let intent_id = format!("{:032x}", 7);
        let now = super::now_ms();
        IntentRecord {
            schema: crate::contract::INTENT_SCHEMA.to_owned(),
            intent_id: intent_id.clone(),
            volume_uuid: uuid.clone(),
            mount_hint: PathBuf::from("/Volumes/Builds"),
            root_dir: String::new(),
            folder: "example/pool".to_owned(),
            unit: IntentUnit {
                parent: String::new(),
                name: format!("{:032x}", 8),
                kind: crate::unit::UnitKind::Directory,
                device: 16_777_219,
                inode: 4_242,
                marker: Some(crate::unit::UnitMarker::new(
                    crate::unit::MarkerFamily::Barge,
                    "a".repeat(64),
                )),
                root_id: None,
                capture_name: format!("{QUARANTINE_PREFIX}{intent_id}"),
                grant: crate::unit::UnitGrant::native_root(format!("{:032x}", 8)),
            },
            plan_digest: "b".repeat(64),
            phase: IntentPhase::Intent,
            generation: 1,
            recorded_ms: now,
            phase_ms: now,
            captures: Vec::new(),
            foreign_captures: Vec::new(),
            identity: None,
            absence: None,
        }
    }

    #[test]
    fn an_intent_advances_forward_only_and_each_phase_reads_back() {
        let (_dir, state, uuid) = fixture("intent-advance");
        let intent = record(&uuid);
        let mut locks = LockSet::new(state.clone());
        let _volume = locks.lock_volume(&uuid).expect("the volume lock");
        let _root = locks.lock_root(&intent.unit.name).expect("the root lock");
        let write = locks.begin_intent_write().expect("the intent step");

        publish_intent(&write, &state, &intent).expect("the intent publishes");
        let read = read_intent(&state, &intent.intent_id).expect("the intent reads back");
        assert_eq!(read.phase, IntentPhase::Intent);
        assert!(read.is_pending());

        let renamed = advance_intent(&write, &state, &read, IntentPhase::Renamed)
            .expect("the phase advances");
        assert_eq!(renamed.generation, 2);
        let read = read_intent(&state, &intent.intent_id).expect("the intent reads back");
        assert_eq!(read.phase, IntentPhase::Renamed);

        let rewind = advance_intent(&write, &state, &read, IntentPhase::Intent)
            .expect_err("an intent never rewinds");
        assert_eq!(rewind.kind(), ErrorKind::Usage);
        assert_eq!(
            read_intent(&state, &intent.intent_id)
                .expect("the intent reads back")
                .phase,
            IntentPhase::Renamed,
            "the refused rewind changed nothing"
        );

        let settled = advance_intent(&write, &state, &read, IntentPhase::MarkerLast)
            .and_then(|next| advance_intent(&write, &state, &next, IntentPhase::Deleted))
            .expect("the intent settles");
        assert!(!settled.is_pending());
        let set = read_intents(&state, 64).expect("the journal reads");
        assert_eq!(set.total, 1);
        assert!(
            set.pending().next().is_none(),
            "a settled intent is not pending work"
        );
    }

    /// The identity evidence an intent for the fixture unit carries.
    fn identity_evidence(intent: &IntentRecord) -> crate::evidence::IdentityEvidence {
        crate::evidence::IdentityEvidence {
            verdict: "verified".to_owned(),
            reason: None,
            inode: Some(intent.unit.inode),
            device_at_record: Some(intent.unit.device),
            device_live: Some(intent.unit.device.saturating_add(1)),
            remount: Some(crate::evidence::RemountEvidenceRow {
                device_changed: true,
            }),
            mount_moved: Some(false),
            marker_digest: intent
                .unit
                .marker
                .as_ref()
                .map(|marker| marker.digest.clone()),
        }
    }

    /// The absence proof an intent for the fixture unit carries.
    fn absence_evidence() -> crate::evidence::AbsenceEvidence {
        crate::evidence::AbsenceEvidence {
            schema: crate::evidence::ABSENCE_SCHEMA.to_owned(),
            path: PathBuf::from("/Users/tom/Dev/projects/example"),
            verdict: "absent".to_owned(),
            anchor: crate::evidence::AbsenceAnchor {
                kind: "root".to_owned(),
                path: PathBuf::from("/"),
            },
            ancestor: Some(crate::evidence::AbsenceAncestor {
                path: PathBuf::from("/Users/tom/Dev/projects"),
                inode: 4_242,
                device: 16_777_220,
                volume: crate::evidence::AbsenceVolumeRow {
                    kind: "boot_data".to_owned(),
                    volume_uuid: UUID.to_owned(),
                    file_system: "apfs".to_owned(),
                },
            }),
            leaf: Some(crate::evidence::AbsenceLeaf {
                name: "example".to_owned(),
                result: "enoent".to_owned(),
            }),
            components_walked: 4,
            proved_ms: 7,
            reason: None,
        }
    }

    #[test]
    fn an_intent_carries_the_plan_s_identity_and_absence_evidence() {
        // Stage 4 F4: the plan's two documents are optional additions inside the
        // v2 major, so a record that carries them reads back exactly and a record
        // that does not - including every v1 record - still reads.
        let (_dir, state, uuid) = fixture("intent-evidence");
        let mut intent = record(&uuid);
        intent.identity = Some(identity_evidence(&intent));
        intent.absence = Some(absence_evidence());
        let mut locks = LockSet::new(state.clone());
        let _volume = locks.lock_volume(&uuid).expect("the volume lock");
        let _root = locks.lock_root(&intent.unit.name).expect("the root lock");
        let write = locks.begin_intent_write().expect("the intent step");
        publish_intent(&write, &state, &intent).expect("the intent publishes");

        let read = read_intent(&state, &intent.intent_id).expect("the intent reads back");
        assert_eq!(read.identity, intent.identity);
        assert_eq!(read.absence, intent.absence);
        let document: serde_json::Value = serde_json::from_slice(
            &std::fs::read(super::intent_path(&state, &intent.intent_id).unwrap()).unwrap(),
        )
        .expect("the document decodes");
        assert_eq!(document["identity"]["verdict"], "verified");
        assert_eq!(document["absence"]["schema"], "barge.absence/1");

        // A record written before F4 carries neither, and still reads.
        let plain = record(&uuid);
        publish_intent(&write, &state, &plain).expect("the plain intent publishes");
        let read = read_intent(&state, &plain.intent_id).expect("the plain intent reads");
        assert!(read.identity.is_none() && read.absence.is_none());
    }

    #[test]
    fn evidence_that_does_not_describe_this_unit_is_refused() {
        let (_dir, state, uuid) = fixture("intent-evidence-refused");
        let intent = record(&uuid);
        let mut locks = LockSet::new(state.clone());
        let _volume = locks.lock_volume(&uuid).expect("the volume lock");
        let _root = locks.lock_root(&intent.unit.name).expect("the root lock");
        let write = locks.begin_intent_write().expect("the intent step");

        let mut anonymous = intent.clone();
        let mut evidence = identity_evidence(&anonymous);
        evidence.verdict = "unknown".to_owned();
        anonymous.identity = Some(evidence);
        let error = publish_intent(&write, &state, &anonymous)
            .expect_err("an intent is written only under a verified identity");
        assert!(
            error.message().contains("verified root"),
            "{}",
            error.message()
        );

        let mut other_inode = intent.clone();
        let mut evidence = identity_evidence(&other_inode);
        evidence.inode = Some(other_inode.unit.inode.saturating_add(1));
        other_inode.identity = Some(evidence);
        let error = publish_intent(&write, &state, &other_inode)
            .expect_err("the evidence must name this unit's inode");
        assert!(
            error.message().contains("another inode"),
            "{}",
            error.message()
        );

        let mut other_bytes = intent.clone();
        let mut evidence = identity_evidence(&other_bytes);
        evidence.marker_digest = Some("c".repeat(64));
        other_bytes.identity = Some(evidence);
        let error = publish_intent(&write, &state, &other_bytes)
            .expect_err("the evidence must name this unit's marker bytes");
        assert!(
            error.message().contains("marker bytes"),
            "{}",
            error.message()
        );

        let mut present = intent.clone();
        present.absence = Some(crate::evidence::AbsenceEvidence {
            verdict: "present".to_owned(),
            ancestor: None,
            leaf: None,
            ..absence_evidence()
        });
        let error = publish_intent(&write, &state, &present)
            .expect_err("an intent records only an absent path");
        assert!(
            error.message().contains("absent path"),
            "{}",
            error.message()
        );

        // A document whose nested field set is not this one is unknown state, not
        // a guess: the evidence denies unknown fields like every record Barge reads.
        let path = super::intent_path(&state, &intent.intent_id).expect("the path");
        let value = serde_json::json!({
            "schema": crate::contract::INTENT_SCHEMA,
            "intent_id": intent.intent_id,
            "volume_uuid": uuid.as_str(),
            "mount_hint": intent.mount_hint,
            "root_dir": intent.root_dir,
            "folder": intent.folder,
            "unit": serde_json::to_value(&intent.unit).expect("the unit encodes"),
            "plan_digest": intent.plan_digest,
            "phase": "intent",
            "generation": 1,
            "recorded_ms": 1,
            "phase_ms": 1,
            "identity": {
                "verdict": "verified",
                "inode": intent.unit.inode,
                "device_at_record": intent.unit.device,
                "device_live": intent.unit.device,
                "remount": {"device_changed": false},
                "mount_moved": false,
                "marker_digest": "a".repeat(64),
                "a_later_field": true,
            },
        });
        std::fs::write(
            &path,
            serde_json::to_vec_pretty(&value).expect("the document encodes"),
        )
        .expect("the document is written");
        let error = read_intent(&state, &intent.intent_id)
            .expect_err("an unknown evidence field is refused");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
    }

    #[test]
    fn an_intent_record_with_an_unknown_field_is_refused() {
        let (_dir, state, uuid) = fixture("intent-unknown-field");
        let intent = record(&uuid);
        let path = super::intent_path(&state, &intent.intent_id).expect("the path");
        let mut value = serde_json::to_value(&intent).expect("the record encodes");
        value["a_later_field"] = serde_json::Value::Bool(true);
        std::fs::write(
            &path,
            serde_json::to_vec_pretty(&value).expect("the record encodes"),
        )
        .expect("the record is written");

        let error = read_intents(&state, 64).expect_err("an unknown field is refused");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
    }

    /// A `barge.intent/1` record exactly as S5 first wrote it.
    fn v1_value(uuid: &VolumeUuid, intent_id: &str) -> serde_json::Value {
        let root_id = format!("{:032x}", 8);
        serde_json::json!({
            "schema": "barge.intent/1",
            "intent_id": intent_id,
            "root_id": root_id,
            "volume_uuid": uuid.to_string(),
            "mount_hint": "/Volumes/Builds",
            "root_dir": "",
            "folder": "example/pool",
            "root_name": root_id,
            "quarantine_name": format!("{QUARANTINE_PREFIX}{intent_id}"),
            "device": 16_777_219,
            "inode": 4_242,
            "marker_digest": "a".repeat(64),
            "plan_digest": "b".repeat(64),
            "phase": "renamed",
            "generation": 2,
            "recorded_ms": 1,
            "phase_ms": 2,
        })
    }

    #[test]
    fn a_v1_record_reads_as_a_barge_marked_directory_unit() {
        let (_dir, state, uuid) = fixture("intent-v1");
        let intent_id = format!("{:032x}", 9);
        let path = super::intent_path(&state, &intent_id).expect("the path");
        std::fs::write(
            &path,
            serde_json::to_vec_pretty(&v1_value(&uuid, &intent_id)).expect("the record encodes"),
        )
        .expect("the v1 record is written");

        let read = read_intent(&state, &intent_id).expect("a v1 record stays readable");
        assert_eq!(read.schema, crate::contract::INTENT_SCHEMA);
        assert_eq!(read.unit.kind, crate::unit::UnitKind::Directory);
        assert_eq!(read.unit.parent, "");
        assert_eq!(read.unit.name, format!("{:032x}", 8));
        assert_eq!(
            read.unit.capture_name,
            format!("{QUARANTINE_PREFIX}{intent_id}")
        );
        assert_eq!(read.unit.device, 16_777_219);
        assert_eq!(read.unit.inode, 4_242);
        let marker = read.unit.marker.as_ref().expect("a v1 root is marked");
        assert_eq!(marker.family, crate::unit::MarkerFamily::Barge);
        assert_eq!(marker.filename, crate::unit::BARGE_MARKER_FILE);
        assert_eq!(marker.digest, "a".repeat(64));
        assert_eq!(
            read.unit.grant,
            crate::unit::UnitGrant::native_root(format!("{:032x}", 8))
        );
        assert_eq!(read.phase, crate::state::IntentPhase::Renamed);
    }

    #[test]
    fn an_intent_field_set_that_does_not_match_its_major_is_refused() {
        let (_dir, state, uuid) = fixture("intent-field-set");
        let intent_id = format!("{:032x}", 10);
        let path = super::intent_path(&state, &intent_id).expect("the path");
        let mut v1_with_a_v2_field = v1_value(&uuid, &intent_id);
        v1_with_a_v2_field["unit"] = serde_json::json!({"name": "late"});
        let v2 = serde_json::to_value(record(&uuid)).expect("the record encodes");
        let mut v2_with_a_v1_field = v2.clone();
        v2_with_a_v1_field["root_id"] = serde_json::json!(format!("{:032x}", 8));
        let mut v2_without_its_unit = v2;
        v2_without_its_unit
            .as_object_mut()
            .expect("an object")
            .remove("unit");
        for (label, value) in [
            ("a v1 field set with a v2 field", v1_with_a_v2_field),
            ("a v2 field set with a v1 field", v2_with_a_v1_field),
            ("a v2 field set without its unit", v2_without_its_unit),
        ] {
            std::fs::write(
                &path,
                serde_json::to_vec_pretty(&value).expect("the record encodes"),
            )
            .expect("the record is written");
            let error = read_intent(&state, &intent_id).expect_err(label);
            assert_eq!(error.kind(), ErrorKind::UnknownState, "{label}");
        }
    }

    #[test]
    fn an_unknown_intent_major_is_unknown_state() {
        let (_dir, state, uuid) = fixture("intent-major");
        let intent_id = format!("{:032x}", 11);
        let path = super::intent_path(&state, &intent_id).expect("the path");
        let mut value = serde_json::to_value(record(&uuid)).expect("the record encodes");
        value["schema"] = serde_json::Value::String("barge.intent/3".to_owned());
        std::fs::write(
            &path,
            serde_json::to_vec_pretty(&value).expect("the record encodes"),
        )
        .expect("the record is written");
        let error = read_intent(&state, &intent_id).expect_err("an unknown major refuses");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
        assert!(
            error.to_string().contains("barge.intent/3"),
            "the refusal names the major: {error}"
        );
    }

    #[test]
    fn a_v2_record_with_a_duplicate_field_is_refused() {
        // A JSON object with two identical members is ambiguous: a reader that kept
        // the last one would erase the foreign-capture evidence the first member
        // recorded, or resolve a duplicated schema/unit to one shape. The strict
        // record refuses it instead.
        let (_dir, state, uuid) = fixture("intent-duplicate-v2");
        let intent_id = format!("{:032x}", 12);
        let path = super::intent_path(&state, &intent_id).expect("the path");
        let mut intent = record(&uuid);
        intent.foreign_captures = vec![leftover("payload.txt", 1)];
        let text =
            String::from_utf8(serde_json::to_vec_pretty(&intent).expect("the record encodes"))
                .expect("the record is text");
        assert!(
            text.contains("foreign_captures"),
            "the record carries the evidence"
        );
        let unit = serde_json::to_string(&intent.unit).expect("the unit encodes");
        let cases = [
            (
                "the foreign evidence and then an empty list",
                crate::testkit::append_json_member(&text, "\"foreign_captures\":[]"),
            ),
            (
                "the schema twice",
                crate::testkit::append_json_member(&text, "\"schema\":\"barge.intent/1\""),
            ),
            (
                "the unit twice",
                crate::testkit::append_json_member(&text, &format!("\"unit\":{unit}")),
            ),
        ];
        for (label, document) in cases {
            std::fs::write(&path, document).expect("the record is written");
            let error = read_intent(&state, &intent_id).expect_err(label);
            assert_eq!(error.kind(), ErrorKind::UnknownState, "{label}");
        }
    }

    #[test]
    fn a_v1_record_with_a_duplicate_field_is_refused() {
        let (_dir, state, uuid) = fixture("intent-duplicate-v1");
        let intent_id = format!("{:032x}", 13);
        let path = super::intent_path(&state, &intent_id).expect("the path");
        let value = v1_value(&uuid, &intent_id);
        let text =
            String::from_utf8(serde_json::to_vec_pretty(&value).expect("the record encodes"))
                .expect("the record is text");
        let cases = [
            (
                "the root id twice",
                crate::testkit::append_json_member(
                    &text,
                    "\"root_id\":\"0000000000000000000000000000000f\"",
                ),
            ),
            (
                "the marker digest twice",
                crate::testkit::append_json_member(
                    &text,
                    &format!("\"marker_digest\":\"{}\"", "b".repeat(64)),
                ),
            ),
        ];
        for (label, document) in cases {
            std::fs::write(&path, document).expect("the record is written");
            let error = read_intent(&state, &intent_id).expect_err(label);
            assert_eq!(error.kind(), ErrorKind::UnknownState, "{label}");
        }
    }

    fn leftover(original: &str, nonce: u8) -> ForeignCapture {
        ForeignCapture {
            private: format!("{CAPTURE_PREFIX}{}", format!("{nonce:02x}").repeat(16)),
            original: original.to_owned(),
            device: 16_777_219,
            inode: 4_242,
            kind: "file".to_owned(),
            captured: Some(CapturedObject {
                device: 16_777_219,
                inode: 4_243,
                kind: "file".to_owned(),
            }),
            put_back: PutBackOutcome::Pending,
            reason: "the captured object is not the observed one".to_owned(),
        }
    }

    #[test]
    fn an_unverified_capture_is_recorded_once_and_bounded() {
        let (_dir, state, uuid) = fixture("intent-leftovers");
        let intent = record(&uuid);
        let mut locks = LockSet::new(state.clone());
        let _volume = locks.lock_volume(&uuid).expect("the volume lock");
        let _root = locks.lock_root(&intent.unit.name).expect("the root lock");
        let write = locks.begin_intent_write().expect("the intent step");
        publish_intent(&write, &state, &intent).expect("the intent publishes");

        let first = record_foreign_capture(&write, &state, &intent, &leftover("payload.txt", 1))
            .expect("the leftover is recorded");
        assert_eq!(first.generation, intent.generation + 1);
        assert_eq!(
            first.phase,
            IntentPhase::Intent,
            "the phase does not change"
        );
        let read = read_intent(&state, &intent.intent_id).expect("the intent reads back");
        assert_eq!(read.foreign_captures.len(), 1);
        assert_eq!(read.foreign_captures[0].original, "payload.txt");

        // The same capture recorded again is its later evidence, not a duplicate,
        // and a different one is added.
        let mut answered = leftover("payload.txt", 1);
        answered.put_back = PutBackOutcome::Renamed;
        let again = record_foreign_capture(&write, &state, &read, &answered)
            .expect("the same capture is recorded again");
        assert_eq!(again.foreign_captures.len(), 1);
        assert_eq!(
            again.foreign_captures[0].put_back,
            PutBackOutcome::Renamed,
            "the later answer replaces the pending one"
        );
        let two = record_foreign_capture(&write, &state, &again, &leftover("second.txt", 2))
            .expect("a second leftover is recorded");
        assert_eq!(two.foreign_captures.len(), 2);

        // The record is bounded, and a record over the bound is refused rather
        // than quietly dropped.
        let mut held = two;
        for index in 0..INTENT_FOREIGN_CAPTURES {
            let next = leftover(&format!("entry-{index}.txt"), 3 + index as u8);
            if held.foreign_captures.len() == INTENT_FOREIGN_CAPTURES {
                let refused = record_foreign_capture(&write, &state, &held, &next)
                    .expect_err("a record over the bound is refused");
                assert_eq!(refused.kind(), ErrorKind::ResourceBound);
                break;
            }
            held = record_foreign_capture(&write, &state, &held, &next)
                .expect("the leftover is recorded");
        }
    }

    #[test]
    fn a_foreign_capture_that_does_not_validate_is_unknown_state() {
        let (_dir, state, uuid) = fixture("intent-leftover-invalid");
        let intent = record(&uuid);
        let base = super::intent_path(&state, &intent.intent_id).expect("the path");

        let mut cases: Vec<(&str, ForeignCapture)> = Vec::new();
        let mut not_private = leftover("payload.txt", 1);
        not_private.private = ".barge-prune-x".to_owned();
        cases.push(("a private name", not_private));
        let mut not_one_name = leftover("payload.txt", 2);
        not_one_name.original = "sub/name".to_owned();
        cases.push(("one entry name", not_one_name));
        let mut wrong_kind = leftover("payload.txt", 3);
        wrong_kind.kind = "device".to_owned();
        cases.push(("a kind", wrong_kind));
        let mut no_captured_kind = leftover("payload.txt", 5);
        no_captured_kind.captured = Some(CapturedObject {
            device: 16_777_219,
            inode: 4_243,
            kind: String::new(),
        });
        cases.push(("the captured object's kind", no_captured_kind));
        let mut long_reason = leftover("payload.txt", 4);
        long_reason.reason = "x".repeat(super::FOREIGN_REASON_BYTES + 1);
        cases.push(("a reason", long_reason));

        for (label, bad) in cases {
            let mut record = intent.clone();
            record.foreign_captures = vec![bad];
            let mut value = serde_json::to_value(&record).expect("the record encodes");
            value["schema"] = serde_json::Value::String(crate::contract::INTENT_SCHEMA.to_owned());
            std::fs::write(
                &base,
                serde_json::to_vec_pretty(&value).expect("the record encodes"),
            )
            .expect("the record is written");
            let error = read_intent(&state, &intent.intent_id).expect_err(label);
            assert_eq!(error.kind(), ErrorKind::UnknownState, "{label}");
        }
    }

    fn capture(original: &str, nonce: u8) -> super::CaptureRecord {
        super::CaptureRecord {
            private: format!("{CAPTURE_PREFIX}{}", format!("{nonce:02x}").repeat(16)),
            original: original.to_owned(),
            device: 16_777_219,
            inode: 4_242,
            kind: "directory".to_owned(),
            verdict: super::CaptureVerdict::Observed,
            mode: None,
        }
    }

    #[test]
    fn recorded_captures_and_the_removed_phase_read_back() {
        let (_dir, state, uuid) = fixture("intent-captures");
        let intent = record(&uuid);
        let mut locks = LockSet::new(state.clone());
        let _volume = locks.lock_volume(&uuid).expect("the volume lock");
        let _root = locks.lock_root(&intent.unit.name).expect("the root lock");
        let write = locks.begin_intent_write().expect("the intent step");
        publish_intent(&write, &state, &intent).expect("the intent publishes");

        let mut held = capture("sub", 1);
        held.mode = Some(0o040500);
        let recorded = super::record_captures(&write, &state, &intent, &[held.clone()])
            .expect("the capture is recorded");
        assert_eq!(recorded.captures.len(), 1);
        assert_eq!(recorded.captures[0].mode, Some(0o040500));
        let read = read_intent(&state, &intent.intent_id).expect("the intent reads back");
        assert_eq!(read.captures[0].private, held.private);
        assert_eq!(read.captures[0].verdict, super::CaptureVerdict::Observed);

        // The list is a snapshot: a later one replaces it rather than growing.
        let none = super::record_captures(&write, &state, &read, &[])
            .expect("the chain is published empty again");
        assert!(none.captures.is_empty());

        // The phases run intent, renamed, marker_last, removed, deleted.
        let removed = advance_intent(&write, &state, &read, IntentPhase::Renamed)
            .and_then(|next| advance_intent(&write, &state, &next, IntentPhase::MarkerLast))
            .and_then(|next| advance_intent(&write, &state, &next, IntentPhase::Removed))
            .expect("the removed phase is recorded");
        assert_eq!(removed.phase, IntentPhase::Removed);
        assert!(removed.is_pending());
        let settled = advance_intent(&write, &state, &removed, IntentPhase::Deleted)
            .expect("the intent settles");
        assert_eq!(settled.phase, IntentPhase::Deleted);
        assert!(!settled.is_pending());
    }

    #[test]
    fn a_capture_record_that_does_not_validate_is_unknown_state() {
        let (_dir, state, uuid) = fixture("intent-capture-invalid");
        let intent = record(&uuid);
        let base = super::intent_path(&state, &intent.intent_id).expect("the path");

        let mut cases: Vec<(&str, super::CaptureRecord)> = Vec::new();
        let mut not_private = capture("sub", 1);
        not_private.private = ".barge-prune-x".to_owned();
        cases.push(("a private name", not_private));
        let mut not_deletable = capture("sub", 2);
        not_deletable.kind = "symlink".to_owned();
        cases.push(("a deletable kind", not_deletable));
        let mut no_original = capture("sub", 3);
        no_original.original = String::new();
        cases.push(("one entry name", no_original));
        let mut wrong_mode = capture("sub", 4);
        wrong_mode.mode = Some(0o7777 + 1);
        cases.push(("a permission mode", wrong_mode));

        for (label, bad) in cases {
            let mut record = intent.clone();
            record.captures = vec![bad];
            let value = serde_json::to_value(&record).expect("the record encodes");
            std::fs::write(
                &base,
                serde_json::to_vec_pretty(&value).expect("the record encodes"),
            )
            .expect("the record is written");
            let error = read_intent(&state, &intent.intent_id).expect_err(label);
            assert_eq!(error.kind(), ErrorKind::UnknownState, "{label}");
        }
    }

    #[test]
    fn a_stray_journal_entry_is_unknown_state() {
        let (_dir, state, _uuid) = fixture("intent-stray");
        std::fs::write(state.intents_dir().join("not-an-intent.txt"), b"x")
            .expect("the stray file is written");
        let error = read_intents(&state, 64).expect_err("a stray name is unknown");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
    }

    #[test]
    fn an_expired_journal_deadline_is_never_empty() {
        let dir = crate::testkit::TempDir::under_scratch("intent-deadline");
        let account = crate::testkit::FakeAccount {
            home: dir.path().to_path_buf(),
        };
        let state = super::StatePaths::resolve(&account, Some(&dir.join("state"))).unwrap();
        let error = super::read_intents_with_deadline(
            &state,
            1,
            super::Deadline::after(std::time::Duration::ZERO),
        )
        .unwrap_err();
        assert_eq!(error.kind(), crate::error::ErrorKind::ResourceBound);
        assert!(!dir.join("state").exists());
    }

    #[test]
    fn an_absent_journal_is_empty_and_creates_nothing() {
        let dir = TempDir::new("intent-absent");
        let state = StatePaths::resolve(
            &FakeAccount {
                home: dir.path().to_path_buf(),
            },
            Some(&dir.join("state")),
        )
        .expect("the fixture state directory resolves");
        let set = read_intents(&state, 64).expect("an absent journal reads");
        assert_eq!(set.total, 0);
        assert!(!dir.join("state").exists(), "reading creates nothing");
    }
}
