//! Persistent log storage for remote runs inside a Barge area, with
//! SHA-256 digests and bounded time-based retention pruning.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use barge::run::LogRef;
use barge_machine::{auth, base64, RunLogResponse};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{Error, Result};

const META_SUFFIX: &str = ".meta.json";
const LOG_SUFFIX: &str = ".log";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LogMeta {
    request_id: String,
    digest: String,
    location: String,
    created_unix_ms: u64,
}

/// Manages full run logs in one Barge-owned directory with bounded retention.
#[derive(Debug, Clone)]
pub(crate) struct LogStore {
    dir: PathBuf,
    retention_ms: u64,
}

impl LogStore {
    pub(crate) fn new(dir: PathBuf, retention_s: u64) -> Self {
        Self {
            dir,
            retention_ms: retention_s.saturating_mul(1000),
        }
    }

    /// Location string placed in [`LogRef::location`] for `request_id`.
    pub(crate) fn location_for(request_id: &str) -> String {
        format!("/v1/runs/logs/{request_id}")
    }

    /// Open a streaming writer for `request_id`'s raw log bytes.
    pub(crate) fn create_writer(&self, request_id: &str) -> Result<LogWriter> {
        fs::create_dir_all(&self.dir).map_err(|error| {
            Error::new(format!(
                "cannot create run log directory {}: {error}",
                self.dir.display()
            ))
        })?;
        let path = self.log_path(request_id);
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&path)
            .map_err(|error| {
                Error::new(format!("cannot create log {}: {error}", path.display()))
            })?;
        Ok(LogWriter {
            store: self.clone(),
            request_id: request_id.to_string(),
            file,
            hasher: Sha256::new(),
        })
    }

    /// Fetch a retained log by `request_id`, pruning expired logs first.
    pub(crate) fn fetch(
        &self,
        request_id: &str,
        now_unix_ms: u64,
    ) -> Result<Option<RunLogResponse>> {
        self.prune_expired(now_unix_ms);
        let meta_path = self.meta_path(request_id);
        let log_path = self.log_path(request_id);
        let meta_bytes = match fs::read(&meta_path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(Error::new(format!(
                    "cannot read log metadata {}: {error}",
                    meta_path.display()
                )))
            }
        };
        let meta: LogMeta = serde_json::from_slice(&meta_bytes).map_err(|error| {
            Error::new(format!(
                "cannot parse log metadata {}: {error}",
                meta_path.display()
            ))
        })?;
        let bytes = match fs::read(&log_path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(Error::new(format!(
                    "cannot read log {}: {error}",
                    log_path.display()
                )))
            }
        };
        Ok(Some(RunLogResponse::new(
            LogRef {
                digest: meta.digest,
                location: meta.location,
            },
            base64::encode(&bytes),
        )))
    }

    /// Remove logs older than `retention_ms` relative to `now_unix_ms`.
    pub(crate) fn prune_expired(&self, now_unix_ms: u64) {
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let Some(request_id) = name.strip_suffix(META_SUFFIX) else {
                continue;
            };
            let Ok(bytes) = fs::read(&path) else {
                continue;
            };
            let Ok(meta) = serde_json::from_slice::<LogMeta>(&bytes) else {
                continue;
            };
            if now_unix_ms.saturating_sub(meta.created_unix_ms) >= self.retention_ms {
                let _ = fs::remove_file(&path);
                let _ = fs::remove_file(self.log_path(request_id));
            }
        }
    }

    fn log_path(&self, request_id: &str) -> PathBuf {
        self.dir.join(format!("{request_id}{LOG_SUFFIX}"))
    }

    fn meta_path(&self, request_id: &str) -> PathBuf {
        self.dir.join(format!("{request_id}{META_SUFFIX}"))
    }
}

/// Appends raw command output bytes and computes the final SHA-256 [`LogRef`].
#[derive(Debug)]
pub(crate) struct LogWriter {
    store: LogStore,
    request_id: String,
    file: File,
    hasher: Sha256,
}

impl LogWriter {
    pub(crate) fn append(&mut self, bytes: &[u8]) {
        self.hasher.update(bytes);
        let _ = self.file.write_all(bytes);
    }

    pub(crate) fn finish(mut self, finished_unix_ms: u64) -> LogRef {
        let _ = self.file.flush();
        let digest = format!("sha256:{}", auth::hex(&self.hasher.finalize()));
        let location = LogStore::location_for(&self.request_id);
        let meta = LogMeta {
            request_id: self.request_id.clone(),
            digest: digest.clone(),
            location: location.clone(),
            created_unix_ms: finished_unix_ms,
        };
        if let Ok(encoded) = serde_json::to_vec(&meta) {
            let meta_path = self.store.meta_path(&self.request_id);
            let _ = write_atomic(&meta_path, &encoded);
        }
        self.store.prune_expired(finished_unix_ms);
        LogRef { digest, location }
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)
}
