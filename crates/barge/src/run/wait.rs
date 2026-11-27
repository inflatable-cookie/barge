//! Advisory positions among runs waiting in this process. The account's
//! container lease remains the authority for capacity.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

static WAITERS: Mutex<Vec<Arc<Profile>>> = Mutex::new(Vec::new());

#[derive(Debug, PartialEq, Eq)]
struct Profile {
    state: PathBuf,
    name: String,
}

#[derive(Debug)]
pub(super) struct Waiter(Arc<Profile>);

impl Waiter {
    pub(super) fn join(state: &Path, profile: &str) -> Self {
        let entry = Arc::new(Profile {
            state: state.canonicalize().unwrap_or_else(|_| state.to_owned()),
            name: profile.to_owned(),
        });
        waiters().push(Arc::clone(&entry));
        Self(entry)
    }

    pub(super) fn position(&self) -> u32 {
        let ahead = waiters()
            .iter()
            .take_while(|entry| !Arc::ptr_eq(entry, &self.0))
            .filter(|entry| entry.as_ref() == self.0.as_ref())
            .count();
        u32::try_from(ahead).unwrap_or(u32::MAX).saturating_add(1)
    }
}

impl Drop for Waiter {
    fn drop(&mut self) {
        waiters().retain(|entry| !Arc::ptr_eq(entry, &self.0));
    }
}

fn waiters() -> MutexGuard<'static, Vec<Arc<Profile>>> {
    WAITERS.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempDir;

    #[test]
    fn a_waiter_moves_up_when_an_earlier_run_leaves_its_profile() {
        let temp = TempDir::new("run-waiters").unwrap();
        let account = temp.folder("account").unwrap();
        let other_account = temp.folder("other-account").unwrap();
        let first = Waiter::join(&account, "one");
        let other_profile = Waiter::join(&account, "two");
        let other_account = Waiter::join(&other_account, "one");
        let second = Waiter::join(&account, "one");
        assert_eq!(first.position(), 1);
        assert_eq!(other_profile.position(), 1);
        assert_eq!(other_account.position(), 1);
        assert_eq!(second.position(), 2);
        drop(first);
        assert_eq!(second.position(), 1);
    }
}
