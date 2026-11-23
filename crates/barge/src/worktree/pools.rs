//! Pool declarations and the repository identity a pool is named by.
//!
//! The account keeps one file, `worktree-pools.json`, beside `layout.json`. It is
//! read here and never written by Barge: an operator declares a pool by writing
//! it, and a stored file that does not validate is `unknown_state`, never an empty
//! list.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::state::{validate_name, validate_relative_path};

/// The schema name of the pool declaration file.
pub const POOLS_SCHEMA: &str = "barge.worktree-pools/1";
/// The declaration file inside the account state directory.
pub(crate) const POOLS_FILE: &str = "worktree-pools.json";
/// The area namespace every pool's area lives under.
pub(crate) const NAMESPACE: &str = "worktrees";

const DEFAULT_WARM: u32 = 2;
const DEFAULT_MAX: u32 = 6;
const MAX_SLUG: usize = 64;

/// A folder of ignored dependencies and the key files that determine its validity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WarmPath {
    pub(crate) path: String,
    #[serde(default)]
    pub(crate) keys: Vec<String>,
}

/// One repository's pool, as declared in the account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Pool {
    /// The canonical repository identity: the normalised origin URL.
    pub(crate) repo: String,
    /// The URL the pool's bare clone fetches from.
    pub(crate) origin: String,
    #[serde(default = "default_warm")]
    pub(crate) warm: u32,
    #[serde(default = "default_max")]
    pub(crate) max: u32,
    #[serde(default)]
    pub(crate) warm_paths: Vec<WarmPath>,
    /// Ignored paths a return removes, such as `dist` or `.cache`.
    #[serde(default)]
    pub(crate) disposable: Vec<String>,
}

fn default_warm() -> u32 {
    DEFAULT_WARM
}

fn default_max() -> u32 {
    DEFAULT_MAX
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PoolsDocument {
    schema: String,
    pools: Vec<Pool>,
}

impl Pool {
    /// The pool's area name: a slug of its repository identity.
    pub(crate) fn slug(&self) -> Result<String> {
        pool_slug(&self.repo)
    }

    fn validate(&self) -> Result<()> {
        if canonical_repo(&self.repo)? != self.repo {
            return Err(Error::usage(format!(
                "pool repo {:?} is not in canonical form",
                self.repo
            )));
        }
        canonical_repo(&self.origin)?;
        if self.max == 0 || self.warm > self.max {
            return Err(Error::usage(format!(
                "pool {} needs 1 <= max and warm <= max",
                self.repo
            )));
        }
        for path in self.warm_paths.iter().map(|warm| warm.path.as_str()) {
            validate_declared_path("warm path", path)?;
        }
        for warm in &self.warm_paths {
            for key in &warm.keys {
                validate_declared_path("warm path key", key)?;
            }
        }
        for path in &self.disposable {
            validate_declared_path("disposable path", path)?;
        }
        Ok(())
    }
}

/// A declared path is a non-empty relative folder path; `validate_relative_path`
/// refuses absolute paths, `..` and empty components.
fn validate_declared_path(field: &str, path: &str) -> Result<()> {
    if path.is_empty() {
        return Err(Error::usage(format!("{field} must not be empty")));
    }
    validate_relative_path(field, path)
}

/// Read every declared pool. A missing file declares none.
pub(crate) fn read_pools(state: &Path) -> Result<Vec<Pool>> {
    let path = state.join(POOLS_FILE);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(Error::io("cannot read", &path, &source)),
    };
    let document: PoolsDocument = serde_json::from_slice(&bytes).map_err(|source| {
        Error::unknown_state(format!("cannot parse {}: {source}", path.display()))
    })?;
    if document.schema != POOLS_SCHEMA {
        return Err(Error::unknown_state(format!(
            "{} has schema {}, not {POOLS_SCHEMA}",
            path.display(),
            document.schema
        )));
    }
    let mut slugs = BTreeSet::new();
    for pool in &document.pools {
        pool.validate().map_err(|error| {
            Error::unknown_state(format!("invalid {}: {error}", path.display()))
        })?;
        let slug = pool.slug()?;
        if !slugs.insert(slug.clone()) {
            return Err(Error::unknown_state(format!(
                "{} declares two pools named {slug}",
                path.display()
            )));
        }
    }
    Ok(document.pools)
}

/// The declared pool for a canonical repository identity.
pub(crate) fn find_pool<'a>(pools: &'a [Pool], repo: &str) -> Result<&'a Pool> {
    pools.iter().find(|pool| pool.repo == repo).ok_or_else(|| {
        Error::not_found(format!(
            "no worktree pool is declared for {repo}; declare it in {POOLS_FILE}"
        ))
    })
}

/// The requested pool, or all pools when no repository is specified.
pub(super) fn select_pools<'a>(pools: &'a [Pool], repo: Option<&str>) -> Result<Vec<&'a Pool>> {
    match repo {
        Some(repo) => Ok(vec![find_pool(pools, &canonical_repo(repo)?)?]),
        None => Ok(pools.iter().collect()),
    }
}

/// The canonical identity of a repository: scheme, user and `.git` removed,
/// scp-style `host:path` written as `host/path`, lower case.
///
/// `https://github.com/Owner/repo.git`, `git@github.com:Owner/repo` and
/// `ssh://git@github.com/owner/repo/` all name `github.com/owner/repo`.
pub(crate) fn canonical_repo(origin: &str) -> Result<String> {
    let trimmed = origin.trim();
    let usable = !trimmed.is_empty()
        && !trimmed.starts_with('-')
        && !trimmed
            .chars()
            .any(|ch| ch.is_whitespace() || ch.is_control());
    if !usable {
        return Err(Error::usage(format!(
            "repository {origin:?} is not a usable origin URL"
        )));
    }
    let body = match trimmed.split_once("://") {
        Some((_scheme, rest)) => rest,
        None => match trimmed.split_once(':') {
            Some((host, path)) if !host.contains('/') => {
                return canonical_repo(&format!("{host}/{path}"));
            }
            _ => trimmed,
        },
    };
    let body = match body.split_once('/') {
        Some((host, rest)) if host.contains('@') => {
            format!("{}/{rest}", host.rsplit('@').next().unwrap_or(host))
        }
        _ => body.to_string(),
    };
    let body = body.trim_end_matches('/');
    let body = body.strip_suffix(".git").unwrap_or(body);
    let body = body.trim_end_matches('/').to_ascii_lowercase();
    if body.is_empty() || body == "/" {
        return Err(Error::usage(format!(
            "repository {origin:?} has no repository path"
        )));
    }
    Ok(body)
}

/// A pool's area name: its identity with every run of other characters as one
/// hyphen. Two declared pools may not share a slug.
pub(crate) fn pool_slug(repo: &str) -> Result<String> {
    let mut slug = String::new();
    for ch in repo.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    slug.truncate(MAX_SLUG);
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        return Err(Error::usage(format!(
            "repository {repo:?} has no name a pool can use"
        )));
    }
    validate_name("pool", &slug)?;
    Ok(slug)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origins_in_every_common_form_share_one_identity() {
        for origin in [
            "https://github.com/Owner/repo.git",
            "git@github.com:Owner/repo",
            "ssh://git@github.com/owner/repo/",
            "github.com/owner/repo",
        ] {
            assert_eq!(
                canonical_repo(origin).unwrap(),
                "github.com/owner/repo",
                "{origin}"
            );
        }
    }

    #[test]
    fn slugs_are_safe_names_and_unsafe_origins_are_refused() {
        assert_eq!(
            pool_slug("github.com/owner/repo").unwrap(),
            "github-com-owner-repo"
        );
        assert!(pool_slug("///").is_err());
        assert!(canonical_repo("--upload-pack=x").is_err());
        assert!(canonical_repo("two words").is_err());
    }

    #[test]
    fn declared_paths_stay_relative() {
        assert!(validate_declared_path("path", "apps/*/node_modules").is_ok());
        assert!(validate_declared_path("path", "../escape").is_err());
        assert!(validate_declared_path("path", "/absolute").is_err());
        assert!(validate_declared_path("path", "").is_err());
    }
}
