//! Small helpers for tests: a private temporary directory that removes itself.
//!
//! The directory is created under `$TMPDIR` (falling back to the platform's
//! temporary directory), never in a home folder or at a drive root. Tests and
//! consumers' tests use it with [`crate::os::FakeOs`], so nothing touches a real
//! external drive.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A private temporary directory, removed when it is dropped.
#[derive(Debug)]
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    /// Create a fresh directory named `barge-<label>-<unique>` under `$TMPDIR`.
    pub fn new(label: &str) -> io::Result<Self> {
        let base = std::env::var_os("TMPDIR")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let unique = COUNTER.fetch_add(1, Ordering::SeqCst);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let path = base.join(format!(
            "barge-{label}-{}-{nanos}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    /// The directory itself.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A path inside the directory (not created).
    pub fn child(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }

    /// A path inside the directory, created as a folder.
    pub fn folder(&self, name: &str) -> io::Result<PathBuf> {
        let path = self.child(name);
        fs::create_dir_all(&path)?;
        Ok(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
