//! Hashes describe the outgoing checkout, and survive a return's reset.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{ErrorKind, Read};
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::cleanup::{self, covers};
use super::git;
use super::pools::Pool;
use crate::error::{Error, Result};
use crate::os::Tree;
use crate::state::publish;

const RECORD: &str = ".barge-warm-paths.json";
const SCHEMA: &str = "barge.worktree-warm-paths/1";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    schema: String,
    hashes: BTreeMap<String, String>,
}

#[derive(Default)]
pub(super) struct WarmReport {
    pub(super) kept: Vec<String>,
    pub(super) removed: Vec<String>,
}

/// Record the outgoing keys before any part of the reset can change them.
/// Retries of a reset must leave this snapshot alone.
pub(super) fn record_return(root: &Path, tree: &Path, pool: &Pool) -> Result<()> {
    if pool.warm_paths.is_empty()
        && !root
            .join(RECORD)
            .try_exists()
            .map_err(|source| Error::io("cannot inspect", &root.join(RECORD), &source))?
    {
        return Ok(());
    }
    let recorded = hashes(tree, pool)?;
    let present = real_warm_paths(tree, pool)?;
    let record = Record {
        schema: SCHEMA.to_string(),
        hashes: recorded
            .into_iter()
            .filter(|(path, _)| present.contains(path))
            .collect(),
    };
    write_record(root, &record)
}

/// Find the ignored entries covered by warm declarations, with normalized paths.
/// Whether the root carries a warm-path snapshot from a previous return.
/// Lease prefers such roots: their dependency folders are likeliest to still
/// match the current lockfiles. A missing file simply reads as cold.
pub(super) fn has_record(root: &Path) -> Result<bool> {
    Ok(!read_record(root)?.hashes.is_empty())
}

pub(super) fn candidates(tree: &Path, pool: &Pool) -> Result<BTreeSet<String>> {
    if pool.warm_paths.is_empty() {
        return Ok(BTreeSet::new());
    }
    Ok(git::ignored_entries(tree)?
        .into_iter()
        .map(|entry| entry.trim_end_matches('/').to_string())
        .filter(|entry| pool.warm_paths.iter().any(|warm| covers(&warm.path, entry)))
        .collect())
}

pub(super) fn validate(
    root: &Path,
    tree: &Path,
    pool: &Pool,
    previous: BTreeSet<String>,
) -> Result<WarmReport> {
    let mut report = WarmReport::default();
    if pool.warm_paths.is_empty() {
        return Ok(report);
    }

    let current = candidates(tree, pool)?;
    let mut record = read_record(root)?;
    let hashes = hashes(tree, pool)?;
    let count = record.hashes.len();
    record
        .hashes
        .retain(|path, hash| hashes.get(path) == Some(hash));

    for entry in &current {
        let valid = pool
            .warm_paths
            .iter()
            .filter(|warm| covers(&warm.path, entry))
            .all(|warm| record.hashes.contains_key(&warm.path));
        if valid {
            report.kept.push(entry.clone());
        } else {
            report.removed.push(entry.clone());
        }
    }

    // The checkout may have changed the ignore rules. Old warm entries that
    // are now unignored still need cleanup, but must never remove tracked files.
    if previous.difference(&current).next().is_some() {
        let tracked = git::tracked_entries(tree)?;
        for entry in previous.difference(&current) {
            if !tracked_overlap(&tracked, entry)
                && !current
                    .iter()
                    .any(|ignored| contains(ignored, entry) || contains(entry, ignored))
            {
                report.removed.push(entry.clone());
            }
        }
    }

    // Invalidate the record first so an interrupted deletion cannot leave a
    // partial folder that a retry at the old base would trust.
    record
        .hashes
        .retain(|pattern, _| !report.removed.iter().any(|entry| covers(pattern, entry)));
    if record.hashes.len() != count {
        write_record(root, &record)?;
    }
    if !report.removed.is_empty() {
        report.removed.sort_unstable();
        for entry in &report.removed {
            cleanup::remove_entry(tree, entry)?;
        }
    }
    Ok(report)
}

fn contains(parent: &str, path: &str) -> bool {
    path == parent
        || path
            .strip_prefix(parent)
            .is_some_and(|tail| tail.starts_with('/'))
}

fn tracked_overlap(tracked: &BTreeSet<String>, entry: &str) -> bool {
    let prefix = format!("{entry}/");
    tracked.contains(entry)
        || tracked
            .range(prefix.clone()..)
            .next()
            .is_some_and(|path| path.starts_with(&prefix))
        || entry
            .match_indices('/')
            .any(|(index, _)| tracked.contains(&entry[..index]))
}

fn hashes(tree: &Path, pool: &Pool) -> Result<BTreeMap<String, String>> {
    let checkout = Tree::open(tree)?;
    let mut hashes = BTreeMap::<_, Sha256>::new();
    let mut buffer = [0; 8192];
    for warm in &pool.warm_paths {
        let hash = hashes.entry(warm.path.clone()).or_default();
        for key in &warm.keys {
            // Include the key name, presence and a fixed-length content digest.
            // This distinguishes missing from empty files and frames each file.
            hash.update(key.as_bytes());
            hash.update([0]);
            match checkout.open_file(Path::new(key))? {
                None => hash.update([0]),
                Some(mut file) => {
                    let mut contents = Sha256::new();
                    loop {
                        match file.read(&mut buffer) {
                            Ok(0) => break,
                            Ok(size) => contents.update(&buffer[..size]),
                            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                            Err(error) => {
                                return Err(Error::io("cannot hash", &tree.join(key), &error))
                            }
                        }
                    }
                    hash.update([1]);
                    hash.update(contents.finalize());
                }
            }
        }
    }
    Ok(hashes
        .into_iter()
        .map(|(path, hash)| {
            let hex = hash
                .finalize()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            (path, hex)
        })
        .collect())
}

/// Return declared warm paths that exist as real directories, without following
/// symlinks.
fn real_warm_paths(tree: &Path, pool: &Pool) -> Result<BTreeSet<String>> {
    let mut present = BTreeSet::new();

    for warm in &pool.warm_paths {
        let mut paths = vec![Path::new("").to_path_buf()];
        for segment in warm.path.split('/') {
            let mut next = Vec::new();
            for path in paths {
                if segment.contains('*') {
                    let directory = tree.join(&path);
                    let entries = match std::fs::read_dir(&directory) {
                        Ok(entries) => entries,
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                        Err(source) => {
                            return Err(Error::io("cannot inspect warm path", &directory, &source))
                        }
                    };
                    for entry in entries {
                        let entry = entry.map_err(|source| {
                            Error::io("cannot inspect warm path", &directory, &source)
                        })?;
                        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                            continue;
                        };
                        if cleanup::segment_matches(segment, &name) {
                            next.push(path.join(name));
                        }
                    }
                } else {
                    next.push(path.join(segment));
                }
            }

            paths = Vec::new();
            for path in next {
                let full_path = tree.join(&path);
                let metadata = match std::fs::symlink_metadata(&full_path) {
                    Ok(metadata) => metadata,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(source) => {
                        return Err(Error::io("cannot inspect warm path", &full_path, &source))
                    }
                };
                if metadata.file_type().is_dir() {
                    paths.push(path);
                }
            }
            if paths.is_empty() {
                break;
            }
        }
        if !paths.is_empty() {
            present.insert(warm.path.clone());
        }
    }

    Ok(present)
}

fn write_record(root: &Path, record: &Record) -> Result<()> {
    let bytes = serde_json::to_vec(record).map_err(|source| {
        Error::unknown_state(format!("cannot encode warm-path hashes: {source}"))
    })?;
    publish(&root.join(RECORD), &bytes)
}

fn read_record(root: &Path) -> Result<Record> {
    let Some(file) = Tree::open(root)?.open_file(Path::new(RECORD))? else {
        return Ok(Record {
            schema: SCHEMA.to_string(),
            hashes: BTreeMap::new(),
        });
    };
    let record: Record = serde_json::from_reader(file).map_err(|source| {
        Error::unknown_state(format!(
            "cannot read {}: {source}",
            root.join(RECORD).display()
        ))
    })?;
    if record.schema != SCHEMA {
        return Err(Error::unknown_state(format!(
            "{} has schema {}, expected {SCHEMA}",
            root.join(RECORD).display(),
            record.schema
        )));
    }
    Ok(record)
}
