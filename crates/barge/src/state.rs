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

use std::fs::{File, OpenOptions};
use std::io::{ErrorKind as IoErrorKind, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::{Error, ErrorKind, Result};
use crate::os::{AccountFacts, VolumeUuid};

/// The documented fixture-only override of the state directory. A normal run
/// leaves it unset, so a stray environment cannot move the account's authority.
pub const STATE_DIR_ENV: &str = "BARGE_STATE_DIR";

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
    /// as it is, apart from being checked for being a real directory.
    pub fn initialize(&self) -> Result<()> {
        ensure_directory(&self.root)?;
        for directory in [
            self.areas_dir(),
            self.placements_dir(),
            self.holds_dir(),
            self.intents_dir(),
            self.journal_dir(),
            self.locks_dir(),
        ] {
            ensure_directory(&directory)?;
        }
        Ok(())
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

/// Validate one identifier used in a file name: the design's 128-bit ids are
/// lowercase hexadecimal, so anything else is refused rather than encoded.
pub fn validate_identifier(field: &str, value: &str) -> Result<()> {
    let well_formed =
        value.len() == 32 && value.chars().all(|character| character.is_ascii_hexdigit());
    if well_formed {
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
        return Err(bound_error(
            path,
            "the read deadline had already expired before the file was opened".to_owned(),
        ));
    }
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == IoErrorKind::NotFound => return Ok(ReadOutcome::Absent),
        Err(error) => {
            return Err(Error::new(
                ErrorKind::Io,
                format!("cannot read {}: {error}", path.display()),
            )
            .with_detail("path", path.display().to_string().into())
            .with_detail("os_error", error.to_string().into()));
        }
    };
    if metadata.file_type().is_symlink() {
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
        return Err(Error::new(
            ErrorKind::UnknownState,
            format!("{} is not a regular file", path.display()),
        )
        .with_detail("path", path.display().to_string().into()));
    }
    if metadata.len() > bounds.max_bytes as u64 {
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
        Error::new(
            ErrorKind::Io,
            format!("cannot read the size of {}: {error}", path.display()),
        )
        .with_detail("path", path.display().to_string().into())
    })?;
    if !opened.is_file() {
        return Err(Error::new(
            ErrorKind::UnknownState,
            format!(
                "{} changed into a non-file while it was opened",
                path.display()
            ),
        )
        .with_detail("path", path.display().to_string().into()));
    }
    if opened.len() > bounds.max_bytes as u64 {
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
            return Err(bound_error(
                path,
                "the read deadline expired before the file was read".to_owned(),
            ));
        }
        let read = file.read(&mut chunk).map_err(|error| {
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

        let written = write_and_publish(&mut file, &temporary, target, bytes, &mut before_rename);
        if written.is_err() {
            // Best effort: a leftover temporary file is never read as the target,
            // and reporting the original failure matters more than this cleanup.
            let _ = std::fs::remove_file(&temporary);
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
    file.write_all(bytes).map_err(|error| {
        Error::new(
            ErrorKind::Io,
            format!("cannot write {}: {error}", temporary.display()),
        )
        .with_detail("path", temporary.display().to_string().into())
        .with_detail("os_error", error.to_string().into())
    })?;
    file.sync_all().map_err(|error| {
        Error::new(
            ErrorKind::Io,
            format!("cannot sync {}: {error}", temporary.display()),
        )
        .with_detail("path", temporary.display().to_string().into())
    })?;
    if let Some(seam) = before_rename.take() {
        seam(temporary)?;
    }
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
pub fn now_ms() -> u64 {
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
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(FILE_MODE)
            .open(path)
            .map_err(|error| lock_open_error(path, &error))?;
        wait_for_lock(&file, path, LockMode::Exclusive, deadline)?;
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
            Err(error) if error.kind() == IoErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(lock_open_error(path, &error)),
        };
        wait_for_lock(&file, path, LockMode::Shared, deadline)?;
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

    /// Observe the registry: a shared lock, and only when the lock file exists.
    /// A read-only command creates no lock, no file and no directory.
    pub fn observe_registry(&mut self) -> Result<Option<StateLock>> {
        let path = self.state.registry_lock_path();
        self.acquire_shared(LockRank::Registry, &path)
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
