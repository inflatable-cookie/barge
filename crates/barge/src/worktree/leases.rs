//! Lease records: one durable file per lease, the request they are bound to, and
//! the checks a request must pass before Barge acts on it.
//!
//! A lease is the record, not a process: it reserves its root between runs until
//! it is returned. The record is published before a lease call replies, so a
//! caller that lost the reply reads the lease back by request or by lease ID.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::git;
use crate::error::{Error, Result};
use crate::root;
use crate::state::publish;

/// The schema name of a lease record and of the lease documents built from it.
pub const LEASE_SCHEMA: &str = "barge.worktree-lease/1";
/// The folder of lease records inside the account state directory.
pub(crate) const LEASES_DIR: &str = "worktree-leases";

const MAX_REQUEST: usize = 64;
const MAX_LABEL: usize = 128;

/// Where a lease is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorktreeLeaseState {
    /// Reserved for one request and in use or waiting for its return.
    Leased,
    /// A return passed its preflight and is resetting the root.
    Returning,
    /// Back in the pool: the root is free.
    Returned,
    /// A return was cut off part-way: the root stays out of the pool until the
    /// next return finishes it.
    Interrupted,
}

impl WorktreeLeaseState {
    /// The stable name used in documents and by people.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Leased => "leased",
            Self::Returning => "returning",
            Self::Returned => "returned",
            Self::Interrupted => "interrupted",
        }
    }

    /// Whether the lease still reserves its root.
    pub fn reserves_root(self) -> bool {
        !matches!(self, Self::Returned)
    }
}

/// What a caller asks for: a checkout of `base` on a new `branch`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseRequest {
    /// The origin URL; it is normalised to the pool's identity.
    pub repo: String,
    /// The exact commit the branch starts at.
    pub base: String,
    /// The branch the base came from, which a return resets to when it is
    /// known in the pool's clone.
    pub base_name: Option<String>,
    /// The new branch's name.
    pub branch: String,
    /// The allocation's identity; repeating it with the same payload returns the
    /// same lease.
    pub request: String,
    /// Metadata only: the task a lease serves. Never part of the binding.
    pub label: Option<String>,
}

impl LeaseRequest {
    /// Check every field before any state is read or changed.
    pub(crate) fn validate(&self) -> Result<()> {
        validate_request_id(&self.request)?;
        validate_base(&self.base)?;
        validate_branch("branch", &self.branch)?;
        if let Some(base_name) = &self.base_name {
            validate_branch("base name", base_name)?;
        }
        if let Some(label) = &self.label {
            let usable = label.len() <= MAX_LABEL && !label.chars().any(char::is_control);
            if !usable {
                return Err(Error::usage(format!(
                    "label must be at most {MAX_LABEL} characters and print as text"
                )));
            }
        }
        Ok(())
    }

    /// Whether a stored lease was made for this payload. The label is metadata,
    /// so it is not compared.
    pub(crate) fn same_payload(&self, lease: &WorktreeLease, repo: &str) -> bool {
        lease.repo == repo && lease.base == self.base && lease.branch == self.branch
    }
}

/// One lease, as stored and as reported. Its fields are the contract's
/// `worktree-lease` shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorktreeLease {
    pub schema: String,
    pub lease_id: String,
    pub request: String,
    /// The canonical repository identity.
    pub repo: String,
    pub base: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_name: Option<String>,
    pub branch: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// The pool's area name.
    pub pool: String,
    pub root_id: String,
    /// The canonical path of the checkout.
    pub path: String,
    /// The commit the checkout was verified at when it was leased or returned.
    pub head: String,
    pub state: WorktreeLeaseState,
    /// Whether the return that began this lease's reset accepted commits beyond
    /// base as published, merged or abandoned.
    pub disposable: bool,
    pub created_unix_ms: u64,
    pub updated_unix_ms: u64,
}

/// The path of one lease's record.
pub(crate) fn lease_path(state: &Path, lease_id: &str) -> PathBuf {
    state.join(LEASES_DIR).join(format!("{lease_id}.json"))
}

/// Read one lease by ID. `None` means no lease has that ID.
pub(crate) fn read_lease(state: &Path, lease_id: &str) -> Result<Option<WorktreeLease>> {
    root::validate_id(lease_id)?;
    let path = lease_path(state, lease_id);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(Error::io("cannot read", &path, &source)),
    };
    let lease = parse_lease(&path, &bytes)?;
    if lease.lease_id != lease_id {
        return Err(Error::unknown_state(format!(
            "{} records lease {}, not {lease_id}",
            path.display(),
            lease.lease_id
        )));
    }
    Ok(Some(lease))
}

/// Every lease record. A record that does not parse is `unknown_state`: Barge
/// cannot tell which root it reserves, so nothing is leased until it is fixed.
pub(crate) fn all_leases(state: &Path) -> Result<Vec<WorktreeLease>> {
    let dir = state.join(LEASES_DIR);
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(Error::io("cannot list", &dir, &source)),
    };
    let mut leases = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| Error::io("cannot list", &dir, &source))?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let bytes = fs::read(&path).map_err(|source| Error::io("cannot read", &path, &source))?;
        leases.push(parse_lease(&path, &bytes)?);
    }
    Ok(leases)
}

fn parse_lease(path: &Path, bytes: &[u8]) -> Result<WorktreeLease> {
    let lease: WorktreeLease = serde_json::from_slice(bytes).map_err(|source| {
        Error::unknown_state(format!("cannot parse {}: {source}", path.display()))
    })?;
    if lease.schema != LEASE_SCHEMA {
        return Err(Error::unknown_state(format!(
            "{} has schema {}, not {LEASE_SCHEMA}",
            path.display(),
            lease.schema
        )));
    }
    root::validate_id(&lease.lease_id)
        .and_then(|()| root::validate_id(&lease.root_id))
        .map_err(|error| Error::unknown_state(format!("invalid {}: {error}", path.display())))?;
    Ok(lease)
}

/// Publish one lease record by atomic rename.
pub(crate) fn write_lease(state: &Path, lease: &WorktreeLease) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(lease)
        .map_err(|source| Error::unknown_state(format!("cannot encode a lease: {source}")))?;
    publish(&lease_path(state, &lease.lease_id), &bytes)
}

/// The lease a request ID is bound to, if any.
pub(crate) fn find_by_request<'a>(
    leases: &'a [WorktreeLease],
    request: &str,
) -> Option<&'a WorktreeLease> {
    leases.iter().find(|lease| lease.request == request)
}

/// A request ID: 1 to 64 letters, digits, `-`, `_` or `.`, not starting with a
/// dot or dash, so it is always safe as a file name and a flag value.
fn validate_request_id(request: &str) -> Result<()> {
    let usable = !request.is_empty()
        && request.len() <= MAX_REQUEST
        && !request.starts_with(['.', '-'])
        && request
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
    if usable {
        Ok(())
    } else {
        Err(Error::usage(format!(
            "request {request:?} must be 1-{MAX_REQUEST} letters, digits, '-', '_' or '.'"
        )))
    }
}

/// A base is an exact SHA-1 commit: 40 lower-case hexadecimal digits.
fn validate_base(base: &str) -> Result<()> {
    let usable = base.len() == 40
        && base
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    if usable {
        Ok(())
    } else {
        Err(Error::usage(format!(
            "base {base:?} must be an exact 40-character lower-case commit SHA"
        )))
    }
}

/// A branch name git accepts, and never one that could read as a flag.
fn validate_branch(field: &str, name: &str) -> Result<()> {
    if name.starts_with('-') || !git::valid_branch_name(name)? {
        return Err(Error::usage(format!(
            "{field} {name:?} is not a valid git branch name that avoids a leading '-'"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_ids_bases_and_branches_are_checked_before_use() {
        assert!(validate_request_id("nucleus-run.1_a").is_ok());
        for bad in ["", "-x", ".hidden", "a/b", "a b"] {
            assert!(validate_request_id(bad).is_err(), "{bad:?}");
        }
        assert!(validate_base(&"a".repeat(40)).is_ok());
        assert!(validate_base("ABCDEF").is_err());
        assert!(validate_branch("branch", "task/one").is_ok());
        assert!(validate_branch("branch", "-delete").is_err());
        assert!(validate_branch("branch", "two..dots").is_err());
    }
}
