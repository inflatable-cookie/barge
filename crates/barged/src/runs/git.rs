//! Git bare-mirror management, bundle ingestion, patch application, submodule
//! and LFS refusal, and exact tree verification.
//!
//! Every `git` invocation passes an explicit argv with no shell and disables
//! interactive credential prompts (`GIT_TERMINAL_PROMPT=0`).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use barge_machine::auth;
use sha2::{Digest, Sha256};

use crate::{Error, Result};

const LFS_POINTER_HEADER: &[u8] = b"version https://git-lfs.github.com/spec/v1";
/// Maximum size of a file we inspect for a Git LFS pointer header.
const MAX_LFS_POINTER_BYTES: u64 = 1024;

/// Stable 16-hex-char identifier for a repo string.
pub(crate) fn repo_hash(repo: &str) -> String {
    let digest = Sha256::digest(repo.as_bytes());
    auth::hex(&digest[..8])
}

/// Validate a git commit SHA (7..=64 ASCII hex chars, rejecting flag-like input).
pub(crate) fn validate_commit_sha(value: &str) -> Result<()> {
    if value.len() < 7 || value.len() > 64 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::new(format!(
            "invalid commit SHA {value:?}: expected 7 to 64 hex characters"
        )));
    }
    Ok(())
}

/// Path to the bare mirror inside a repo's mirror root.
pub(crate) fn mirror_dir(mirror_root: &Path) -> PathBuf {
    mirror_root.join("mirror.git")
}

/// Query which of `commits` exist as commit objects in the bare mirror.
pub(crate) fn commits_present(mirror_root: &Path, commits: &[String]) -> Result<Vec<String>> {
    let mirror = mirror_dir(mirror_root);
    if !mirror.exists() {
        return Ok(Vec::new());
    }
    let mut have = Vec::new();
    for commit in commits {
        validate_commit_sha(commit)?;
        let status = git_command()
            .arg("--git-dir")
            .arg(&mirror)
            .args(["cat-file", "-e", &format!("{commit}^{{commit}}")])
            .status()
            .map_err(|error| Error::new(format!("cannot run git cat-file: {error}")))?;
        if status.success() {
            have.push(commit.clone());
        }
    }
    Ok(have)
}

/// Apply a git bundle into the repo's bare mirror, creating the mirror if
/// needed, and pin every unbundled head ref so `git gc` never drops it.
pub(crate) fn apply_bundle(
    mirror_root: &Path,
    request_id: &str,
    bundle_bytes: &[u8],
) -> Result<()> {
    let mirror = ensure_bare_mirror(mirror_root)?;
    let bundle_path = mirror_root.join(format!(".upload-{request_id}.bundle"));
    fs::write(&bundle_path, bundle_bytes).map_err(|error| {
        Error::new(format!(
            "cannot write temporary bundle {}: {error}",
            bundle_path.display()
        ))
    })?;
    let outcome = unbundle_and_pin(&mirror, &bundle_path);
    let _ = fs::remove_file(&bundle_path);
    outcome
}

fn ensure_bare_mirror(mirror_root: &Path) -> Result<PathBuf> {
    let mirror = mirror_dir(mirror_root);
    if mirror.join("HEAD").exists() {
        return Ok(mirror);
    }
    fs::create_dir_all(&mirror).map_err(|error| {
        Error::new(format!(
            "cannot create mirror directory {}: {error}",
            mirror.display()
        ))
    })?;
    run_git(
        git_command()
            .arg("init")
            .arg("--bare")
            .arg("--quiet")
            .arg(&mirror),
        "git init --bare",
    )?;
    Ok(mirror)
}

fn unbundle_and_pin(mirror: &Path, bundle_path: &Path) -> Result<()> {
    let output = run_git(
        git_command()
            .arg("--git-dir")
            .arg(mirror)
            .arg("bundle")
            .arg("unbundle")
            .arg(bundle_path),
        "git bundle unbundle",
    )?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let Some(sha) = line.split_whitespace().next() else {
            continue;
        };
        if validate_commit_sha(sha).is_ok() {
            let ref_name = format!("refs/barge-uploads/{sha}");
            run_git(
                git_command()
                    .arg("--git-dir")
                    .arg(mirror)
                    .args(["update-ref", &ref_name, sha]),
                "git update-ref",
            )?;
        }
    }
    Ok(())
}

/// Prepare the slot's checkout at `base`, apply `patch` if present, reject
/// submodules and Git LFS, and verify `HEAD` plus `git write-tree` matches
/// `expected_tree`. Returns the verified tree SHA.
pub(crate) fn sync_checkout(
    mirror_root: &Path,
    checkout_dir: &Path,
    base: &str,
    patch: Option<&[u8]>,
    expected_tree: &str,
) -> Result<String> {
    validate_commit_sha(base)?;
    let mirror = mirror_dir(mirror_root);
    if !mirror.join("HEAD").exists() {
        return Err(Error::new(
            "bare mirror does not exist; upload a bundle before starting a run",
        ));
    }

    prepare_slot_repo(&mirror, checkout_dir, base)?;

    if let Some(patch_bytes) = patch {
        if !patch_bytes.is_empty() {
            apply_patch(checkout_dir, patch_bytes)?;
        }
    }

    reject_submodules(checkout_dir)?;
    reject_lfs(checkout_dir)?;

    let head = git_stdout(
        git_command()
            .arg("-C")
            .arg(checkout_dir)
            .args(["rev-parse", "HEAD"]),
        "git rev-parse HEAD",
    )?;
    let resolved_base = git_stdout(
        git_command().arg("-C").arg(checkout_dir).args([
            "rev-parse",
            "--verify",
            &format!("{base}^{{commit}}"),
        ]),
        "git rev-parse base",
    )?;
    if head != resolved_base {
        return Err(Error::new(format!(
            "HEAD mismatch after checkout: expected {resolved_base}, got {head}"
        )));
    }

    let actual_tree = git_stdout(
        git_command().arg("-C").arg(checkout_dir).arg("write-tree"),
        "git write-tree",
    )?;
    let combined = format!("{head}+{actual_tree}");
    if expected_tree != actual_tree && expected_tree != combined {
        return Err(Error::new(format!(
            "tree mismatch: expected {expected_tree}, got {combined}"
        )));
    }
    Ok(actual_tree)
}

fn prepare_slot_repo(mirror: &Path, checkout_dir: &Path, base: &str) -> Result<()> {
    let needs_clone = if checkout_dir.join(".git").exists() {
        // Ensure the existing slot checkout still sees `base` and is clean.
        let has_base = git_command()
            .arg("-C")
            .arg(checkout_dir)
            .args(["cat-file", "-e", &format!("{base}^{{commit}}")])
            .status()
            .is_ok_and(|s| s.success());
        if !has_base {
            let _ = git_command()
                .arg("-C")
                .arg(checkout_dir)
                .args(["fetch", "--no-tags"])
                .arg(mirror)
                .arg(base)
                .status();
        }
        reset_and_checkout(checkout_dir, base).is_err()
    } else {
        true
    };

    if needs_clone {
        if checkout_dir.exists() {
            fs::remove_dir_all(checkout_dir).map_err(|error| {
                Error::new(format!(
                    "cannot clean broken checkout {}: {error}",
                    checkout_dir.display()
                ))
            })?;
        }
        run_git(
            git_command()
                .args(["clone", "--shared", "--no-checkout", "--quiet"])
                .arg(mirror)
                .arg(checkout_dir),
            "git clone --shared",
        )?;
        run_git(
            git_command()
                .arg("-C")
                .arg(checkout_dir)
                .args(["checkout", "--force", "--detach", "--quiet", base]),
            "git checkout",
        )?;
    }
    Ok(())
}

fn reset_and_checkout(checkout_dir: &Path, base: &str) -> Result<()> {
    // Discard any staged/unstaged changes from a previous run.
    let _ = git_command()
        .arg("-C")
        .arg(checkout_dir)
        .args(["reset", "--hard", "--quiet"])
        .status();
    run_git(
        git_command()
            .arg("-C")
            .arg(checkout_dir)
            .args(["clean", "-fdx", "--quiet"]),
        "git clean",
    )?;
    run_git(
        git_command()
            .arg("-C")
            .arg(checkout_dir)
            .args(["checkout", "--force", "--detach", "--quiet", base]),
        "git checkout",
    )?;
    Ok(())
}

fn apply_patch(checkout_dir: &Path, patch_bytes: &[u8]) -> Result<()> {
    let patch_path = checkout_dir.join(".git").join("barge-run.patch");
    fs::write(&patch_path, patch_bytes).map_err(|error| {
        Error::new(format!(
            "cannot write patch file {}: {error}",
            patch_path.display()
        ))
    })?;
    let outcome = run_git(
        git_command()
            .arg("-C")
            .arg(checkout_dir)
            .args(["apply", "--index"])
            .arg(&patch_path),
        "git apply --index",
    );
    let _ = fs::remove_file(&patch_path);
    outcome.map(|_| ())
}

fn reject_submodules(checkout_dir: &Path) -> Result<()> {
    if checkout_dir.join(".gitmodules").exists() {
        return Err(Error::new(
            "submodules (.gitmodules) are not supported for remote runs",
        ));
    }
    let output = run_git(
        git_command()
            .arg("-C")
            .arg(checkout_dir)
            .args(["ls-files", "--stage"]),
        "git ls-files --stage",
    )?;
    let listing = String::from_utf8_lossy(&output.stdout);
    if listing.lines().any(|line| line.starts_with("160000 ")) {
        return Err(Error::new(
            "gitlink submodule entries are not supported for remote runs",
        ));
    }
    Ok(())
}

fn reject_lfs(checkout_dir: &Path) -> Result<()> {
    let gitattributes = checkout_dir.join(".gitattributes");
    if let Ok(contents) = fs::read_to_string(&gitattributes) {
        if contents.contains("filter=lfs") {
            return Err(Error::new(
                "Git LFS (.gitattributes filter=lfs) is not supported for remote runs",
            ));
        }
    }
    let output = run_git(
        git_command()
            .arg("-C")
            .arg(checkout_dir)
            .args(["ls-files", "-z"]),
        "git ls-files",
    )?;
    for rel_bytes in output.stdout.split(|&b| b == 0) {
        if rel_bytes.is_empty() {
            continue;
        }
        let Ok(rel) = std::str::from_utf8(rel_bytes) else {
            continue;
        };
        let path = checkout_dir.join(rel);
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.is_file() || meta.len() > MAX_LFS_POINTER_BYTES {
            continue;
        }
        if let Ok(bytes) = fs::read(&path) {
            if bytes.starts_with(LFS_POINTER_HEADER) {
                return Err(Error::new(format!(
                    "Git LFS pointer detected at {rel}; Git LFS is not supported for remote runs"
                )));
            }
        }
    }
    Ok(())
}

fn git_command() -> Command {
    let mut cmd = Command::new("git");
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    cmd
}

fn run_git(cmd: &mut Command, label: &str) -> Result<Output> {
    let output = cmd
        .output()
        .map_err(|error| Error::new(format!("cannot run {label}: {error}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.trim();
        return Err(Error::new(if detail.is_empty() {
            format!("{label} failed with status {}", output.status)
        } else {
            format!("{label} failed: {detail}")
        }));
    }
    Ok(output)
}

fn git_stdout(cmd: &mut Command, label: &str) -> Result<String> {
    let output = run_git(cmd, label)?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}
