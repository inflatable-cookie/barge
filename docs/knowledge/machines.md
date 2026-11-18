# Machines

Status: design outline approved by Tom, 2026-11-18. Build steps 1 and 2 are
complete; later steps remain unbuilt.

Barge manages the machines it distributes resources across. The first is the
Bazzite PC: wake it, see its state, and sleep, restart or shut it down. This
replaces MoonDeck Buddy, which is no longer maintained, and Tom's MoonDeck
Remote menu-bar app ([vision](vision.md#direction)).

## Parts

- **`barge` CLI on the Mac:** `barge machine list|status|wake|sleep|restart|shutdown|cancel`.
  Wake-on-LAN is a client operation and needs nothing running on the PC.
- **`barged` on the PC:** a small Rust daemon. It reports state and performs
  power actions. On Bazzite it runs as a systemd user unit and asks logind over
  D-Bus for sleep, restart and shutdown, with a polkit rule granting only those
  actions. It never runs a shell command it was sent. It binds one configured
  address — the machine's Tailscale address — and answers only nodes on the
  allowlist in its config. [Install guide](../install-barged.md).
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

- HTTP with JSON over the tailnet. `barged` binds only the single address in
  its config — the machine's Tailscale address — so the network decides who can
  reach it. It refuses a wildcard bind.
- Authorisation: Tailscale identifies the caller's node. `barged` asks
  tailscaled's local API (`whois`) for the peer's node and accepts only nodes on
  an allowlist written at pairing — by stable node ID or MagicDNS name — so no
  PIN flow is needed. A paired client stores nothing secret beyond its tailnet
  identity. An unidentifiable caller and a node off the allowlist are both
  refused with `403`, before an endpoint runs.
- Endpoints: `GET /v1/state`, `POST /v1/power` (`{action, delay_s}`),
  `POST /v1/power/cancel`. Every response carries a `schema` field, under the
  same contract as the CLI's JSON; a non-2xx response is a
  `barge.daemon-error/1` document. State reports the action counting down and
  its deadline when one is pending, and a second action is refused `409` until
  that countdown fires or is cancelled. The daemon's default port is `9777`, and
  a registered machine records its own `daemon_port`.
- Power never names a method: a request carries one of sleep, restart or
  shutdown, and the daemon maps that closed set onto logind's `Suspend`,
  `Reboot` and `PowerOff`. Countdowns are bounded at one hour.
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

1. **Built:** machine registry, Wake-on-LAN and protocol types in
   `barge-machine`; `barge machine add|list|remove|wake`.
2. **Built:** `barged`: state and power with countdown and cancel, the
   allowlist and Tailscale `whois`, logind over D-Bus, the client in
   `barge-machine`, `barge machine status|sleep|restart|shutdown|cancel`, the
   systemd unit and polkit rule in `packaging/barged/`, and the
   [install guide](../install-barged.md) for Bazzite.
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
