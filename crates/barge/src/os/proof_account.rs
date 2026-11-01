//! The proof-only synthetic account for F6-R1's native proof.
//!
//! Compiled only under the `proof-synthetic-account` feature, which no ordinary
//! build enables. Under it alone, Oxide's state and cache directories come from
//! one validated private account home instead of the password database's. The
//! binding checks live here. Nothing else changes: release gates, plan parsing,
//! deletion, custody and the incomplete-reference rule are the ordinary code.
//!
//! What a run of this build establishes is release logic over synthetic
//! `AccountFacts`. It does not establish that the production account lookup is
//! equivalent to this one (docs/knowledge/contracts/stage4-design.md).

use std::ffi::OsString;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::error::{Error, ErrorKind, Result};
use crate::state::{ACCOUNT_HOME_ENV, STATE_DIR_ENV};

/// The supervisor's owned scratch root. Every synthetic path must stay inside it.
pub const TASK_SCRATCH_ROOT_ENV: &str = "OXIDE_TASK_SCRATCH_ROOT";

/// The supervisor's evidence directory. Its presence marks the run as supervised.
pub const PROOF_EVIDENCE_ENV: &str = "OXIDE_PROOF_EVIDENCE";

/// The private home the proof exports as `HOME`. It must be the synthetic account home.
pub const HOME_ENV: &str = "HOME";

/// The bindings a synthetic run presents, read once so tests can pass their own.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Bindings {
    account_home: Option<OsString>,
    home: Option<OsString>,
    state_dir: Option<OsString>,
    scratch_root: Option<OsString>,
    evidence: Option<OsString>,
}

impl Bindings {
    /// The bindings this process was started with.
    pub(super) fn from_env() -> Self {
        Self {
            account_home: std::env::var_os(ACCOUNT_HOME_ENV),
            home: std::env::var_os(HOME_ENV),
            state_dir: std::env::var_os(STATE_DIR_ENV),
            scratch_root: std::env::var_os(TASK_SCRATCH_ROOT_ENV),
            evidence: std::env::var_os(PROOF_EVIDENCE_ENV),
        }
    }
}

/// The validated synthetic account home for Oxide's directories.
///
/// `uid` is the effective uid every bound path must belong to. `real_home` is the
/// password database's home, which the synthetic home must not lie inside. Every
/// binding must be present: an unbound run refuses rather than reading the real
/// home. The result is the canonical account home, and a refusal changes nothing.
pub(super) fn oxide_account_home(
    bindings: &Bindings,
    uid: u32,
    real_home: &Path,
) -> Result<PathBuf> {
    let scratch_raw = required(&bindings.scratch_root, TASK_SCRATCH_ROOT_ENV)?;
    real_directory(TASK_SCRATCH_ROOT_ENV, scratch_raw, uid)?;
    let scratch = resolve(TASK_SCRATCH_ROOT_ENV, scratch_raw)?;

    let evidence = required(&bindings.evidence, PROOF_EVIDENCE_ENV)?;
    real_directory(PROOF_EVIDENCE_ENV, evidence, uid)?;

    let account_raw = required(&bindings.account_home, ACCOUNT_HOME_ENV)?;
    let account = contained_directory(ACCOUNT_HOME_ENV, account_raw, &scratch, uid)?;

    let home_raw = required(&bindings.home, HOME_ENV)?;
    let home = contained_directory(HOME_ENV, home_raw, &scratch, uid)?;
    if home != account {
        return Err(refuse(
            ErrorKind::RefusedBoundary,
            HOME_ENV,
            home_raw,
            "is not the synthetic account home, so the private home is not the bound one",
        ));
    }

    let state_raw = required(&bindings.state_dir, STATE_DIR_ENV)?;
    contained_directory(STATE_DIR_ENV, state_raw, &scratch, uid)?;

    let real = std::fs::canonicalize(real_home).unwrap_or_else(|_| real_home.to_path_buf());
    if account.starts_with(&real) {
        return Err(refuse(
            ErrorKind::RefusedBoundary,
            ACCOUNT_HOME_ENV,
            account_raw,
            "lies inside the real account home",
        ));
    }
    Ok(account)
}

/// A binding that must be present; an absent one is a run outside its supervisor.
fn required<'a>(value: &'a Option<OsString>, name: &str) -> Result<&'a Path> {
    match value {
        Some(raw) => Ok(Path::new(raw)),
        None => Err(Error::new(
            ErrorKind::Usage,
            format!("{name} is not bound, so this proof-only build will not resolve Oxide's directories"),
        )
        .with_detail("binding", name.into())),
    }
}

/// A real directory this account owns: absolute, not a symlink, owned by `uid`.
fn real_directory(name: &str, path: &Path, uid: u32) -> Result<()> {
    if !path.is_absolute() {
        return Err(refuse(
            ErrorKind::Usage,
            name,
            path,
            "must be an absolute path",
        ));
    }
    let metadata = std::fs::symlink_metadata(path).map_err(|error| {
        refuse(
            ErrorKind::UnknownState,
            name,
            path,
            &format!("is absent or cannot be read: {error}"),
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(refuse(
            ErrorKind::UnknownState,
            name,
            path,
            "is not a real directory; a symlink is refused",
        ));
    }
    if metadata.uid() != uid {
        return Err(refuse(
            ErrorKind::ForeignOwner,
            name,
            path,
            "is not owned by this account",
        ));
    }
    Ok(())
}

/// The canonical form of one bound, existing path.
fn resolve(name: &str, path: &Path) -> Result<PathBuf> {
    std::fs::canonicalize(path).map_err(|error| {
        refuse(
            ErrorKind::UnknownState,
            name,
            path,
            &format!("cannot be resolved: {error}"),
        )
    })
}

/// A real directory whose resolved form lies strictly inside `scratch`.
///
/// The literal path must not be a symlink, and the resolved path must stay under
/// the scratch root, so a symlinked ancestor cannot carry the binding out of it.
fn contained_directory(name: &str, path: &Path, scratch: &Path, uid: u32) -> Result<PathBuf> {
    real_directory(name, path, uid)?;
    let resolved = resolve(name, path)?;
    if resolved == scratch || !resolved.starts_with(scratch) {
        return Err(refuse(
            ErrorKind::RefusedContainment,
            name,
            path,
            "resolves outside the owned scratch root",
        ));
    }
    Ok(resolved)
}

fn refuse(kind: ErrorKind, name: &str, path: &Path, why: &str) -> Error {
    Error::new(kind, format!("{name} {} {why}", path.display()))
        .with_detail("binding", name.into())
        .with_detail("path", path.display().to_string().into())
}

#[cfg(test)]
mod tests {
    // A failed fixture step is the test's failure, so these tests opt out of the shipped-path lints.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::path::PathBuf;

    use super::{
        oxide_account_home, Bindings, HOME_ENV, PROOF_EVIDENCE_ENV, TASK_SCRATCH_ROOT_ENV,
    };
    use crate::error::ErrorKind;
    use crate::state::{ACCOUNT_HOME_ENV, STATE_DIR_ENV};
    use crate::testkit::TempDir;
    use std::os::unix::fs::MetadataExt;

    /// One run's tree: `scratch/` is the owned root, `outside/` is its sibling, and
    /// `scratch/home` is the synthetic account home the bindings name.
    struct Tree {
        root: TempDir,
        uid: u32,
    }

    impl Tree {
        fn new(label: &str) -> Self {
            let root = TempDir::new(label);
            let uid = std::fs::metadata(root.path())
                .expect("the fixture exists")
                .uid();
            let tree = Self { root, uid };
            for name in [
                "scratch/home",
                "scratch/state",
                "scratch/evidence",
                "outside/home",
            ] {
                std::fs::create_dir_all(tree.path(name)).expect("the fixture directory exists");
            }
            tree
        }

        fn path(&self, name: &str) -> PathBuf {
            self.root.path().join(name)
        }

        /// Every binding names the good tree.
        fn bound(&self) -> Bindings {
            Bindings {
                account_home: Some(self.path("scratch/home").into_os_string()),
                home: Some(self.path("scratch/home").into_os_string()),
                state_dir: Some(self.path("scratch/state").into_os_string()),
                scratch_root: Some(self.path("scratch").into_os_string()),
                evidence: Some(self.path("scratch/evidence").into_os_string()),
            }
        }

        /// The real home is a path this tree never creates, so no account lies inside it.
        fn real_home(&self) -> PathBuf {
            self.path("real-home")
        }
    }

    fn with(bindings: Bindings, name: &str, value: Option<PathBuf>) -> Bindings {
        let value = value.map(PathBuf::into_os_string);
        match name {
            ACCOUNT_HOME_ENV => Bindings {
                account_home: value,
                ..bindings
            },
            HOME_ENV => Bindings {
                home: value,
                ..bindings
            },
            STATE_DIR_ENV => Bindings {
                state_dir: value,
                ..bindings
            },
            TASK_SCRATCH_ROOT_ENV => Bindings {
                scratch_root: value,
                ..bindings
            },
            PROOF_EVIDENCE_ENV => Bindings {
                evidence: value,
                ..bindings
            },
            other => panic!("no binding named {other}"),
        }
    }

    #[test]
    fn a_fully_bound_private_home_is_the_synthetic_account() {
        let tree = Tree::new("proof-account-good");
        let home = oxide_account_home(&tree.bound(), tree.uid, &tree.real_home())
            .expect("a bound private home inside the scratch root is accepted");
        assert_eq!(
            home,
            std::fs::canonicalize(tree.path("scratch/home")).expect("the home resolves")
        );
    }

    #[test]
    fn an_unbound_proof_build_refuses_rather_than_reading_the_real_home() {
        let tree = Tree::new("proof-account-unbound");
        for name in [
            ACCOUNT_HOME_ENV,
            HOME_ENV,
            STATE_DIR_ENV,
            TASK_SCRATCH_ROOT_ENV,
            PROOF_EVIDENCE_ENV,
        ] {
            let refused =
                oxide_account_home(&with(tree.bound(), name, None), tree.uid, &tree.real_home())
                    .expect_err("an unbound binding is refused");
            assert_eq!(refused.kind(), ErrorKind::Usage, "{name}");
            assert_eq!(refused.detail()["binding"], name, "{name}");
        }
    }

    #[test]
    fn a_relative_or_absent_account_home_refuses() {
        let tree = Tree::new("proof-account-relative");
        let relative = with(
            tree.bound(),
            ACCOUNT_HOME_ENV,
            Some(PathBuf::from("relative/home")),
        );
        let refused = oxide_account_home(&relative, tree.uid, &tree.real_home())
            .expect_err("a relative home is refused");
        assert_eq!(refused.kind(), ErrorKind::Usage);

        let absent = with(
            tree.bound(),
            ACCOUNT_HOME_ENV,
            Some(tree.path("scratch/missing")),
        );
        let refused = oxide_account_home(&absent, tree.uid, &tree.real_home())
            .expect_err("an absent home is refused");
        assert_eq!(refused.kind(), ErrorKind::UnknownState);
    }

    #[test]
    fn a_symlinked_account_home_refuses() {
        let tree = Tree::new("proof-account-symlink");
        let link = tree.path("scratch/link");
        std::os::unix::fs::symlink(tree.path("scratch/home"), &link).expect("the link is made");
        let bindings = with(
            with(tree.bound(), ACCOUNT_HOME_ENV, Some(link.clone())),
            HOME_ENV,
            Some(link.clone()),
        );
        let refused = oxide_account_home(&bindings, tree.uid, &tree.real_home())
            .expect_err("a symlinked home is refused");
        assert_eq!(refused.kind(), ErrorKind::UnknownState);
    }

    #[test]
    fn a_home_that_resolves_outside_the_scratch_root_refuses() {
        let tree = Tree::new("proof-account-escape");
        // The sibling directory is outside the scratch root by component.
        let sibling = with(
            with(
                tree.bound(),
                ACCOUNT_HOME_ENV,
                Some(tree.path("outside/home")),
            ),
            HOME_ENV,
            Some(tree.path("outside/home")),
        );
        let refused = oxide_account_home(&sibling, tree.uid, &tree.real_home())
            .expect_err("a sibling home is refused");
        assert_eq!(refused.kind(), ErrorKind::RefusedContainment);

        // A symlinked ancestor carries the literal path inside and the resolved path out.
        std::os::unix::fs::symlink(tree.path("outside"), tree.path("scratch/hop"))
            .expect("the ancestor link is made");
        let via_hop = tree.path("scratch/hop/home");
        let hopped = with(
            with(tree.bound(), ACCOUNT_HOME_ENV, Some(via_hop.clone())),
            HOME_ENV,
            Some(via_hop.clone()),
        );
        let refused = oxide_account_home(&hopped, tree.uid, &tree.real_home())
            .expect_err("a home reached through an escaping ancestor is refused");
        assert_eq!(refused.kind(), ErrorKind::RefusedContainment);
    }

    #[test]
    fn a_state_dir_outside_the_scratch_root_refuses() {
        let tree = Tree::new("proof-account-state");
        let bindings = with(tree.bound(), STATE_DIR_ENV, Some(tree.path("outside/home")));
        let refused = oxide_account_home(&bindings, tree.uid, &tree.real_home())
            .expect_err("a state directory outside the scratch root is refused");
        assert_eq!(refused.kind(), ErrorKind::RefusedContainment);
    }

    #[test]
    fn a_home_that_is_not_the_account_home_refuses() {
        let tree = Tree::new("proof-account-mismatch");
        let bindings = with(tree.bound(), HOME_ENV, Some(tree.path("scratch/state")));
        let refused = oxide_account_home(&bindings, tree.uid, &tree.real_home())
            .expect_err("a private home other than the account home is refused");
        assert_eq!(refused.kind(), ErrorKind::RefusedBoundary);
    }

    #[test]
    fn a_foreign_owned_binding_refuses() {
        let tree = Tree::new("proof-account-owner");
        let refused =
            oxide_account_home(&tree.bound(), tree.uid.wrapping_add(1), &tree.real_home())
                .expect_err("a binding this account does not own is refused");
        assert_eq!(refused.kind(), ErrorKind::ForeignOwner);
    }

    #[test]
    fn an_account_home_inside_the_real_home_refuses() {
        let tree = Tree::new("proof-account-real");
        let refused = oxide_account_home(&tree.bound(), tree.uid, &tree.path("scratch"))
            .expect_err("a synthetic home inside the real home is refused");
        assert_eq!(refused.kind(), ErrorKind::RefusedBoundary);
    }
}
