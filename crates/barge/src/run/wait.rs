//! Advisory positions among runs waiting for capacity in this process.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use super::Waiting;

static WAITERS: Mutex<Vec<Arc<Target>>> = Mutex::new(Vec::new());

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Queue {
    Machine,
    Profile,
}

#[derive(Debug, PartialEq, Eq)]
struct Target {
    state: PathBuf,
    queue: Queue,
    name: String,
}

#[derive(Debug)]
pub(super) struct Waiter {
    target: Arc<Target>,
    last_position: u32,
}

impl Waiter {
    fn join(state: &Path, queue: Queue, name: &str) -> Self {
        let target = Arc::new(Target {
            state: state.canonicalize().unwrap_or_else(|_| state.to_owned()),
            queue,
            name: name.to_owned(),
        });
        waiters().push(Arc::clone(&target));
        Self {
            target,
            last_position: 0,
        }
    }

    fn position(&self) -> u32 {
        let ahead = waiters()
            .iter()
            .take_while(|entry| !Arc::ptr_eq(entry, &self.target))
            .filter(|entry| entry.as_ref() == self.target.as_ref())
            .count();
        u32::try_from(ahead).unwrap_or(u32::MAX).saturating_add(1)
    }

    pub(super) fn report(
        waiter: &mut Option<Self>,
        state: &Path,
        queue: Queue,
        name: &str,
        progress: &mut dyn FnMut(Waiting),
    ) {
        if waiter
            .as_ref()
            .is_some_and(|waiter| waiter.target.name != name)
        {
            *waiter = None;
        }
        let waiter = waiter.get_or_insert_with(|| Self::join(state, queue, name));
        let position = waiter.position();
        if position != waiter.last_position {
            waiter.last_position = position;
            progress(Waiting {
                machine_or_profile: name.to_owned(),
                position,
            });
        }
    }
}

impl Drop for Waiter {
    fn drop(&mut self) {
        waiters().retain(|entry| !Arc::ptr_eq(entry, &self.target));
    }
}

fn waiters() -> MutexGuard<'static, Vec<Arc<Target>>> {
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
        let first = Waiter::join(&account, Queue::Profile, "one");
        let other_profile = Waiter::join(&account, Queue::Profile, "two");
        let other_account = Waiter::join(&other_account, Queue::Profile, "one");
        let machine = Waiter::join(&account, Queue::Machine, "one");
        let second = Waiter::join(&account, Queue::Profile, "one");
        assert_eq!(first.position(), 1);
        assert_eq!(other_profile.position(), 1);
        assert_eq!(other_account.position(), 1);
        assert_eq!(machine.position(), 1);
        assert_eq!(second.position(), 2);
        drop(first);
        assert_eq!(second.position(), 1);
    }
}
