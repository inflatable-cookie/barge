//! Account locking and atomic publication.
use crate::{Error, ErrorKind, Result};
use std::{
    fs::{self, File},
    io::{self, Write},
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};
static PUBLISH_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Write `bytes` to `path` through a temporary file in the same directory and an
/// atomic rename, so a reader sees the old document or the new one, never half.
///
/// This is the same publication step as `barge::state::publish`, kept here so
/// the storage library's dependency tree does not grow (docs/knowledge/machines.md,
/// "Registry state").
pub(crate) fn publish(path: &Path, bytes: &[u8]) -> Result<()> {
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("registry");
    let temporary = path.with_file_name(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        PUBLISH_COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    let mut file = File::create(&temporary)
        .map_err(|e| io_error("cannot create registry temporary file", &temporary, &e))?;
    if let Err(e) = file.write_all(bytes).and_then(|()| file.sync_all()) {
        let _ = fs::remove_file(&temporary);
        return Err(io_error("cannot write registry", &temporary, &e));
    }
    if let Err(e) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(io_error("cannot publish registry", path, &e));
    }
    // Best effort: make the rename durable without making it a failure.
    if let Ok(directory) = File::open(directory) {
        let _ = directory.sync_all();
    }
    Ok(())
}

pub(crate) fn io_error(action: &str, path: &Path, source: &io::Error) -> Error {
    Error::new(
        ErrorKind::Io,
        format!("{action} {}: {source}", path.display()),
    )
}

/// The held account state lock.
///
/// The `lock` file is the one `barge::state` also takes, so registry writes and
/// storage writes exclude each other. Closing the descriptor releases the lock,
/// which is why there is no explicit unlock and no `Drop` body.
#[derive(Debug)]
pub(crate) struct StateLock {
    /// Held only for the lock its descriptor carries; never read.
    _file: File,
}

impl StateLock {
    pub(crate) fn acquire(root: &Path) -> Result<Self> {
        fs::create_dir_all(root)
            .map_err(|e| io_error("cannot create state directory", root, &e))?;
        let path = root.join("lock");
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| io_error("cannot open account state lock", &path, &e))?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            // SAFETY: flock operates on the descriptor owned by this guard, and
            // the lock is released when that descriptor is closed.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == -1 {
                return Err(io_error(
                    "cannot acquire account state lock",
                    &path,
                    &io::Error::last_os_error(),
                ));
            }
        }
        Ok(Self { _file: file })
    }
}
