//! Test-only fixture helpers: one fresh temporary directory per test.
//!
//! Every fixture lives inside a directory this process creates and removes, and
//! never at a fixed path, so two tests — or two tasks on one machine — cannot
//! share a name. Tests that need the state directory use
//! [`crate::state::StatePaths`] over a directory here.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// A temporary directory that removes itself when the test ends.
#[derive(Debug)]
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    /// Create a fresh directory named after `label` and this process.
    pub fn new(label: &str) -> Self {
        let base = std::env::temp_dir();
        for _ in 0..32 {
            let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.subsec_nanos());
            let path = base.join(format!(
                "barge-test-{label}-{}-{sequence}-{nanos}",
                std::process::id()
            ));
            match std::fs::create_dir(&path) {
                Ok(()) => return Self { path },
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("cannot create the fixture directory: {error}"),
            }
        }
        panic!(
            "could not find a fresh fixture directory name under {}",
            base.display()
        );
    }

    /// The directory's path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A path inside the directory. Nothing is created.
    pub fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }

    /// Write one fixture file, creating its parent directories.
    pub fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.path.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("the fixture parent is created");
        }
        std::fs::write(&path, bytes).expect("the fixture file is written");
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
