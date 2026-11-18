//! Power: one pending action, a deadline, and a backend that performs exactly
//! three things.
//!
//! [`PowerBackend`] is the seam. On Linux the implementation asks logind over
//! D-Bus for `Suspend`, `Reboot` or `PowerOff`; it never runs a command and the
//! request body cannot name a method, because [`PowerActionKind`] is a closed
//! enum. [`Clock`] is the other seam: tests advance a fake clock instead of
//! sleeping.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use barge_machine::{Pending, PowerActionKind, MAX_POWER_DELAY_S};

use crate::{Error, Result};

/// What the daemon performs when a countdown fires.
pub trait PowerBackend: Send + Sync + 'static {
    fn perform(&self, action: PowerActionKind) -> Result<()>;
}

/// The deadline's source of truth.
pub trait Clock: Send + Sync + 'static {
    fn now_unix_ms(&self) -> u64;
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_unix_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0)
    }
}

/// Why a power request was not accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestError {
    /// One action is already counting down.
    Pending(Pending),
    /// The delay is longer than the daemon accepts.
    DelayTooLong { limit: u32 },
}

impl fmt::Display for RequestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pending(pending) => write!(
                f,
                "a {} is already pending until {}",
                pending.action.as_str(),
                pending.deadline_unix_ms
            ),
            Self::DelayTooLong { limit } => {
                write!(f, "delay must be 0-{limit} seconds")
            }
        }
    }
}

/// The logind method for an action. The match is exhaustive, so the daemon can
/// only ever ask logind for these three.
pub fn logind_method(action: PowerActionKind) -> &'static str {
    match action {
        PowerActionKind::Sleep => "Suspend",
        PowerActionKind::Restart => "Reboot",
        PowerActionKind::Shutdown => "PowerOff",
    }
}

#[derive(Debug)]
struct Shared {
    pending: Mutex<Option<Pending>>,
    changed: Condvar,
    stopped: AtomicBool,
}

/// One pending power action at a time, with a cancellable countdown.
#[derive(Clone)]
pub struct PowerController {
    shared: Arc<Shared>,
    backend: Arc<dyn PowerBackend>,
    clock: Arc<dyn Clock>,
}

impl fmt::Debug for PowerController {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PowerController")
            .field("pending", &self.pending())
            .finish_non_exhaustive()
    }
}

impl PowerController {
    pub fn new(backend: Arc<dyn PowerBackend>, clock: Arc<dyn Clock>) -> Self {
        Self {
            shared: Arc::new(Shared {
                pending: Mutex::new(None),
                changed: Condvar::new(),
                stopped: AtomicBool::new(false),
            }),
            backend,
            clock,
        }
    }

    /// Accept a countdown, or refuse because one is already running.
    pub fn request(
        &self,
        action: PowerActionKind,
        delay_s: u32,
    ) -> std::result::Result<Pending, RequestError> {
        if delay_s > MAX_POWER_DELAY_S {
            return Err(RequestError::DelayTooLong {
                limit: MAX_POWER_DELAY_S,
            });
        }
        let mut pending = self.lock();
        if let Some(current) = pending.as_ref() {
            return Err(RequestError::Pending(*current));
        }
        let deadline_unix_ms = self
            .clock
            .now_unix_ms()
            .saturating_add(u64::from(delay_s) * 1000);
        let requested = Pending {
            action,
            deadline_unix_ms,
        };
        *pending = Some(requested);
        self.shared.changed.notify_all();
        Ok(requested)
    }

    /// Stop the countdown. `false` means nothing was counting down.
    pub fn cancel(&self) -> bool {
        let mut pending = self.lock();
        let cancelled = pending.take().is_some();
        if cancelled {
            self.shared.changed.notify_all();
        }
        cancelled
    }

    pub fn pending(&self) -> Option<Pending> {
        self.lock().to_owned()
    }

    /// Fire the pending action if its deadline has passed, and say which one
    /// fired. The runner calls this every time it wakes; tests call it after
    /// moving a fake clock, so the countdown's real path is exercised without
    /// waiting.
    pub fn fire_due(&self) -> Option<PowerActionKind> {
        let mut pending = self.lock();
        if self.shared.stopped.load(Ordering::SeqCst) {
            return None;
        }
        let now = self.clock.now_unix_ms();
        let requested = if pending
            .as_ref()
            .is_some_and(|entry| entry.deadline_unix_ms <= now)
        {
            pending.take()
        } else {
            None
        }?;
        drop(pending);
        if let Err(error) = self.backend.perform(requested.action) {
            eprintln!("barged: {} failed: {error}", requested.action.as_str());
        }
        Some(requested.action)
    }

    /// Start the countdown thread. It ends when [`PowerController::stop`] is
    /// called.
    pub fn start(&self) -> std::io::Result<JoinHandle<()>> {
        let runner = self.clone();
        std::thread::Builder::new()
            .name("barged-countdown".to_string())
            .spawn(move || runner.run())
    }

    /// Ask the countdown thread to finish. A pending action is dropped, not
    /// fired: a daemon being stopped must not power off the machine.
    pub fn stop(&self) {
        let _pending = self.lock();
        self.shared.stopped.store(true, Ordering::SeqCst);
        self.shared.changed.notify_all();
    }

    fn run(&self) {
        while !self.shared.stopped.load(Ordering::SeqCst) {
            self.wait_for_change();
            if !self.shared.stopped.load(Ordering::SeqCst) {
                self.fire_due();
            }
        }
    }

    fn wait_for_change(&self) {
        let pending = self.lock();
        if self.shared.stopped.load(Ordering::SeqCst) {
            return;
        }
        match pending.as_ref() {
            Some(entry) => {
                let wait = Duration::from_millis(
                    entry
                        .deadline_unix_ms
                        .saturating_sub(self.clock.now_unix_ms()),
                );
                drop(self.shared.changed.wait_timeout(pending, wait));
            }
            None => {
                drop(self.shared.changed.wait(pending));
            }
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Pending>> {
        self.shared
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Linux's power backend: logind over the system bus.
#[cfg(target_os = "linux")]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Logind;

#[cfg(target_os = "linux")]
impl PowerBackend for Logind {
    fn perform(&self, action: PowerActionKind) -> Result<()> {
        let connection = zbus::blocking::Connection::system()
            .map_err(|error| Error::new(format!("cannot reach the system D-Bus: {error}")))?;
        let method = logind_method(action);
        connection
            .call_method(
                Some("org.freedesktop.login1"),
                "/org/freedesktop/login1",
                Some("org.freedesktop.login1.Manager"),
                method,
                // `interactive = false`: the polkit rule grants exactly these
                // three actions to the daemon's user, so no prompt is possible.
                &(false,),
            )
            .map_err(|error| {
                Error::new(format!(
                    "logind refused {method} for {}: {error}",
                    action.as_str()
                ))
            })?;
        Ok(())
    }
}

/// Every non-Linux build: state works, power does not.
#[cfg(not(target_os = "linux"))]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Unavailable;

#[cfg(not(target_os = "linux"))]
impl PowerBackend for Unavailable {
    fn perform(&self, action: PowerActionKind) -> Result<()> {
        Err(Error::new(format!(
            "this build has no power backend for {}; {} needs Linux and logind",
            std::env::consts::OS,
            action.as_str()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn stop_before_runner_wait_does_not_fire_due_action() {
        let (controller, backend, _) = controller();
        controller.request(PowerActionKind::Shutdown, 0).unwrap();
        controller.stop();
        controller.run();
        assert_eq!(controller.fire_due(), None);
        assert!(backend.called().is_empty());
    }

    #[test]
    fn stop_synchronizes_with_the_wait_mutex() {
        let (controller, backend, _) = controller();
        controller.request(PowerActionKind::Shutdown, 0).unwrap();
        let guard = controller.lock();
        let (started, received) = std::sync::mpsc::channel();
        let stop = controller.clone();
        let thread = std::thread::spawn(move || {
            started.send(()).unwrap();
            stop.stop();
        });
        received.recv().unwrap();
        assert!(!controller.shared.stopped.load(Ordering::SeqCst));
        drop(guard);
        thread.join().unwrap();
        assert!(controller.shared.stopped.load(Ordering::SeqCst));
        controller.wait_for_change();
        assert_eq!(controller.fire_due(), None);
        assert!(backend.called().is_empty());
    }

    #[derive(Debug, Default)]
    struct RecordingBackend(Mutex<Vec<PowerActionKind>>);

    impl PowerBackend for RecordingBackend {
        fn perform(&self, action: PowerActionKind) -> Result<()> {
            self.0.lock().unwrap().push(action);
            Ok(())
        }
    }

    impl RecordingBackend {
        fn called(&self) -> Vec<PowerActionKind> {
            self.0.lock().unwrap().clone()
        }
    }

    #[derive(Debug, Default)]
    struct FakeClock(Mutex<u64>);

    impl FakeClock {
        fn at(&self, millis: u64) {
            *self.0.lock().unwrap() = millis;
        }
        fn advance(&self, millis: u64) {
            *self.0.lock().unwrap() += millis;
        }
    }

    impl Clock for FakeClock {
        fn now_unix_ms(&self) -> u64 {
            *self.0.lock().unwrap()
        }
    }

    fn controller() -> (PowerController, Arc<RecordingBackend>, Arc<FakeClock>) {
        let backend = Arc::new(RecordingBackend::default());
        let clock = Arc::new(FakeClock::default());
        clock.at(1_000);
        let performer: Arc<dyn PowerBackend> = backend.clone();
        let time: Arc<dyn Clock> = clock.clone();
        (PowerController::new(performer, time), backend, clock)
    }

    #[test]
    fn backend_does_not_hold_pending_lock() {
        struct Blocking(
            std::sync::mpsc::Sender<()>,
            Mutex<std::sync::mpsc::Receiver<()>>,
        );
        impl PowerBackend for Blocking {
            fn perform(&self, _: PowerActionKind) -> Result<()> {
                self.0.send(()).unwrap();
                self.1.lock().unwrap().recv().unwrap();
                Ok(())
            }
        }
        let (entered, wait) = std::sync::mpsc::channel();
        let (release, resume) = std::sync::mpsc::channel();
        let controller = PowerController::new(
            Arc::new(Blocking(entered, Mutex::new(resume))),
            Arc::new(SystemClock),
        );
        controller.request(PowerActionKind::Shutdown, 0).unwrap();
        let worker = controller.clone();
        let thread = std::thread::spawn(move || worker.fire_due());
        wait.recv().unwrap();
        let check = controller.clone();
        let (done, result) = std::sync::mpsc::channel();
        let probe = std::thread::spawn(move || {
            done.send((check.pending(), check.cancel())).unwrap();
        });
        let observed = result.recv_timeout(Duration::from_secs(1));
        release.send(()).unwrap();
        thread.join().unwrap();
        probe.join().unwrap();
        assert_eq!(observed.unwrap(), (None, false));
    }

    #[test]
    fn a_countdown_fires_once_when_its_deadline_passes() {
        let (controller, backend, clock) = controller();
        let pending = controller.request(PowerActionKind::Sleep, 15).unwrap();
        assert_eq!(pending.deadline_unix_ms, 16_000);
        assert_eq!(controller.pending(), Some(pending));
        assert_eq!(controller.fire_due(), None);
        assert!(backend.called().is_empty());

        clock.advance(15_000);
        assert_eq!(controller.fire_due(), Some(PowerActionKind::Sleep));
        assert_eq!(backend.called(), vec![PowerActionKind::Sleep]);
        assert_eq!(controller.pending(), None);
        // Firing is once: the deadline is gone with the action.
        assert_eq!(controller.fire_due(), None);
        assert_eq!(backend.called(), vec![PowerActionKind::Sleep]);
    }

    #[test]
    fn cancel_before_the_deadline_means_the_backend_is_never_called() {
        let (controller, backend, clock) = controller();
        controller.request(PowerActionKind::Restart, 60).unwrap();
        clock.advance(59_000);
        assert!(controller.cancel());
        assert_eq!(controller.pending(), None);
        clock.advance(60_000);
        assert_eq!(controller.fire_due(), None);
        assert!(backend.called().is_empty());
        assert!(!controller.cancel(), "nothing left to cancel");
    }

    #[test]
    fn a_second_action_is_refused_while_one_is_pending() {
        let (controller, backend, clock) = controller();
        let first = controller.request(PowerActionKind::Sleep, 15).unwrap();
        assert_eq!(
            controller.request(PowerActionKind::Shutdown, 15),
            Err(RequestError::Pending(first))
        );
        clock.advance(15_000);
        controller.fire_due();
        assert_eq!(backend.called(), vec![PowerActionKind::Sleep]);
        // Once it fired, a new action is accepted.
        controller.request(PowerActionKind::Shutdown, 15).unwrap();
    }

    #[test]
    fn delays_are_bounded_and_zero_is_immediate() {
        let (controller, backend, clock) = controller();
        assert_eq!(
            controller.request(PowerActionKind::Sleep, MAX_POWER_DELAY_S + 1),
            Err(RequestError::DelayTooLong {
                limit: MAX_POWER_DELAY_S
            })
        );
        // Zero means the countdown is already over: the next wake fires it.
        let immediate = controller.request(PowerActionKind::Shutdown, 0).unwrap();
        assert_eq!(immediate.deadline_unix_ms, clock.now_unix_ms());
        assert_eq!(controller.fire_due(), Some(PowerActionKind::Shutdown));
        assert_eq!(backend.called(), vec![PowerActionKind::Shutdown]);
    }

    #[test]
    fn only_the_three_actions_have_a_logind_method() {
        let methods: Vec<&str> = [
            PowerActionKind::Sleep,
            PowerActionKind::Restart,
            PowerActionKind::Shutdown,
        ]
        .into_iter()
        .map(logind_method)
        .collect();
        assert_eq!(methods, vec!["Suspend", "Reboot", "PowerOff"]);
    }

    #[test]
    fn the_runner_fires_a_short_countdown_on_a_real_clock() {
        let backend = Arc::new(RecordingBackend::default());
        let performer: Arc<dyn PowerBackend> = backend.clone();
        let controller = PowerController::new(performer, Arc::new(SystemClock));

        controller.request(PowerActionKind::Sleep, 0).unwrap();
        let runner = controller.start().unwrap();
        let mut fired = Vec::new();
        for _ in 0..200 {
            fired = backend.called();
            if !fired.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        controller.stop();
        runner.join().unwrap();
        assert_eq!(fired, vec![PowerActionKind::Sleep]);
        assert_eq!(controller.pending(), None);
    }

    #[test]
    fn stopping_drops_a_pending_action_instead_of_firing_it() {
        let (controller, backend, _clock) = controller();
        controller.request(PowerActionKind::Shutdown, 15).unwrap();
        let runner = controller.start().unwrap();
        controller.stop();
        runner.join().unwrap();
        assert_eq!(
            controller.pending(),
            Some(Pending {
                action: PowerActionKind::Shutdown,
                deadline_unix_ms: 16_000,
            })
        );
        assert!(backend.called().is_empty());
    }
}
