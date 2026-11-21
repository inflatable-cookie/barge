//! Machine observations shared by the window and the native tray menu.
//!
//! The store keeps what the last poll saw, when Barge received it, and the last
//! power action Barge sent to each machine. Both surfaces read the same
//! [`MachineView`]s, whose state comes from [`machine_state::status`].

use std::collections::HashSet;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use barge_machine::{
    backfill, follow_addresses, match_registered_with, DiscoveredMachine, Machine, MatchProbe,
    MdnsBrowser, NetworkProbe, PendingPairRequest, Registry, State, UdpMdnsBrowser,
    DEFAULT_DISCOVER_TIMEOUT,
};
use serde::Serialize;

use crate::machine_state::{self, Input, LastAction, Status};
#[cfg(test)]
use crate::notifications::NoopNotifier;
use crate::notifications::PairNotifier;

/// Milliseconds since the Unix epoch on this Mac's clock.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |value| value.as_millis().try_into().unwrap_or(u64::MAX))
}

#[derive(Clone, Debug, Serialize)]
pub struct MachineView {
    pub name: String,
    pub hostname: String,
    pub mac: String,
    pub mode: &'static str,
    pub paired: bool,
    pub address: String,
    pub daemon_port: u16,
    pub reachable: bool,
    pub error: Option<String>,
    /// When Barge received the last successful observation, on this Mac's clock.
    pub seen_at_unix_ms: Option<u64>,
    pub status: Status,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DiscoveredMachineView {
    pub id: String,
    pub name: String,
    pub mode: String,
    pub protocol_version: String,
    pub address: String,
    pub port: u16,
}

impl From<DiscoveredMachine> for DiscoveredMachineView {
    fn from(value: DiscoveredMachine) -> Self {
        Self {
            id: value.id,
            name: value.name,
            mode: value.mode,
            protocol_version: value.protocol_version,
            address: value.address,
            port: value.port,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PendingPairPrompt {
    pub machine_name: String,
    pub request_id: String,
    pub client_name: String,
    pub code: String,
    pub created_at_unix_ms: u64,
    pub expires_at_unix_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PairedClientView {
    pub id: String,
    pub name: String,
    pub paired_at_unix_ms: u64,
    pub this_client: bool,
}

/// Pending pairing requests across all paired machines, with one-shot
/// notification tracking per `request_id`.
#[derive(Default)]
pub struct PendingPairs {
    prompts: Vec<PendingPairPrompt>,
    notified: HashSet<String>,
}

impl PendingPairs {
    /// Replace the active prompts and return only newly observed ones that have
    /// not triggered a native notification yet.
    pub fn sync(&mut self, prompts: Vec<PendingPairPrompt>) -> Vec<PendingPairPrompt> {
        let active: HashSet<String> = prompts
            .iter()
            .map(|prompt| prompt.request_id.clone())
            .collect();
        self.notified.retain(|id| active.contains(id));
        let mut fresh = Vec::new();
        for prompt in &prompts {
            if self.notified.insert(prompt.request_id.clone()) {
                fresh.push(prompt.clone());
            }
        }
        self.prompts = prompts;
        fresh
    }

    /// Drop a prompt once the operator approves or denies it locally.
    pub fn remove(&mut self, request_id: &str) {
        self.prompts
            .retain(|prompt| prompt.request_id != request_id);
    }

    pub fn snapshot(&self) -> Vec<PendingPairPrompt> {
        self.prompts.clone()
    }
}

/// One poll's outcome. `started_at_ms` is when the poll began, so it is compared
/// with the actions sent before it began rather than with the time it finished.
pub struct Poll {
    pub machine: Machine,
    pub lan: bool,
    pub started_at_ms: u64,
    pub result: Result<State, String>,
}

#[derive(Clone)]
struct Entry {
    machine: Machine,
    lan: bool,
    reachable: bool,
    error: Option<String>,
    state: Option<State>,
    seen_at: Option<u64>,
    action: Option<LastAction>,
    /// Set while a click is being sent: the state shows its wait at once.
    sending: bool,
    /// The action that click replaced, put back if the send is refused.
    replaced: Option<LastAction>,
}

impl Entry {
    fn unseen(machine: Machine, lan: bool) -> Self {
        Self {
            machine,
            lan,
            reachable: false,
            error: None,
            state: None,
            seen_at: None,
            action: None,
            sending: false,
            replaced: None,
        }
    }

    fn view(&self, now: u64) -> MachineView {
        let machine = &self.machine;
        let status = machine_state::status(&Input {
            now_ms: now,
            reachable: self.reachable,
            observation: self.state.as_ref(),
            seen_at_ms: self.seen_at,
            last_action: self.action,
        });
        MachineView {
            name: machine.name().to_owned(),
            hostname: machine.hostname().to_owned(),
            mac: machine.mac().unwrap_or("").to_owned(),
            mode: if self.lan { "lan" } else { "tailscale" },
            paired: self.lan,
            address: if self.lan {
                machine.hostname()
            } else {
                machine.daemon_host()
            }
            .to_owned(),
            daemon_port: machine.daemon_port(),
            reachable: self.reachable,
            error: self.error.clone(),
            seen_at_unix_ms: self.seen_at,
            status,
        }
    }
}

/// What Barge knows about each registered machine, in registry order.
#[derive(Default)]
pub struct Observations {
    entries: Vec<Entry>,
}

impl Observations {
    /// List what the registry names without touching the network, so the
    /// window can show every registered machine at once. Entries keep their
    /// last observation and action; a machine the tray has not polled yet
    /// reads as Checking. A machine that is no longer registered drops out.
    pub fn sync_registry(&mut self, machines: Vec<(Machine, bool)>) {
        let mut previous = std::mem::take(&mut self.entries);
        for (machine, lan) in machines {
            let found = previous
                .iter()
                .position(|entry| entry.machine.name() == machine.name());
            let mut entry = match found {
                Some(index) => previous.swap_remove(index),
                None => Entry::unseen(machine.clone(), lan),
            };
            entry.machine = machine;
            entry.lan = lan;
            self.entries.push(entry);
        }
    }

    /// Replace the machines with this poll's, keeping each one's last
    /// observation and action. A machine that is no longer registered drops out.
    pub fn apply(&mut self, polls: Vec<Poll>) {
        let mut previous = std::mem::take(&mut self.entries);
        for poll in polls {
            let found = previous
                .iter()
                .position(|entry| entry.machine.name() == poll.machine.name());
            let mut entry = match found {
                Some(index) => previous.swap_remove(index),
                None => Entry::unseen(poll.machine.clone(), poll.lan),
            };
            entry.machine = poll.machine;
            entry.lan = poll.lan;
            match poll.result {
                Ok(state) => {
                    entry.reachable = true;
                    entry.error = None;
                    entry.state = Some(state);
                    entry.seen_at = Some(poll.started_at_ms);
                }
                Err(error) => {
                    entry.reachable = false;
                    entry.error = Some(error);
                    // A failure after the countdown fired is the machine going down.
                    if let Some(action) = entry.action.as_mut() {
                        if poll.started_at_ms >= action.fires_at_ms {
                            action.dropped = true;
                        }
                    }
                }
            }
            if entry.action.is_some_and(|action| {
                machine_state::completes(
                    &action,
                    poll.started_at_ms,
                    entry.reachable,
                    entry.state.as_ref(),
                )
            }) {
                entry.action = None;
            }
            self.entries.push(entry);
        }
    }

    /// Record the action a click sends before the daemon answers, so the window
    /// and the menu show its wait at once instead of after the send and a poll.
    /// [`Self::sent`] keeps the daemon's own time; [`Self::refused`] puts it back.
    pub fn begin(&mut self, name: &str, action: LastAction) {
        if let Some(entry) = self.entry_mut(name) {
            entry.replaced = entry.action;
            entry.sending = true;
            entry.action = Some(action);
        }
    }

    /// Record an action the daemon accepted, at the time it was accepted.
    pub fn sent(&mut self, name: &str, action: LastAction) {
        if let Some(entry) = self.entry_mut(name) {
            entry.action = Some(action);
            entry.sending = false;
            entry.replaced = None;
        }
    }

    /// The send failed, so nothing was sent: put back what the click replaced.
    pub fn refused(&mut self, name: &str) {
        if let Some(entry) = self.entry_mut(name) {
            if entry.sending {
                entry.action = entry.replaced;
                entry.sending = false;
                entry.replaced = None;
            }
        }
    }

    /// The daemon confirmed a cancel, so the cached countdown is gone too.
    pub fn cancelled(&mut self, name: &str) {
        if let Some(entry) = self.entry_mut(name) {
            entry.action = None;
            entry.sending = false;
            entry.replaced = None;
            if let Some(state) = entry.state.as_mut() {
                state.pending = None;
            }
        }
    }

    pub fn snapshot(&self, now: u64) -> Vec<MachineView> {
        self.entries.iter().map(|entry| entry.view(now)).collect()
    }

    /// Whether any machine is waiting on an action, so it should be polled fast.
    pub fn any_waiting(&self, now: u64) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.view(now).status.waiting)
    }

    fn entry_mut(&mut self, name: &str) -> Option<&mut Entry> {
        self.entries
            .iter_mut()
            .find(|entry| entry.machine.name() == name)
    }
}

/// Use the CLI's LAN key and the same address selection as the CLI.
pub fn client(
    registry: &Registry,
    machine: &Machine,
) -> barge_machine::Result<barge_machine::DaemonClient> {
    match registry.credentials(machine.name())? {
        Some(creds) => Ok(barge_machine::DaemonClient::for_lan_client(
            machine,
            creds.client_id,
            creds.key,
        )),
        None => Ok(barge_machine::DaemonClient::for_machine(machine)),
    }
}

/// List what the registry names without touching the network, so the window
/// shows every registered machine at once. A refresh fills the observations.
pub fn ensure_registered(observations: &Mutex<Observations>) -> Result<(), String> {
    let registry = Registry::discover().map_err(|error| error.to_string())?;
    ensure_registry(&registry, observations)
}

pub fn ensure_registry(
    registry: &Registry,
    observations: &Mutex<Observations>,
) -> Result<(), String> {
    let machines = registry.list().map_err(|error| error.to_string())?;
    let keyed = machines
        .into_iter()
        .map(|machine| {
            let lan = matches!(registry.key(machine.name()), Ok(Some(_)));
            (machine, lan)
        })
        .collect();
    observations
        .lock()
        .map_err(|error| error.to_string())?
        .sync_registry(keyed);
    Ok(())
}

/// Browse `_barged._tcp` on the LAN, update any registered machine whose ID
/// moved to a new address, and return unregistered machines found on the network.
pub fn discover(observations: &Mutex<Observations>) -> Result<Vec<DiscoveredMachineView>, String> {
    let registry = Registry::discover().map_err(|error| error.to_string())?;
    discover_with(&registry, &UdpMdnsBrowser, &NetworkProbe, observations)
}

/// Found machines that no registered record accounts for.
pub fn discover_with(
    registry: &Registry,
    browser: &dyn MdnsBrowser,
    probe: &dyn MatchProbe,
    observations: &Mutex<Observations>,
) -> Result<Vec<DiscoveredMachineView>, String> {
    let found = browser
        .browse(DEFAULT_DISCOVER_TIMEOUT)
        .map_err(|error| error.to_string())?;
    let changed = follow_addresses(registry, &found).map_err(|error| error.to_string())?;
    if !changed.is_empty() {
        ensure_registry(registry, observations)?;
    }
    let registered = registry.list().map_err(|error| error.to_string())?;
    Ok(found
        .into_iter()
        .filter(|item| match_registered_with(item, &registered, probe).is_none())
        .map(DiscoveredMachineView::from)
        .collect())
}

#[cfg(test)]
fn refresh_registry(registry: &Registry, observations: &Mutex<Observations>) -> Result<(), String> {
    let pending = Mutex::new(PendingPairs::default());
    refresh_registry_with(registry, observations, &pending, &NoopNotifier)
}

/// Read every registered machine, poll pending pair requests on reachable
/// paired machines, and trigger native notifications for newly arrived ones.
pub fn refresh_all(
    observations: &Mutex<Observations>,
    pending: &Mutex<PendingPairs>,
    notifier: &dyn PairNotifier,
) -> Result<(), String> {
    let registry = Registry::discover().map_err(|error| error.to_string())?;
    refresh_registry_with(&registry, observations, pending, notifier)
}

pub fn refresh_registry_with(
    registry: &Registry,
    observations: &Mutex<Observations>,
    pending: &Mutex<PendingPairs>,
    notifier: &dyn PairNotifier,
) -> Result<(), String> {
    let machines = registry.list().map_err(|error| error.to_string())?;
    let outcomes = std::thread::scope(|scope| {
        let handles: Vec<_> = machines
            .iter()
            .map(|machine| {
                scope.spawn(move || {
                    let started_at_ms = now_ms();
                    let key = registry.key(machine.name());
                    let lan = matches!(&key, Ok(Some(_)));
                    let daemon = key.and_then(|_| client(registry, machine));
                    let (result, machine_pending) = match daemon {
                        Ok(daemon_client) => {
                            let state = daemon_client.state().map_err(|error| error.to_string());
                            let prompts = if state.is_ok() && lan {
                                daemon_client
                                    .pending_pairs()
                                    .map(|list| {
                                        list.requests
                                            .into_iter()
                                            .map(|req| pending_prompt(machine.name(), req))
                                            .collect()
                                    })
                                    .unwrap_or_default()
                            } else {
                                Vec::new()
                            };
                            (state, prompts)
                        }
                        Err(error) => (Err(error.to_string()), Vec::new()),
                    };
                    let machine = match &result {
                        Ok(state) => reconciled(registry, machine, state),
                        Err(_) => machine.clone(),
                    };
                    (
                        Poll {
                            machine,
                            lan,
                            started_at_ms,
                            result,
                        },
                        machine_pending,
                    )
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().map_err(|_| "Machine check failed".to_owned()))
            .collect::<Result<Vec<_>, _>>()
    })?;
    let mut polls = Vec::with_capacity(outcomes.len());
    let mut prompts = Vec::new();
    for (poll, machine_pending) in outcomes {
        polls.push(poll);
        prompts.extend(machine_pending);
    }
    prompts.sort_by(|a, b| {
        a.created_at_unix_ms
            .cmp(&b.created_at_unix_ms)
            .then_with(|| a.machine_name.cmp(&b.machine_name))
            .then_with(|| a.request_id.cmp(&b.request_id))
    });
    observations
        .lock()
        .map_err(|error| error.to_string())?
        .apply(polls);
    let fresh = pending
        .lock()
        .map_err(|error| error.to_string())?
        .sync(prompts);
    for prompt in fresh {
        notifier.notify(&prompt);
    }
    Ok(())
}

/// The registered record after recording what its daemon reported, so
/// discovery matches it by ID from now on.
fn reconciled(registry: &Registry, machine: &Machine, state: &State) -> Machine {
    match backfill(registry, machine.name(), state) {
        Ok(outcome) => {
            if let Some(reported) = &outcome.id_mismatch {
                eprintln!(
                    "barge: {} reports machine ID {reported}, not the ID it is registered with; the record is unchanged",
                    machine.name()
                );
            }
            outcome.machine
        }
        Err(_) => machine.clone(),
    }
}

fn pending_prompt(machine_name: &str, req: PendingPairRequest) -> PendingPairPrompt {
    PendingPairPrompt {
        machine_name: machine_name.to_owned(),
        request_id: req.request_id,
        client_name: req.client_name,
        code: req.code,
        created_at_unix_ms: req.created_at_unix_ms,
        expires_at_unix_ms: req.expires_at_unix_ms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use barge_machine::{Pending, PowerActionKind, PowerState};

    fn read_http_request(stream: &mut impl std::io::Read) -> std::io::Result<Vec<u8>> {
        let mut request = Vec::new();
        let mut byte = [0_u8; 1];
        while !request.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte)?;
            request.push(byte[0]);
        }
        let headers = String::from_utf8_lossy(&request);
        let length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or(0);
        let body_start = request.len();
        request.resize(body_start + length, 0);
        stream.read_exact(&mut request[body_start..])?;
        Ok(request)
    }

    const T0: u64 = 1_000_000;

    fn pc() -> Machine {
        Machine::new("pc", "pc.local", "00:11:22:33:44:55").unwrap()
    }

    fn poll(started_at_ms: u64, result: Result<State, String>) -> Poll {
        Poll {
            machine: pc(),
            lan: true,
            started_at_ms,
            result,
        }
    }

    fn awake(observed: u64) -> State {
        State::new(PowerState::Awake, observed)
    }

    fn sleep_action(sent_at_ms: u64) -> LastAction {
        LastAction::power(PowerActionKind::Sleep, sent_at_ms, 15)
    }

    #[test]
    fn failed_read_keeps_the_last_observation_and_when_it_was_received() {
        let mut observations = Observations::default();
        observations.apply(vec![poll(T0, Ok(awake(T0)))]);
        observations.apply(vec![poll(T0 + 600_000, Err("connection timed out".into()))]);
        let view = &observations.snapshot(T0 + 600_000)[0];
        assert!(!view.reachable);
        assert_eq!(view.error.as_deref(), Some("connection timed out"));
        assert_eq!(view.seen_at_unix_ms, Some(T0));
        assert_eq!(view.status.phase, machine_state::Phase::Offline);
        assert_eq!(view.status.last_seen_unix_ms, Some(T0));
    }

    #[test]
    fn a_poll_that_began_before_a_send_does_not_settle_its_countdown() {
        let mut observations = Observations::default();
        observations.apply(vec![poll(T0 - 500, Ok(awake(T0 - 500)))]);
        observations.sent("pc", sleep_action(T0));
        // This poll began before the send and finished after it.
        observations.apply(vec![poll(T0 - 100, Ok(awake(T0 - 100)))]);
        let view = &observations.snapshot(T0 + 100)[0];
        assert_eq!(view.status.phase, machine_state::Phase::Pending);
        assert!(view.status.waiting);
    }

    #[test]
    fn a_failed_poll_after_the_fire_drops_a_restart_and_recovery_completes_it() {
        let mut observations = Observations::default();
        observations.apply(vec![poll(T0, Ok(awake(T0)))]);
        observations.sent("pc", LastAction::power(PowerActionKind::Restart, T0, 15));
        observations.apply(vec![poll(T0 + 16_000, Err("refused".into()))]);
        assert_eq!(
            observations.snapshot(T0 + 16_000)[0].status.phase,
            machine_state::Phase::Restarting
        );
        observations.apply(vec![poll(T0 + 90_000, Ok(awake(T0 + 90_000)))]);
        let view = &observations.snapshot(T0 + 90_000)[0];
        assert_eq!(view.status.phase, machine_state::Phase::Awake);
        assert!(!view.status.waiting);
    }

    #[test]
    fn a_failed_poll_during_the_countdown_is_not_a_drop() {
        let mut observations = Observations::default();
        observations.apply(vec![poll(T0, Ok(awake(T0)))]);
        observations.sent("pc", LastAction::power(PowerActionKind::Restart, T0, 15));
        observations.apply(vec![poll(T0 + 5_000, Err("timeout".into()))]);
        observations.apply(vec![poll(T0 + 16_000, Ok(awake(T0 + 16_000)))]);
        // The poll failed before the restart fired, so the machine did not
        // drop during it. After the fire, a reachable, awake machine is still
        // restarting until it has been seen to drop.
        assert_eq!(
            observations.snapshot(T0 + 16_000)[0].status.phase,
            machine_state::Phase::Restarting
        );
    }

    #[test]
    fn cancel_clears_the_cached_countdown_at_once() {
        let mut observations = Observations::default();
        let state = awake(T0).with_pending(Some(Pending {
            action: PowerActionKind::Sleep,
            deadline_unix_ms: T0 + 12_000,
        }));
        observations.apply(vec![poll(T0, Ok(state))]);
        assert_eq!(
            observations.snapshot(T0)[0].status.phase,
            machine_state::Phase::Pending
        );
        observations.cancelled("pc");
        assert_eq!(
            observations.snapshot(T0)[0].status.phase,
            machine_state::Phase::Awake
        );
    }

    #[test]
    fn waiting_is_reported_for_the_store_so_the_poller_can_go_fast() {
        let mut observations = Observations::default();
        observations.apply(vec![poll(T0, Ok(awake(T0)))]);
        assert!(!observations.any_waiting(T0));
        observations.sent("pc", LastAction::wake(T0));
        // Reachable and awake already: the wake has landed, so nothing waits.
        observations.apply(vec![poll(T0 + 1_000, Ok(awake(T0 + 1_000)))]);
        assert!(!observations.any_waiting(T0 + 1_000));
        observations.sent("pc", LastAction::wake(T0 + 2_000));
        observations.apply(vec![poll(T0 + 3_000, Err("timeout".into()))]);
        assert!(observations.any_waiting(T0 + 3_000));
    }

    #[test]
    fn a_click_shows_its_wait_before_the_daemon_answers_and_a_refusal_puts_it_back() {
        let mut observations = Observations::default();
        observations.apply(vec![poll(T0, Ok(awake(T0)))]);
        observations.apply(vec![poll(T0 + 500, Err("timed out".into()))]);
        assert_eq!(
            observations.snapshot(T0 + 1_000)[0].status.phase,
            machine_state::Phase::Offline
        );

        observations.begin("pc", LastAction::wake(T0 + 1_000));
        let waiting = &observations.snapshot(T0 + 1_000)[0];
        assert_eq!(waiting.status.phase, machine_state::Phase::Waking);
        assert!(waiting.status.waiting);

        observations.refused("pc");
        let back = &observations.snapshot(T0 + 2_000)[0];
        assert_eq!(back.status.phase, machine_state::Phase::Offline);
        assert_eq!(back.status.offers, vec![machine_state::Offer::Wake]);
        assert!(!back.status.waiting);
    }

    #[test]
    fn a_clicked_sleep_shows_its_countdown_before_the_daemon_reports_one() {
        let mut observations = Observations::default();
        observations.apply(vec![poll(T0, Ok(awake(T0)))]);
        observations.begin("pc", sleep_action(T0 + 1_000));
        let view = &observations.snapshot(T0 + 1_000)[0];
        assert_eq!(view.status.phase, machine_state::Phase::Pending);
        assert_eq!(view.status.offers, vec![machine_state::Offer::Cancel]);
        assert!(view.status.waiting);

        // The daemon accepted it after its own check, so its time wins.
        observations.sent("pc", sleep_action(T0 + 2_000));
        observations.refused("pc");
        assert_eq!(
            observations.snapshot(T0 + 3_000)[0].status.phase,
            machine_state::Phase::Pending
        );
    }

    #[test]
    fn a_refusal_restores_the_action_the_click_replaced() {
        let mut observations = Observations::default();
        observations.apply(vec![poll(T0, Ok(awake(T0)))]);
        // A sleep that fired and left the machine on: the card says "Still on".
        observations.sent("pc", sleep_action(T0));
        observations.apply(vec![poll(T0 + 80_000, Ok(awake(T0 + 80_000)))]);
        assert_eq!(
            observations.snapshot(T0 + 80_000)[0].status.phase,
            machine_state::Phase::StillOn
        );

        observations.begin(
            "pc",
            LastAction::power(PowerActionKind::Shutdown, T0 + 81_000, 15),
        );
        assert_eq!(
            observations.snapshot(T0 + 81_000)[0].status.phase,
            machine_state::Phase::Pending
        );

        observations.refused("pc");
        assert_eq!(
            observations.snapshot(T0 + 82_000)[0].status.phase,
            machine_state::Phase::StillOn
        );
    }

    #[test]
    fn a_refusal_after_a_late_refresh_leaves_the_observation_alone() {
        let mut observations = Observations::default();
        let pending = awake(T0).with_pending(Some(Pending {
            action: PowerActionKind::Sleep,
            deadline_unix_ms: T0 + 12_000,
        }));
        observations.apply(vec![poll(T0, Ok(pending))]);
        observations.begin("pc", LastAction::wake(T0 + 1_000));
        // A poll taken while the click was in flight lands before the answer.
        observations.apply(vec![poll(T0 + 2_000, Ok(awake(T0 + 2_000)))]);
        observations.refused("pc");
        let view = &observations.snapshot(T0 + 2_000)[0];
        assert_eq!(view.status.phase, machine_state::Phase::Awake);
        assert!(!view.status.waiting);
    }

    #[test]
    fn sync_lists_registered_machines_without_touching_the_network() {
        let temp = barge::testing::TempDir::new("app-machine-sync").unwrap();
        let registry = Registry::open(temp.child("state")).unwrap();
        // The daemon address is closed: any poll would fail, so a snapshot
        // that answers proves no network was touched.
        registry
            .add(
                Machine::new("pc", "127.0.0.1", "00:11:22:33:44:55")
                    .unwrap()
                    .with_daemon_port(1)
                    .unwrap(),
            )
            .unwrap();
        let observations = Mutex::new(Observations::default());
        ensure_registry(&registry, &observations).unwrap();
        let view = &observations.lock().unwrap().snapshot(now_ms())[0];
        assert_eq!(view.name, "pc");
        assert_eq!(view.status.phase, machine_state::Phase::Checking);
        assert_eq!(view.status.offers, vec![machine_state::Offer::Wake]);
        assert_eq!(view.seen_at_unix_ms, None);
    }

    #[test]
    fn sync_keeps_observations_and_drops_removed_machines() {
        let temp = barge::testing::TempDir::new("app-machine-resync").unwrap();
        let registry = Registry::open(temp.child("state")).unwrap();
        registry.add(pc()).unwrap();
        let observations = Mutex::new(Observations::default());
        ensure_registry(&registry, &observations).unwrap();
        observations
            .lock()
            .unwrap()
            .apply(vec![poll(T0, Ok(awake(T0)))]);
        assert_eq!(
            observations.lock().unwrap().snapshot(T0)[0].status.phase,
            machine_state::Phase::Awake
        );
        // A second sync keeps the observation; removing the machine drops it.
        ensure_registry(&registry, &observations).unwrap();
        assert_eq!(
            observations.lock().unwrap().snapshot(T0)[0].status.phase,
            machine_state::Phase::Awake
        );
        registry.remove("pc").unwrap();
        ensure_registry(&registry, &observations).unwrap();
        assert!(observations.lock().unwrap().snapshot(T0).is_empty());
    }

    #[test]
    fn machines_no_longer_registered_drop_out() {
        let mut observations = Observations::default();
        observations.apply(vec![poll(T0, Ok(awake(T0)))]);
        observations.apply(Vec::new());
        assert!(observations.snapshot(T0).is_empty());
    }

    #[test]
    fn empty_and_registered_machines_use_only_the_fixture_registry() {
        use std::io::Write;
        use std::net::TcpListener;

        let temp = barge::testing::TempDir::new("app-machine-poll").unwrap();
        let registry = Registry::open(temp.child("state")).unwrap();
        let observations = Mutex::new(Observations::default());
        refresh_registry(&registry, &observations).unwrap();
        assert!(observations.lock().unwrap().snapshot(T0).is_empty());

        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        registry
            .add(
                Machine::new("pc", "127.0.0.1", "00:11:22:33:44:55")
                    .unwrap()
                    .with_daemon_port(port)
                    .unwrap(),
            )
            .unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let _request = read_http_request(&mut stream).unwrap();
            let body = r#"{"schema":"barge.machine-state/2","power":"awake","observed_at_unix_ms":42,"pending":{"action":"sleep","deadline_unix_ms":15042},"machine_id":"mid-1"}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        });
        refresh_registry(&registry, &observations).unwrap();
        let first = observations.lock().unwrap().snapshot(now_ms());
        assert_eq!(first.len(), 1);
        assert!(first[0].reachable);
        assert_eq!(first[0].status.phase, machine_state::Phase::Pending);
        assert_eq!(first[0].daemon_port, port);
        server.join().unwrap();

        refresh_registry(&registry, &observations).unwrap();
        let failed = observations.lock().unwrap().snapshot(now_ms());
        assert!(!failed[0].reachable);
        assert_eq!(failed[0].seen_at_unix_ms, first[0].seen_at_unix_ms);
    }

    #[test]
    fn discover_filters_registered_machines_and_follows_moved_addresses() {
        struct FakeBrowser(Vec<DiscoveredMachine>);
        impl MdnsBrowser for FakeBrowser {
            fn browse(
                &self,
                _timeout: std::time::Duration,
            ) -> barge_machine::Result<Vec<DiscoveredMachine>> {
                Ok(self.0.clone())
            }
        }

        let temp = barge::testing::TempDir::new("app-discover-follow").unwrap();
        let registry = Registry::open(temp.child("state")).unwrap();
        registry
            .add(
                Machine::discovered("mid-1", "linux-pc", "192.0.2.20", 9777)
                    .unwrap()
                    .with_mac("00:11:22:33:44:55")
                    .unwrap(),
            )
            .unwrap();
        let observations = Mutex::new(Observations::default());
        ensure_registry(&registry, &observations).unwrap();

        let browser = FakeBrowser(vec![
            DiscoveredMachine {
                id: "mid-1".to_string(),
                name: "Linux PC".to_string(),
                mode: "lan".to_string(),
                protocol_version: "1".to_string(),
                address: "192.0.2.99".to_string(),
                port: 9888,
                host: "linux-pc.local".to_string(),
            },
            DiscoveredMachine {
                id: "mid-2".to_string(),
                name: "SteamDeck".to_string(),
                mode: "lan".to_string(),
                protocol_version: "1".to_string(),
                address: "192.0.2.50".to_string(),
                port: 9777,
                host: "steamdeck.local".to_string(),
            },
        ]);

        let found = discover_with(&registry, &browser, &Offline, &observations).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, "mid-2");
        assert_eq!(found[0].name, "SteamDeck");

        let updated = registry.get("linux-pc").unwrap();
        assert_eq!(updated.hostname(), "192.0.2.99");
        assert_eq!(updated.daemon_port(), 9888);

        let views = observations.lock().unwrap().snapshot(now_ms());
        assert_eq!(views[0].address, "192.0.2.99");
        assert_eq!(views[0].daemon_port, 9888);
    }

    #[test]
    fn refresh_polls_pending_pairs_and_notifies_once_per_request() {
        use std::io::Write;
        use std::net::TcpListener;

        #[derive(Default)]
        struct RecordingNotifier(Mutex<Vec<PendingPairPrompt>>);
        impl PairNotifier for RecordingNotifier {
            fn notify(&self, prompt: &PendingPairPrompt) {
                self.0.lock().unwrap().push(prompt.clone());
            }
        }

        let temp = barge::testing::TempDir::new("app-pending-pairs").unwrap();
        let registry = Registry::open(temp.child("state")).unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        registry
            .add(Machine::discovered("mid-1", "linux-pc", "127.0.0.1", port).unwrap())
            .unwrap();
        registry.pair_client("linux-pc", "cid-1", &[7; 32]).unwrap();

        let server = std::thread::spawn(move || {
            for _ in 0..4 {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_http_request(&mut stream).unwrap();
                let text = String::from_utf8_lossy(&request);
                let body = if text.starts_with("GET /v1/state ") {
                    r#"{"schema":"barge.machine-state/2","power":"awake","observed_at_unix_ms":42,"machine_id":"mid-1"}"#
                } else {
                    r#"{"schema":"barge.pair-pending/1","requests":[{"request_id":"req-1","client_name":"Steam Deck","code":"340092","created_at_unix_ms":100,"expires_at_unix_ms":120100}]}"#
                };
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .unwrap();
            }
        });

        let observations = Mutex::new(Observations::default());
        let pending = Mutex::new(PendingPairs::default());
        let notifier = RecordingNotifier::default();

        refresh_registry_with(&registry, &observations, &pending, &notifier).unwrap();
        assert_eq!(pending.lock().unwrap().snapshot().len(), 1);
        assert_eq!(notifier.0.lock().unwrap().len(), 1);
        assert_eq!(notifier.0.lock().unwrap()[0].code, "340092");

        // Second poll sees the same pending request and does not notify again.
        refresh_registry_with(&registry, &observations, &pending, &notifier).unwrap();
        assert_eq!(pending.lock().unwrap().snapshot().len(), 1);
        assert_eq!(notifier.0.lock().unwrap().len(), 1);

        server.join().unwrap();
    }

    /// Answers nothing, so matching uses the record and the browse alone.
    struct Offline;

    impl MatchProbe for Offline {
        fn mac(&self, _found: &DiscoveredMachine) -> Option<String> {
            None
        }

        fn resolve(&self, _hostname: &str) -> Vec<std::net::IpAddr> {
            Vec::new()
        }
    }

    struct Browse(Vec<DiscoveredMachine>);

    impl MdnsBrowser for Browse {
        fn browse(
            &self,
            _timeout: std::time::Duration,
        ) -> barge_machine::Result<Vec<DiscoveredMachine>> {
            Ok(self.0.clone())
        }
    }

    #[test]
    fn discover_does_not_offer_a_registered_machine_recorded_before_discovery() {
        // The registered record predates discovery: no ID, and the hostname is
        // linux-pc.local, not the address the browse found it at.
        let temp = barge::testing::TempDir::new("app-discover-reconcile").unwrap();
        let registry = Registry::open(temp.child("state")).unwrap();
        registry
            .add(Machine::new("linux-pc", "linux-pc.local", "00:11:22:33:44:55").unwrap())
            .unwrap();
        let observations = Mutex::new(Observations::default());
        ensure_registry(&registry, &observations).unwrap();
        let browser = Browse(vec![DiscoveredMachine {
            id: "mid-1".to_string(),
            name: "Linux PC".to_string(),
            mode: "lan".to_string(),
            protocol_version: "1".to_string(),
            address: "192.0.2.55".to_string(),
            port: 9777,
            host: "Linux-PC.local".to_string(),
        }]);

        let found = discover_with(&registry, &browser, &Offline, &observations).unwrap();
        assert!(found.is_empty(), "{found:?}");
        assert_eq!(registry.get("linux-pc").unwrap().id(), None);
    }

    #[test]
    fn refresh_records_the_id_and_mac_a_registered_machine_reports() {
        use std::io::Write;
        use std::net::TcpListener;

        let temp = barge::testing::TempDir::new("app-refresh-backfill").unwrap();
        let registry = Registry::open(temp.child("state")).unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        registry
            .add(
                Machine::new("linux-pc", "127.0.0.1", "00:11:22:33:44:55")
                    .unwrap()
                    .with_daemon_port(port)
                    .unwrap(),
            )
            .unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let _request = read_http_request(&mut stream).unwrap();
            let body = r#"{"schema":"barge.machine-state/2","power":"awake","observed_at_unix_ms":42,"machine_id":"mid-1","mac":"aa:bb:cc:dd:ee:ff","broadcast":"192.0.2.255"}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        });

        let observations = Mutex::new(Observations::default());
        let pending = Mutex::new(PendingPairs::default());
        refresh_registry_with(&registry, &observations, &pending, &NoopNotifier).unwrap();
        server.join().unwrap();

        // The ID is recorded and the MAC it already had is kept.
        let record = registry.get("linux-pc").unwrap();
        assert_eq!(record.id(), Some("mid-1"));
        assert_eq!(record.mac(), Some("00:11:22:33:44:55"));
        assert_eq!(record.broadcast(), std::net::Ipv4Addr::new(192, 0, 2, 255));
        assert_eq!(
            observations.lock().unwrap().snapshot(now_ms())[0].mac,
            "00:11:22:33:44:55"
        );
    }
}
