# Machines

Status: design outline approved by Tom, 2026-11-18. Not built yet; see the
build order.

Barge manages the machines it distributes resources across. The first is the
Bazzite PC: wake it, see its state, and sleep, restart or shut it down. This
replaces MoonDeck Buddy, which is no longer maintained, and Tom's MoonDeck
Remote menu-bar app ([vision](vision.md#direction)).

## Parts

- **`barge` CLI on the Mac:** `barge machine list|status|wake|sleep|restart|shutdown|cancel`.
  Wake-on-LAN is a client operation and needs nothing running on the PC.
- **`barged` on the PC:** a small Rust daemon. It reports state and performs
  power actions. On Bazzite it runs as a systemd service and asks logind over
  D-Bus for sleep, restart and shutdown, with a polkit rule granting only those
  actions. It never runs a shell command it was sent.
- **Barge UI:** a fresh Tauri app (Tom, 2026-11-18). Machine control comes
  first. Storage, worktrees and resources join later in the same app.

The storage library stays daemon-free: Oxide never depends on `barged` or the UI.

## Behaviour

These carry over from MoonDeck Remote, which is a reference for behaviour, not
for visual design:

- Sleep starts a cancellable countdown, 15 seconds by default.
- Restart and shutdown need explicit confirmation, then the same countdown.
- A pending power action can be cancelled until it fires.
- An unreachable daemon is shown as unreachable, never as asleep. State shown
  is what was observed, with its age.
- Wake works while the daemon is down, using the stored MAC and broadcast
  address.
- Launching Moonlight against the PC is a convenience action in the UI.

## Protocol and trust

- HTTP with JSON over the tailnet. `barged` listens only on its Tailscale
  address, so the network decides who can reach it.
- Authorisation: Tailscale identifies the caller's node and user. `barged`
  accepts only nodes on an allowlist written at pairing, so no PIN flow is
  needed. A paired client stores nothing secret beyond its tailnet identity.
- Endpoints: `GET /v1/state`, `POST /v1/power` (`{action, delay_s}`),
  `POST /v1/power/cancel`. Every response carries a `schema` field, under the
  same contract as the CLI's JSON.
- Wake-on-LAN goes out on the LAN broadcast. Waking over Tailscale from
  outside the LAN is out of scope until a relay exists, for example another
  always-on device.

## UI

Barge's UI does much more than MoonDeck Remote, so its information
architecture is designed before it's built (Tom: "much more professional and
carefully laid out"). Its main areas, eventually:

- **Machines:** this Mac and the PC: state, power, and later load and
  resources.
- **Storage:** volumes, layout classes, space, bindings and doctor findings.
- **Worktrees:** pools, leases, warm state.
- **Activity:** recent actions and their results.

The first release ships Machines only, inside a layout that already has room
for the rest. A menu-bar entry keeps the daily power controls one click away.
The design pass produces the layout and visual system, reviewed by Tom before
the build brief.

## Repository layout

- `crates/barge-machine`: machine registry, Wake-on-LAN, protocol types and
  the client, shared by the CLI and UI.
- `crates/barged`: the daemon (Linux first).
- `apps/barge-ui`: the Tauri app.

## Build order

1. Machine registry, Wake-on-LAN and protocol types; `barge machine wake`.
2. `barged`: state and power with countdown and cancel; systemd unit and
   polkit rule; an install guide for Bazzite.
3. UI design pass: information architecture and visual system, for Tom's
   review.
4. The Tauri app: Machines plus the menu-bar entry.
5. Retire MoonDeck Buddy on the PC. This changes host software, so it needs
   Tom's go-ahead. The Steam Deck needs a `barged` client first, or loses
   remote control until it has one.

## MoonDeck Buddy scope

Tom used MoonDeck Buddy on the Steam Deck only for remote control of the PC
(2026-11-18), so `barged` replaces it entirely. No game-launch features carry
over. The Steam Deck becomes a client of `barged` once a client exists for it.
