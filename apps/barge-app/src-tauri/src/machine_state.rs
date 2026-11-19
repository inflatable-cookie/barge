//! The one state model for a registered machine, shared by the window and the
//! native menu.
//!
//! A state is a pure function of what Barge last saw and what Barge last sent:
//! the daemon's last observation and when Barge received it, whether the daemon
//! answers now, and the last power action Barge sent. Nothing here reads a clock
//! or the network; callers pass `now`. Clocks: `seen_at_ms` and the `*_ms`
//! action times are on this Mac's clock. A daemon deadline is converted to this
//! clock from its own observation time, so a skewed PC clock cannot move a
//! countdown.

use barge_machine::{PowerActionKind, PowerState, State};
use serde::Serialize;

/// Each waiting state ends at its target or at its timeout, measured from when
/// the action was sent. Restart passes through unreachable, so it gets longest.
pub const WAKE_TIMEOUT_MS: u64 = 120_000;
pub const SLEEP_TIMEOUT_MS: u64 = 60_000;
pub const RESTART_TIMEOUT_MS: u64 = 180_000;

/// Poll interval while a machine is waiting on an action.
pub const FAST_POLL_MS: u64 = 2_000;

/// What Barge sent to a machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sent {
    Wake,
    Power(PowerActionKind),
}

/// The last action Barge sent, and what has been seen since.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LastAction {
    pub sent: Sent,
    /// When the daemon accepted it, on this Mac's clock.
    pub sent_at_ms: u64,
    /// When a power action fires: `sent_at_ms` plus its delay. A wake has no
    /// countdown, so this is `sent_at_ms`.
    pub fires_at_ms: u64,
    /// Set when a failed poll began after the action fired: the machine dropped
    /// off during a restart.
    pub dropped: bool,
}

impl LastAction {
    pub fn wake(sent_at_ms: u64) -> Self {
        Self {
            sent: Sent::Wake,
            sent_at_ms,
            fires_at_ms: sent_at_ms,
            dropped: false,
        }
    }

    pub fn power(kind: PowerActionKind, sent_at_ms: u64, delay_s: u32) -> Self {
        Self {
            sent: Sent::Power(kind),
            sent_at_ms,
            fires_at_ms: sent_at_ms.saturating_add(u64::from(delay_s) * 1000),
            dropped: false,
        }
    }
}

/// Everything the state depends on.
pub struct Input<'a> {
    pub now_ms: u64,
    /// Whether the daemon answered the current poll.
    pub reachable: bool,
    /// The last successful observation. Older than `now_ms` when unreachable.
    pub observation: Option<&'a State>,
    /// When Barge received `observation`, on this Mac's clock.
    pub seen_at_ms: Option<u64>,
    pub last_action: Option<LastAction>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Offline,
    Checking,
    Waking,
    Awake,
    Pending,
    GoingDown,
    Restarting,
    DidNotWake,
    StillOn,
    Unknown,
}

/// An action the operator can take now.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Offer {
    Wake,
    Sleep,
    Restart,
    Shutdown,
    Cancel,
    Moonlight,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Status {
    pub phase: Phase,
    /// The power action in flight, for a countdown or a power phase.
    pub action: Option<PowerActionKind>,
    /// When a countdown fires, on this Mac's clock.
    pub deadline_unix_ms: Option<u64>,
    /// When Barge last saw the machine, shown only while it is unreachable.
    pub last_seen_unix_ms: Option<u64>,
    /// The actions the operator can take in this state, and nothing else.
    pub offers: Vec<Offer>,
    /// Whether the app should poll this machine fast until the state settles.
    pub waiting: bool,
}

const AWAKE_OFFERS: [Offer; 4] = [
    Offer::Sleep,
    Offer::Restart,
    Offer::Shutdown,
    Offer::Moonlight,
];

pub fn status(input: &Input) -> Status {
    let now = input.now_ms;
    let seen = input.seen_at_ms.unwrap_or(now);
    if let Some(state) = input.observation {
        if let Some(pending) = state.pending {
            // The daemon's deadline is on its own clock; keep only its distance
            // from the observation, and place it on this Mac's clock at receipt.
            let deadline = seen.saturating_add(
                pending
                    .deadline_unix_ms
                    .saturating_sub(state.observed_at_unix_ms),
            );
            if input.reachable || now < deadline {
                let app_sent = input
                    .last_action
                    .is_some_and(|action| matches!(action.sent, Sent::Power(_)));
                return countdown(pending.action, deadline, input, app_sent);
            }
        }
    }
    let Some(action) = input.last_action else {
        return settled(input);
    };
    if let Sent::Power(kind) = action.sent {
        // Sent, but no observation since shows the countdown yet.
        let seen_since = input.seen_at_ms.is_some_and(|at| at >= action.sent_at_ms);
        if now < action.fires_at_ms && !seen_since {
            return countdown(kind, action.fires_at_ms, input, true);
        }
    }
    let elapsed = now.saturating_sub(action.sent_at_ms);
    let awake = input.reachable && is_awake(input.observation);
    match action.sent {
        Sent::Wake => {
            if awake {
                settled(input)
            } else if elapsed < WAKE_TIMEOUT_MS {
                waiting(Phase::Waking, input)
            } else if input.reachable {
                settled(input)
            } else {
                plain(Phase::DidNotWake, vec![Offer::Wake], input)
            }
        }
        Sent::Power(PowerActionKind::Restart) => {
            if now < action.fires_at_ms || (awake && action.dropped) {
                settled(input)
            } else if elapsed < RESTART_TIMEOUT_MS {
                power(Phase::Restarting, PowerActionKind::Restart, input)
            } else if !input.reachable {
                plain(Phase::DidNotWake, vec![Offer::Wake], input)
            } else if action.dropped {
                settled(input)
            } else {
                plain(Phase::StillOn, AWAKE_OFFERS.to_vec(), input)
            }
        }
        Sent::Power(kind) => {
            if now < action.fires_at_ms || !input.reachable {
                settled(input)
            } else if elapsed < SLEEP_TIMEOUT_MS {
                power(Phase::GoingDown, kind, input)
            } else {
                plain(Phase::StillOn, AWAKE_OFFERS.to_vec(), input)
            }
        }
    }
}

/// Whether what Barge has seen now completes an action, so its record can go.
/// A sleep or shutdown completes once it has fired and the machine is down; a
/// restart once it has dropped and is back awake; a wake once it is awake.
pub fn completes(
    action: &LastAction,
    now_ms: u64,
    reachable: bool,
    observation: Option<&State>,
) -> bool {
    let awake = reachable && is_awake(observation);
    match action.sent {
        Sent::Wake => awake,
        Sent::Power(PowerActionKind::Restart) => action.dropped && awake,
        Sent::Power(_) => !reachable && now_ms >= action.fires_at_ms,
    }
}

/// The state's text, as the menu shows it. The window words the same states;
/// keep the two in step.
pub fn label(status: &Status, now_ms: u64) -> String {
    match status.phase {
        Phase::Offline => "Offline".into(),
        Phase::Checking => "Checking…".into(),
        Phase::Waking => "Waking…".into(),
        Phase::Awake => "Awake".into(),
        Phase::Unknown => "Unknown".into(),
        Phase::DidNotWake => "Didn't wake".into(),
        Phase::StillOn => "Still on".into(),
        Phase::Restarting => "Restarting…".into(),
        Phase::GoingDown => match status.action {
            Some(PowerActionKind::Shutdown) => "Shutting down…".into(),
            _ => "Going to sleep…".into(),
        },
        Phase::Pending => {
            let verb = match status.action {
                Some(PowerActionKind::Restart) => "Restarting",
                Some(PowerActionKind::Shutdown) => "Shutting down",
                _ => "Sleeping",
            };
            let seconds = status
                .deadline_unix_ms
                .map_or(0, |deadline| countdown_seconds(deadline, now_ms));
            format!("{verb} in {seconds} s")
        }
    }
}

/// Whole seconds left, rounded up, so a countdown never reads 0 before it fires.
pub fn countdown_seconds(deadline_ms: u64, now_ms: u64) -> u64 {
    deadline_ms.saturating_sub(now_ms).div_ceil(1000)
}

fn is_awake(observation: Option<&State>) -> bool {
    observation.is_some_and(|state| state.power == PowerState::Awake)
}

fn settled(input: &Input) -> Status {
    if !input.reachable {
        // Never observed, and nothing sent: the registry names the machine but
        // no poll has reported back yet. It offers Wake, which needs no daemon.
        if input.observation.is_none() && input.last_action.is_none() {
            return plain(Phase::Checking, vec![Offer::Wake], input);
        }
        plain(Phase::Offline, vec![Offer::Wake], input)
    } else if is_awake(input.observation) {
        plain(Phase::Awake, AWAKE_OFFERS.to_vec(), input)
    } else {
        plain(Phase::Unknown, vec![Offer::Moonlight], input)
    }
}

fn countdown(kind: PowerActionKind, deadline: u64, input: &Input, app_sent: bool) -> Status {
    Status {
        phase: Phase::Pending,
        action: Some(kind),
        deadline_unix_ms: Some(deadline),
        last_seen_unix_ms: last_seen(input),
        offers: vec![Offer::Cancel],
        waiting: app_sent,
    }
}

fn power(phase: Phase, kind: PowerActionKind, input: &Input) -> Status {
    Status {
        action: Some(kind),
        waiting: true,
        ..plain(phase, Vec::new(), input)
    }
}

fn waiting(phase: Phase, input: &Input) -> Status {
    Status {
        waiting: true,
        ..plain(phase, Vec::new(), input)
    }
}

fn plain(phase: Phase, offers: Vec<Offer>, input: &Input) -> Status {
    Status {
        phase,
        action: None,
        deadline_unix_ms: None,
        last_seen_unix_ms: last_seen(input),
        offers,
        waiting: false,
    }
}

fn last_seen(input: &Input) -> Option<u64> {
    if input.reachable {
        None
    } else {
        input.seen_at_ms
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use barge_machine::Pending;

    const T0: u64 = 1_000_000;

    fn awake(observed: u64) -> State {
        State::new(PowerState::Awake, observed)
    }

    fn countdown_state(action: PowerActionKind, observed: u64, deadline: u64) -> State {
        awake(observed).with_pending(Some(Pending {
            action,
            deadline_unix_ms: deadline,
        }))
    }

    fn input<'a>(
        now: u64,
        reachable: bool,
        observation: Option<&'a State>,
        action: Option<LastAction>,
    ) -> Input<'a> {
        Input {
            now_ms: now,
            reachable,
            observation,
            seen_at_ms: observation.map(|_| T0),
            last_action: action,
        }
    }

    fn phase(status: &Status) -> Phase {
        status.phase
    }

    #[test]
    fn offline_offers_only_wake_and_keeps_the_stale_age() {
        let state = awake(T0);
        let status = status(&input(T0 + 300_000, false, Some(&state), None));
        assert_eq!(status.phase, Phase::Offline);
        assert_eq!(status.offers, vec![Offer::Wake]);
        assert_eq!(status.last_seen_unix_ms, Some(T0));
        assert!(!status.waiting);
    }

    #[test]
    fn offline_without_any_observation_offers_wake_and_claims_nothing() {
        let status = status(&Input {
            now_ms: T0,
            reachable: false,
            observation: None,
            seen_at_ms: None,
            last_action: None,
        });
        assert_eq!(status.phase, Phase::Checking);
        assert_eq!(status.offers, vec![Offer::Wake]);
        assert_eq!(status.last_seen_unix_ms, None);
        assert_eq!(label(&status, T0), "Checking…");
        assert!(!status.waiting);
    }

    #[test]
    fn awake_offers_the_awake_set_and_no_age() {
        let state = awake(T0);
        let status = status(&input(T0 + 5_000, true, Some(&state), None));
        assert_eq!(status.phase, Phase::Awake);
        assert_eq!(
            status.offers,
            vec![
                Offer::Sleep,
                Offer::Restart,
                Offer::Shutdown,
                Offer::Moonlight
            ]
        );
        assert_eq!(status.last_seen_unix_ms, None);
    }

    #[test]
    fn reachable_but_not_awake_is_unknown_with_moonlight_only() {
        let state = State::new(PowerState::Unknown, T0);
        let status = status(&input(T0, true, Some(&state), None));
        assert_eq!(status.phase, Phase::Unknown);
        assert_eq!(status.offers, vec![Offer::Moonlight]);
    }

    #[test]
    fn wake_waits_until_the_machine_is_awake_or_the_timeout_passes() {
        let sent = LastAction::wake(T0);
        let waking = status(&input(T0 + 2_000, false, None, Some(sent)));
        assert_eq!(waking.phase, Phase::Waking);
        assert!(waking.waiting);
        assert!(waking.offers.is_empty());

        let state = awake(T0 + 60_000);
        let arrived = status(&Input {
            now_ms: T0 + 60_000,
            reachable: true,
            observation: Some(&state),
            seen_at_ms: Some(T0 + 60_000),
            last_action: Some(sent),
        });
        assert_eq!(arrived.phase, Phase::Awake);
        assert!(!arrived.waiting);

        let timed_out = status(&input(T0 + WAKE_TIMEOUT_MS, false, None, Some(sent)));
        assert_eq!(timed_out.phase, Phase::DidNotWake);
        assert_eq!(timed_out.offers, vec![Offer::Wake]);
    }

    #[test]
    fn a_wake_that_times_out_with_the_machine_answering_shows_its_state() {
        let sent = LastAction::wake(T0);
        let state = State::new(PowerState::Unknown, T0);
        let status = status(&Input {
            now_ms: T0 + WAKE_TIMEOUT_MS,
            reachable: true,
            observation: Some(&state),
            seen_at_ms: Some(T0 + WAKE_TIMEOUT_MS),
            last_action: Some(sent),
        });
        assert_eq!(status.phase, Phase::Unknown);
    }

    #[test]
    fn countdown_from_the_daemon_is_pending_with_cancel() {
        let state = countdown_state(PowerActionKind::Sleep, T0, T0 + 12_000);
        let status = status(&input(T0, true, Some(&state), None));
        assert_eq!(status.phase, Phase::Pending);
        assert_eq!(status.action, Some(PowerActionKind::Sleep));
        assert_eq!(status.offers, vec![Offer::Cancel]);
        assert_eq!(label(&status, T0), "Sleeping in 12 s");
        assert!(
            !status.waiting,
            "a countdown Barge did not send is not fast-polled"
        );
    }

    #[test]
    fn the_daemon_deadline_is_placed_on_this_macs_clock_from_receipt() {
        // The PC's clock reads 9 s ahead. The countdown has 12 s left on the
        // daemon's clock, so it has 12 s left here too.
        let daemon_now = T0 + 9_000;
        let state = countdown_state(PowerActionKind::Restart, daemon_now, daemon_now + 12_000);
        let status = status(&input(T0, true, Some(&state), None));
        assert_eq!(label(&status, T0), "Restarting in 12 s");
    }

    #[test]
    fn a_countdown_cannot_be_cancelled_through_an_unreachable_machine_past_its_deadline() {
        let state = countdown_state(PowerActionKind::Sleep, T0, T0 + 12_000);
        let past = status(&input(T0 + 13_000, false, Some(&state), None));
        assert_eq!(past.phase, Phase::Offline);
        let before = status(&input(T0 + 1_000, false, Some(&state), None));
        assert_eq!(before.phase, Phase::Pending);
        assert_eq!(before.offers, vec![Offer::Cancel]);
        assert_eq!(before.last_seen_unix_ms, Some(T0));
    }

    #[test]
    fn a_sent_sleep_shows_its_countdown_before_the_daemon_reports_one() {
        let sent = LastAction::power(PowerActionKind::Sleep, T0, 15);
        let status = status(&Input {
            now_ms: T0 + 100,
            reachable: true,
            observation: None,
            seen_at_ms: None,
            last_action: Some(sent),
        });
        assert_eq!(status.phase, Phase::Pending);
        assert!(status.waiting);
        assert_eq!(label(&status, T0 + 100), "Sleeping in 15 s");
    }

    #[test]
    fn a_countdown_cancelled_outside_barge_is_settled_by_the_next_observation() {
        let sent = LastAction::power(PowerActionKind::Sleep, T0, 15);
        let state = awake(T0 + 500);
        let status = status(&Input {
            now_ms: T0 + 1_000,
            reachable: true,
            observation: Some(&state),
            seen_at_ms: Some(T0 + 500),
            last_action: Some(sent),
        });
        assert_eq!(status.phase, Phase::Awake);
    }

    #[test]
    fn a_fired_sleep_reads_going_down_until_the_machine_drops() {
        let sent = LastAction::power(PowerActionKind::Sleep, T0, 15);
        let state = awake(T0 + 16_000);
        let going = status(&Input {
            now_ms: T0 + 16_000,
            reachable: true,
            observation: Some(&state),
            seen_at_ms: Some(T0 + 16_000),
            last_action: Some(sent),
        });
        assert_eq!(going.phase, Phase::GoingDown);
        assert_eq!(label(&going, T0 + 16_000), "Going to sleep…");
        assert!(going.waiting);
        assert!(going.offers.is_empty());

        let down = status(&input(T0 + 20_000, false, Some(&state), Some(sent)));
        assert_eq!(down.phase, Phase::Offline);
        assert_eq!(down.offers, vec![Offer::Wake]);
    }

    #[test]
    fn a_shutdown_that_never_takes_effect_is_still_on_after_its_timeout() {
        let sent = LastAction::power(PowerActionKind::Shutdown, T0, 15);
        let state = awake(T0 + 60_000);
        let status = status(&Input {
            now_ms: T0 + 15_000 + SLEEP_TIMEOUT_MS,
            reachable: true,
            observation: Some(&state),
            seen_at_ms: Some(T0 + 15_000 + SLEEP_TIMEOUT_MS),
            last_action: Some(sent),
        });
        assert_eq!(status.phase, Phase::StillOn);
        assert_eq!(label(&status, T0), "Still on");
        assert_eq!(
            status.offers,
            vec![
                Offer::Sleep,
                Offer::Restart,
                Offer::Shutdown,
                Offer::Moonlight
            ]
        );
        let shutting = status_for(PowerActionKind::Shutdown, T0 + 16_000);
        assert_eq!(label(&shutting, T0 + 16_000), "Shutting down…");
    }

    fn status_for(kind: PowerActionKind, now: u64) -> Status {
        let sent = LastAction::power(kind, T0, 15);
        let state = awake(now);
        status(&Input {
            now_ms: now,
            reachable: true,
            observation: Some(&state),
            seen_at_ms: Some(now),
            last_action: Some(sent),
        })
    }

    #[test]
    fn restart_passes_through_unreachable_and_finishes_when_the_daemon_answers() {
        let sent = LastAction::power(PowerActionKind::Restart, T0, 15);
        let before_drop = status(&Input {
            now_ms: T0 + 16_000,
            reachable: true,
            observation: Some(&awake(T0 + 16_000)),
            seen_at_ms: Some(T0 + 16_000),
            last_action: Some(sent),
        });
        assert_eq!(before_drop.phase, Phase::Restarting);
        assert!(before_drop.waiting);

        let mut dropped = sent;
        dropped.dropped = true;
        let down = status(&input(T0 + 60_000, false, Some(&awake(T0)), Some(dropped)));
        assert_eq!(down.phase, Phase::Restarting);
        assert_eq!(label(&down, T0), "Restarting…");

        let back = status(&Input {
            now_ms: T0 + 90_000,
            reachable: true,
            observation: Some(&awake(T0 + 90_000)),
            seen_at_ms: Some(T0 + 90_000),
            last_action: Some(dropped),
        });
        assert_eq!(back.phase, Phase::Awake);
        assert!(completes(
            &dropped,
            T0 + 90_000,
            true,
            Some(&awake(T0 + 90_000))
        ));
    }

    #[test]
    fn a_restart_that_never_comes_back_is_didnt_wake_and_offers_wake() {
        let mut sent = LastAction::power(PowerActionKind::Restart, T0, 15);
        sent.dropped = true;
        let timed_out = status(&input(T0 + RESTART_TIMEOUT_MS, false, None, Some(sent)));
        assert_eq!(timed_out.phase, Phase::DidNotWake);
        assert_eq!(timed_out.offers, vec![Offer::Wake]);
    }

    #[test]
    fn a_restart_that_never_drops_is_still_on_after_its_timeout() {
        let sent = LastAction::power(PowerActionKind::Restart, T0, 15);
        let state = awake(T0 + RESTART_TIMEOUT_MS);
        let status = status(&Input {
            now_ms: T0 + RESTART_TIMEOUT_MS,
            reachable: true,
            observation: Some(&state),
            seen_at_ms: Some(T0 + RESTART_TIMEOUT_MS),
            last_action: Some(sent),
        });
        assert_eq!(status.phase, Phase::StillOn);
    }

    #[test]
    fn an_observation_predating_a_send_does_not_settle_the_countdown() {
        let before = awake(T0 - 1_000);
        let sent = LastAction::power(PowerActionKind::Sleep, T0, 15);
        let status = status(&Input {
            now_ms: T0 + 100,
            reachable: true,
            observation: Some(&before),
            seen_at_ms: Some(T0 - 1_000),
            last_action: Some(sent),
        });
        assert_eq!(status.phase, Phase::Pending);
    }

    #[test]
    fn a_stale_observation_is_shown_only_while_unreachable() {
        let state = awake(T0);
        let stale = status(&input(T0 + 600_000, false, Some(&state), None));
        assert_eq!(stale.last_seen_unix_ms, Some(T0));
        let fresh = status(&input(T0 + 600_000, true, Some(&state), None));
        assert_eq!(fresh.last_seen_unix_ms, None);
    }

    #[test]
    fn countdown_rounds_up_and_never_reads_zero_before_it_fires() {
        assert_eq!(countdown_seconds(1_500, 0), 2);
        assert_eq!(countdown_seconds(1_000, 0), 1);
        assert_eq!(countdown_seconds(0, 10), 0);
    }

    #[test]
    fn completes_only_when_the_target_is_seen() {
        let wake = LastAction::wake(T0);
        assert!(!completes(&wake, T0, false, None));
        assert!(completes(&wake, T0, true, Some(&awake(T0))));

        let sleep = LastAction::power(PowerActionKind::Sleep, T0, 15);
        assert!(!completes(&sleep, T0 + 16_000, true, Some(&awake(T0))));
        assert!(!completes(&sleep, T0 + 10_000, false, None));
        assert!(completes(&sleep, T0 + 16_000, false, None));
    }

    #[test]
    fn waiting_follows_the_app_sent_actions_only() {
        let awake_state = awake(T0);
        assert!(!status(&input(T0, true, Some(&awake_state), None)).waiting);
        assert!(status(&input(T0, false, None, Some(LastAction::wake(T0)))).waiting);
        let sent = LastAction::power(PowerActionKind::Sleep, T0, 15);
        assert!(status(&input(T0, true, None, Some(sent))).waiting);
    }

    #[test]
    fn every_state_offers_only_what_it_can_do() {
        let cases: Vec<(Status, Vec<Offer>)> = vec![
            (
                status(&input(T0, false, Some(&awake(T0)), None)),
                vec![Offer::Wake],
            ),
            (
                status(&input(T0, false, None, Some(LastAction::wake(T0)))),
                vec![],
            ),
            (
                status(&input(T0, true, Some(&awake(T0)), None)),
                vec![
                    Offer::Sleep,
                    Offer::Restart,
                    Offer::Shutdown,
                    Offer::Moonlight,
                ],
            ),
            (
                status(&input(
                    T0,
                    true,
                    Some(&countdown_state(PowerActionKind::Sleep, T0, T0 + 9_000)),
                    None,
                )),
                vec![Offer::Cancel],
            ),
        ];
        for (status, offers) in cases {
            assert_eq!(status.offers, offers, "{:?}", status.phase);
        }
        assert_eq!(
            phase(&status(&input(T0, false, None, None))),
            Phase::Checking
        );
    }
}
