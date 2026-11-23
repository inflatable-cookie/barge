//! The return's cleanup rules: sort each ignored path in a worktree into kept
//! (a warm path), removed (declared disposable), or undeclared (refuses the
//! return), and remove the disposable ones without following a symlink.

use std::path::Path;

use crate::error::{Error, Result};
use crate::os::Tree;

/// How the ignored paths of one worktree were sorted.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Sweep {
    /// Warm paths: left in place.
    pub(crate) kept: Vec<String>,
    /// Declared disposable: removed.
    pub(crate) removed: Vec<String>,
    /// Neither declared: the return is refused, because Barge cannot account
    /// for them and so does not delete them.
    pub(crate) undeclared: Vec<String>,
}

/// Sort ignored entries. Warm paths are checked first, so a path declared both
/// ways is kept rather than deleted.
pub(crate) fn classify(entries: &[String], warm: &[&str], disposable: &[&str]) -> Sweep {
    let mut sweep = Sweep::default();
    for entry in entries {
        if warm.iter().any(|pattern| covers(pattern, entry)) {
            sweep.kept.push(entry.clone());
        } else if disposable.iter().any(|pattern| covers(pattern, entry)) {
            sweep.removed.push(entry.clone());
        } else {
            sweep.undeclared.push(entry.clone());
        }
    }
    sweep
}

/// Whether a declared path covers an entry: the same path, a folder above it,
/// or a glob (`*` within one segment) that matches the entry's leading segments.
pub(crate) fn covers(pattern: &str, entry: &str) -> bool {
    let mut pattern_segments = pattern.trim_end_matches('/').split('/');
    let mut entry_segments = entry.trim_end_matches('/').split('/');
    loop {
        match (pattern_segments.next(), entry_segments.next()) {
            (None, _) => return true,
            (Some(_), None) => return false,
            (Some(pattern), Some(segment)) => {
                if !segment_matches(pattern, segment) {
                    return false;
                }
            }
        }
    }
}

/// Match one path segment against a pattern whose only wildcard is `*`.
pub(crate) fn segment_matches(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let (mut p, mut t) = (0, 0);
    let mut star: Option<usize> = None;
    let mut resume = 0;
    while t < text.len() {
        if p < pattern.len() && pattern[p] == '*' {
            star = Some(p);
            resume = t;
            p += 1;
        } else if p < pattern.len() && pattern[p] == text[t] {
            p += 1;
            t += 1;
        } else if let Some(star_at) = star {
            p = star_at + 1;
            resume += 1;
            t = resume;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == '*' {
        p += 1;
    }
    p == pattern.len()
}

/// Remove one entry from a worktree. A symlink is removed as a link and never
/// followed; a folder is emptied through the same rule, and a nested folder on
/// another filesystem is refused rather than walked.
pub(crate) fn remove_entry(tree: &Path, entry: &str) -> Result<()> {
    let relative = Path::new(entry.trim_end_matches('/'));
    let removed = Tree::open(tree)?.remove_entry(relative)?;
    if removed.removed {
        return Ok(());
    }
    Err(Error::refused(format!(
        "could not remove {}: {}",
        tree.join(relative).display(),
        crate::trash::stayed_reason(&removed)
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declared_paths_cover_their_contents_and_globs_match_one_segment() {
        assert!(covers("dist", "dist/"));
        assert!(covers("dist", "dist/app.js"));
        assert!(!covers("dist", "distant/"));
        assert!(covers("apps/*/node_modules", "apps/web/node_modules/"));
        assert!(!covers("apps/*/node_modules", "apps/web/x/node_modules/"));
        assert!(covers("apps/w*b/dist", "apps/web/dist/"));
        assert!(!covers("apps/*", "other/file"));
    }

    #[test]
    fn warm_paths_win_over_disposable_and_the_rest_is_undeclared() {
        let entries = vec![
            "node_modules/".to_string(),
            "dist/".to_string(),
            "secret.env".to_string(),
        ];
        let sweep = classify(&entries, &["node_modules"], &["node_modules", "dist"]);
        assert_eq!(sweep.kept, vec!["node_modules/".to_string()]);
        assert_eq!(sweep.removed, vec!["dist/".to_string()]);
        assert_eq!(sweep.undeclared, vec!["secret.env".to_string()]);
    }
}
