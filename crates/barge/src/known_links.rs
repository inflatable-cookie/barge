//! The documented places where a tool keeps a link into Barge-managed storage.
//!
//! This is a fixed list of data, not an adapter: Barge never scans a home folder.
//! `barge move --plan` looks only at these paths, so a link a tool keeps
//! somewhere else is still the operator's to find by hand. Adding a place is
//! adding a row; adding behaviour for a tool is not what this module is for.
//!
//! A link found here that points inside a moved source, and that no binding
//! records, is a link the move would strand. The plan names it, the command that
//! adopts it after the move, and the value it must hold afterwards.
//!
//! The walk never follows a symlink on the way: every folder the pattern names
//! below the home folder must be a real folder, so a documented location cannot
//! be reached through a link and `barge move --apply` can only ever retarget the
//! one link the pattern ends at. The home folder itself is resolved once, because
//! where the account's home lives is a platform fact, not a link inside it.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::error::{Error, Result};
use crate::state::validate_name;

/// One documented place where a tool keeps a link into Barge-managed storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct KnownLink {
    /// A short label for reports, for example `colima-disk`.
    pub location: &'static str,
    /// The link's path below the home folder. A `*` segment stands for every
    /// entry at that level, which is how Lima names one disk per profile.
    pub pattern: &'static str,
    /// The consumer name an adoption should use, with `{name}` replaced by the
    /// wildcard folders, lower-cased and made safe for a consumer name.
    pub consumer: &'static str,
}

/// The list, as data. Keep it small: every row is a place Barge promises to
/// check before a move.
pub const KNOWN_LINKS: &[KnownLink] = &[
    KnownLink {
        location: "colima-disk",
        pattern: ".colima/_lima/*/disk",
        consumer: "colima-{name}-disk",
    },
    KnownLink {
        location: "cargo-registry",
        pattern: ".cargo/registry",
        consumer: "cargo-registry",
    },
    KnownLink {
        location: "cargo-git",
        pattern: ".cargo/git",
        consumer: "cargo-git",
    },
];

/// One existing path the list names under a home folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// The row it came from.
    pub known: KnownLink,
    /// The path itself, whether or not it is a link. `barge move` keeps only the
    /// symlinks.
    pub path: PathBuf,
    /// The consumer name an adoption of this path should use.
    pub consumer: String,
}

/// Every path the list names under `home` that exists now, in list order and
/// then name order. Nothing else is read: no folder is walked beyond the segments
/// a row names, and no symlinked folder is followed.
pub fn candidates(home: &Path) -> Result<Vec<Candidate>> {
    let home = match fs::canonicalize(home) {
        Ok(home) => home,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(Error::io("cannot resolve", home, &source)),
    };
    let mut found = Vec::new();
    for known in KNOWN_LINKS {
        let mut segments = known.pattern.split('/').peekable();
        let mut level = vec![(home.clone(), Vec::new())];
        while let Some(segment) = segments.next() {
            let last = segments.peek().is_none();
            let mut next = Vec::new();
            for (path, names) in level {
                if segment == "*" {
                    for child in real_directories(&path)? {
                        let Some(name) = child.file_name() else {
                            continue;
                        };
                        let mut names = names.clone();
                        names.push(name.to_string_lossy().into_owned());
                        next.push((child, names));
                    }
                    continue;
                }
                let child = path.join(segment);
                // The pattern ends at the link itself, which may be a symlink.
                // Every folder on the way must be real, so the walk never leaves
                // the documented location through one.
                if if last {
                    fs::symlink_metadata(&child).is_ok()
                } else {
                    is_real_directory(&child)
                } {
                    next.push((child, names));
                }
            }
            level = next;
        }
        for (path, names) in level {
            found.push(Candidate {
                known: *known,
                path,
                consumer: consumer_name(known, &names)?,
            });
        }
    }
    Ok(found)
}

/// The real subdirectories of a real directory, sorted, with none when it is
/// absent or is itself a symlink.
fn real_directories(folder: &Path) -> Result<Vec<PathBuf>> {
    if !is_real_directory(folder) {
        return Ok(Vec::new());
    }
    let mut paths = Vec::new();
    let reader = match fs::read_dir(folder) {
        Ok(reader) => reader,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(paths),
        Err(source) => return Err(Error::io("cannot read", folder, &source)),
    };
    for entry in reader {
        let entry = entry.map_err(|source| Error::io("cannot read", folder, &source))?;
        let path = entry.path();
        if is_real_directory(&path) {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

/// Whether `path` is a directory that is not itself a symlink.
fn is_real_directory(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_dir())
        .unwrap_or(false)
}

/// The consumer name for one candidate, from the row's template.
fn consumer_name(known: &KnownLink, names: &[String]) -> Result<String> {
    let mut consumer = known.consumer.to_string();
    if consumer.contains("{name}") {
        let filled = if names.is_empty() {
            "default".to_string()
        } else {
            names
                .iter()
                .map(|name| safe_name(name))
                .collect::<Vec<_>>()
                .join("-")
        };
        consumer = consumer.replace("{name}", &filled);
    }
    validate_name("suggested consumer", &consumer)?;
    Ok(consumer)
}

/// A wildcard folder name as a consumer may carry it: lower case, letters,
/// digits, `-`, `_` and `.`, and at most 32 characters.
fn safe_name(name: &str) -> String {
    let mut safe = String::new();
    for character in name.chars() {
        let character = character.to_ascii_lowercase();
        if character.is_ascii_lowercase()
            || character.is_ascii_digit()
            || matches!(character, '-' | '_' | '.')
        {
            safe.push(character);
        } else {
            safe.push('-');
        }
    }
    let safe = safe.trim_matches(|character| character == '-' || character == '.');
    let safe = safe.chars().take(32).collect::<String>();
    if safe.is_empty() {
        "unknown".to_string()
    } else {
        safe
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempDir;
    use std::os::unix::fs::symlink;

    #[test]
    fn named_places_are_found_in_list_order() {
        let temp = TempDir::new("known-links").unwrap();
        let home = fs::canonicalize(temp.folder("home").unwrap()).unwrap();
        for profile in ["b", "a"] {
            fs::create_dir_all(home.join(".colima/_lima").join(profile)).unwrap();
            fs::write(home.join(".colima/_lima").join(profile).join("disk"), b"").unwrap();
        }
        fs::create_dir_all(home.join(".cargo/registry")).unwrap();
        fs::create_dir_all(home.join(".cargo/git")).unwrap();

        let found = candidates(&home).unwrap();
        let described: Vec<(String, PathBuf, String)> = found
            .iter()
            .map(|candidate| {
                (
                    candidate.known.location.to_string(),
                    candidate.path.strip_prefix(&home).unwrap().to_path_buf(),
                    candidate.consumer.clone(),
                )
            })
            .collect();
        assert_eq!(
            described,
            vec![
                (
                    "colima-disk".to_string(),
                    PathBuf::from(".colima/_lima/a/disk"),
                    "colima-a-disk".to_string()
                ),
                (
                    "colima-disk".to_string(),
                    PathBuf::from(".colima/_lima/b/disk"),
                    "colima-b-disk".to_string()
                ),
                (
                    "cargo-registry".to_string(),
                    PathBuf::from(".cargo/registry"),
                    "cargo-registry".to_string()
                ),
                (
                    "cargo-git".to_string(),
                    PathBuf::from(".cargo/git"),
                    "cargo-git".to_string()
                ),
            ]
        );
    }

    #[test]
    fn an_empty_home_names_nothing() {
        let temp = TempDir::new("known-links-empty").unwrap();
        let home = temp.folder("home").unwrap();
        assert!(candidates(&home).unwrap().is_empty());
        assert!(candidates(&temp.path().join("absent")).unwrap().is_empty());
    }

    #[test]
    fn a_linked_folder_on_the_way_is_never_followed() {
        let temp = TempDir::new("known-links-symlink").unwrap();
        let home = fs::canonicalize(temp.folder("home").unwrap()).unwrap();
        let elsewhere = temp.folder("elsewhere").unwrap();
        // A real documented location is found.
        fs::create_dir_all(home.join(".colima/_lima/colima")).unwrap();
        fs::write(home.join(".colima/_lima/colima/disk"), b"").unwrap();
        assert_eq!(candidates(&home).unwrap().len(), 1);

        // The Colima folder replaced by a link: nothing is discovered through it,
        // although the same shape sits behind the link.
        fs::create_dir_all(elsewhere.join("_lima/colima")).unwrap();
        fs::write(elsewhere.join("_lima/colima/disk"), b"").unwrap();
        fs::remove_dir_all(home.join(".colima")).unwrap();
        symlink(&elsewhere, home.join(".colima")).unwrap();
        assert!(
            candidates(&home).unwrap().is_empty(),
            "a linked folder is not a documented location"
        );

        // A linked profile folder is not followed either, so an enumerated name
        // can never come from outside the documented shape.
        fs::remove_file(home.join(".colima")).unwrap();
        fs::create_dir_all(home.join(".colima/_lima")).unwrap();
        symlink(
            elsewhere.join("_lima/colima"),
            home.join(".colima/_lima/colima"),
        )
        .unwrap();
        assert!(candidates(&home).unwrap().is_empty());

        // An intermediate folder of another row behaves the same way.
        fs::create_dir_all(elsewhere.join("cargo")).unwrap();
        fs::write(elsewhere.join("cargo/registry"), b"").unwrap();
        symlink(elsewhere.join("cargo"), home.join(".cargo")).unwrap();
        assert!(candidates(&home).unwrap().is_empty());
    }

    #[test]
    fn a_linked_home_folder_is_resolved_once() {
        let temp = TempDir::new("known-links-linked-home").unwrap();
        let real = temp.folder("real-home").unwrap();
        fs::create_dir_all(real.join(".cargo")).unwrap();
        fs::write(real.join(".cargo/registry"), b"").unwrap();
        let link = temp.child("linked-home");
        symlink(&real, &link).unwrap();
        let found = candidates(&link).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(
            found[0].path,
            fs::canonicalize(&real).unwrap().join(".cargo/registry")
        );
    }

    #[test]
    fn a_profile_name_is_made_safe_for_a_consumer() {
        let temp = TempDir::new("known-links-name").unwrap();
        let home = temp.folder("home").unwrap();
        fs::create_dir_all(home.join(".colima/_lima/My VM")).unwrap();
        fs::write(home.join(".colima/_lima/My VM/disk"), b"").unwrap();
        let found = candidates(&home).unwrap();
        assert_eq!(found[0].consumer, "colima-my-vm-disk");
        assert_eq!(safe_name("___"), "___");
        assert_eq!(safe_name("..."), "unknown");
        assert_eq!(safe_name(&"x".repeat(80)).len(), 32);
    }
}
