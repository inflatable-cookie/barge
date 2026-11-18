//! Durable root pins.
#[cfg(test)]
use crate::area::PIN_HOOK;
use crate::area::{parent, unchanged_marker, Area, AreaLock, BARGE_DIR, PINS_DIR};
use crate::error::{Error, Result};
use crate::root;
use crate::state::publish;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
/// The schema name of a pin record.
pub const PIN_SCHEMA: &str = "barge.pin/1";
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PinRecord {
    schema: String,
    root_id: String,
    reason: String,
    pinned_unix_ms: u64,
}

pub(crate) fn pin_path(directory: &Path, id: &str) -> PathBuf {
    directory
        .join(BARGE_DIR)
        .join(PINS_DIR)
        .join(format!("{id}.json"))
}

/// Whether a pin protects a root. A pin path that exists but cannot be
/// inspected counts as pinned: unknown is never safe.
pub(crate) fn pin_state(directory: &Path, id: &str) -> Result<bool> {
    let path = pin_path(directory, id);
    match fs::symlink_metadata(&path) {
        Ok(_) => Ok(true),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(Error::io("cannot inspect the pin at", &path, &source)),
    }
}

impl Area {
    /// Protect a root from prune and eviction until it is unpinned.
    pub fn pin(&self, id: &str, reason: &str) -> Result<()> {
        self.core.ensure_writable()?;
        let root = self.resolve(id)?;
        #[cfg(test)]
        PIN_HOOK.with(|hook| {
            let callback = hook.borrow_mut().take();
            if let Some(hook) = callback {
                hook();
            }
        });
        let directory = parent(&root.path)?;
        self.ensure_layout(&directory)?;
        let _lock = AreaLock::acquire(&directory)?;
        // Eviction renames under this same lock, so the root resolved a moment
        // ago can already be trash: pin only the root this path still is.
        if !unchanged_marker(&root, &self.key().label()) {
            return Err(Error::refused(format!(
                "root {id} in area {} changed before it could be pinned; nothing was pinned",
                self.key().label()
            )));
        }
        let record = PinRecord {
            schema: PIN_SCHEMA.to_string(),
            root_id: id.to_string(),
            reason: reason.to_string(),
            pinned_unix_ms: self.core.clock.now_unix_ms(),
        };
        let bytes = serde_json::to_vec_pretty(&record)
            .map_err(|source| Error::unknown_state(format!("cannot encode a pin: {source}")))?;
        publish(&pin_path(&directory, id), &bytes)
    }

    /// Remove a root's pin.
    pub fn unpin(&self, id: &str) -> Result<()> {
        self.core.ensure_writable()?;
        root::validate_id(id)?;
        let mut removed = false;
        for (directory, _) in self.search_dirs()? {
            if !directory.is_dir() {
                continue;
            }
            self.ensure_layout(&directory)?;
            let _lock = AreaLock::acquire(&directory)?;
            match fs::remove_file(pin_path(&directory, id)) {
                Ok(()) => removed = true,
                Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
                Err(source) => {
                    return Err(Error::io(
                        "cannot remove the pin at",
                        &pin_path(&directory, id),
                        &source,
                    ))
                }
            }
        }
        if removed {
            Ok(())
        } else {
            Err(Error::not_found(format!(
                "root {id} in area {} is not pinned",
                self.key().label()
            )))
        }
    }
}
