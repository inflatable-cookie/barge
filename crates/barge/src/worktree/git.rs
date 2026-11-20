//! The git operations a worktree pool needs, by shelling out to `git`.
//!
//! Every call that names a checkout first checks that the folder is a linked
//! worktree (its `.git` is a file). A folder that is not one is never handed to
//! git, because `git -C` would walk up to whatever repository encloses it. Calls
//! never prompt, and the repository overrides a parent process may carry are
//! dropped, so a hook's `GIT_DIR` cannot point a pool at another repository.

use std::path::Path;
use std::process::Command;

use crate::error::{Error, Result};

/// One `git` call: whether it succeeded, and what it printed.
#[derive(Debug)]
pub(crate) struct Output {
    pub(crate) success: bool,
    pub(crate) stdout: String,
    stderr: String,
}

fn raw(dir: Option<&Path>, args: &[&str]) -> Result<Output> {
    let mut command = Command::new("git");
    if let Some(dir) = dir {
        command.arg("-C").arg(dir);
    }
    let output = command
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .map_err(|source| Error::io_message(format!("cannot run git: {source}")))?;
    Ok(Output {
        success: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
    })
}

/// Run git and return its standard output; a non-zero exit is an I/O failure
/// that names the command and git's own message.
fn run(dir: Option<&Path>, args: &[&str]) -> Result<String> {
    let output = raw(dir, args)?;
    if output.success {
        return Ok(output.stdout);
    }
    let location = dir.map_or_else(String::new, |dir| format!(" in {}", dir.display()));
    Err(Error::io_message(format!(
        "git {}{location} failed: {}",
        args.join(" "),
        output.stderr
    )))
}

fn text(path: &Path) -> Result<&str> {
    path.to_str().ok_or_else(|| {
        Error::usage(format!(
            "path {} is not valid UTF-8, which git calls need",
            path.display()
        ))
    })
}

/// Refuse a checkout that is not a linked worktree before any git call reads
/// or changes it.
pub(crate) fn require_worktree(tree: &Path) -> Result<()> {
    if tree.join(".git").is_file() {
        Ok(())
    } else {
        Err(Error::unknown_state(format!(
            "{} is not a linked worktree; nothing was changed",
            tree.display()
        )))
    }
}

/// Whether git accepts `name` as a branch name. Needs no repository.
pub(crate) fn valid_branch_name(name: &str) -> Result<bool> {
    Ok(raw(None, &["check-ref-format", &format!("refs/heads/{name}")])?.success)
}

/// Create the pool's bare clone and record its origin as `origin`.
pub(crate) fn init_bare(clone: &Path, origin: &str) -> Result<()> {
    run(None, &["init", "--bare", "--quiet", text(clone)?])?;
    run(Some(clone), &["remote", "add", "origin", origin])?;
    Ok(())
}

/// Fetch every branch of the origin into the clone's remote-tracking refs.
pub(crate) fn fetch(clone: &Path) -> Result<()> {
    run(
        Some(clone),
        &["fetch", "--prune", "--no-tags", "--quiet", "origin"],
    )?;
    Ok(())
}

/// Drop the metadata of worktrees whose folder is gone.
pub(crate) fn prune_worktrees(clone: &Path) -> Result<()> {
    run(Some(clone), &["worktree", "prune"])?;
    Ok(())
}

/// The commit a revision names, when it names one.
pub(crate) fn commit_of(dir: &Path, revision: &str) -> Result<Option<String>> {
    let output = raw(
        Some(dir),
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{revision}^{{commit}}"),
        ],
    )?;
    Ok(output
        .success
        .then(|| output.stdout.trim().to_string())
        .filter(|sha| !sha.is_empty()))
}

/// Whether the clone already has a local branch of this name.
pub(crate) fn branch_exists(clone: &Path, branch: &str) -> Result<bool> {
    Ok(raw(
        Some(clone),
        &[
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )?
    .success)
}

/// Check out a new branch at `base` in a folder that does not exist yet.
pub(crate) fn add_worktree(clone: &Path, tree: &Path, branch: &str, base: &str) -> Result<()> {
    run(
        Some(clone),
        &[
            "worktree",
            "add",
            "--quiet",
            "--no-track",
            "-b",
            branch,
            text(tree)?,
            base,
        ],
    )?;
    Ok(())
}

/// Start a new branch at `base` in an existing, clean worktree.
pub(crate) fn checkout_branch(tree: &Path, branch: &str, base: &str) -> Result<()> {
    require_worktree(tree)?;
    run(
        Some(tree),
        &["checkout", "--quiet", "--no-track", "-b", branch, base],
    )?;
    Ok(())
}

/// Detach a worktree at `target`, discarding its tracked changes.
pub(crate) fn detach(tree: &Path, target: &str) -> Result<()> {
    require_worktree(tree)?;
    run(
        Some(tree),
        &["checkout", "--quiet", "--force", "--detach", target],
    )?;
    Ok(())
}

/// Delete a local branch of the clone. The branch must not be checked out.
pub(crate) fn delete_branch(clone: &Path, branch: &str) -> Result<()> {
    run(Some(clone), &["branch", "--quiet", "-D", branch])?;
    Ok(())
}

/// The commit a worktree's HEAD names.
pub(crate) fn head(tree: &Path) -> Result<String> {
    require_worktree(tree)?;
    Ok(
        run(Some(tree), &["rev-parse", "--verify", "HEAD^{commit}"])?
            .trim()
            .to_string(),
    )
}

/// Every tracked change and every untracked, unignored entry in a worktree.
pub(crate) fn dirty_entries(tree: &Path) -> Result<Vec<String>> {
    require_worktree(tree)?;
    let output = run(Some(tree), &["status", "--porcelain", "-z"])?;
    Ok(output
        .split('\0')
        .filter(|entry| !entry.is_empty())
        .map(str::to_string)
        .collect())
}

/// Every ignored, untracked path in a worktree. A fully ignored folder is one
/// entry with a trailing slash.
pub(crate) fn ignored_entries(tree: &Path) -> Result<Vec<String>> {
    require_worktree(tree)?;
    let output = run(
        Some(tree),
        &[
            "ls-files",
            "-z",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--directory",
        ],
    )?;
    Ok(output
        .split('\0')
        .filter(|entry| !entry.is_empty())
        .map(str::to_string)
        .collect())
}

/// How many commits HEAD has beyond `base`.
pub(crate) fn commits_beyond(tree: &Path, base: &str) -> Result<u64> {
    require_worktree(tree)?;
    let output = run(
        Some(tree),
        &["rev-list", "--count", &format!("{base}..HEAD")],
    )?;
    output.trim().parse().map_err(|_| {
        Error::unknown_state(format!(
            "git printed an unreadable commit count for {}",
            tree.display()
        ))
    })
}
