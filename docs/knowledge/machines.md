# Machines

Status: design outline approved by Tom, 2026-11-18. Build steps 1, 2 and LAN authentication are
complete; later steps remain unbuilt.

Barge manages the machines it distributes resources across. The first is the
Bazzite PC: wake it, see its state, and sleep, restart or shut it down. This
replaces MoonDeck Buddy, which is no longer maintained, and Tom's MoonDeck
Remote menu-bar app ([vision](vision.md#direction)).

## Parts

- **`barge` CLI on the Mac:** the `machine` verbs listed in
  [architecture](architecture.md#cli). Wake-on-LAN is a client operation and
  needs nothing running on the PC. JSON restart and shutdown requests require
  `--yes` rather than prompting.
- **`barged` on the PC:** a small Rust daemon. It reports state and performs
  power actions. On Bazzite it runs as a systemd user unit and asks logind over
  D-Bus for sleep, restart and shutdown, with a polkit rule granting only those
  actions. It never runs a shell command it was sent. It binds one configured address and accepts either allowlisted Tailscale nodes or authenticated LAN clients. [Install guide](../install-barged.md).
- **Barge app:** a menu-bar Tauri app built with Poodle and Longhorn,
  covering machines and storage first ([app](app.md)).

The storage library stays daemon-free: Oxide never depends on `barged` or the UI.

## Behaviour

These carry over from MoonDeck Remote, which is a reference for behaviour, not
for visual design:

- Sleep starts a cancellable countdown, 15 seconds by default.
- Restart and shutdown need explicit confirmation, then the same countdown.
- A pending power action can be cancelled until it fires.
- An unreachable daemon is shown as Offline, never as asleep. State shown is
  what was observed; an unreachable machine also shows when Barge last saw it.
  A registered machine no poll has reported back on yet is Checking…, never
  an empty registry.
- Wake works while the daemon is down, using the stored MAC and broadcast
  address.
- Launching Moonlight against the PC is a convenience action in the UI.

## States

One state model, in the app's `machine_state.rs`, is shared by the window and
the native menu. It is a pure function of the last observation, when Barge
received it, whether the daemon answers now, and the last power action Barge
sent. Each state shows what was observed and offers only the actions below.

| State | Shown as | Offers |
| --- | --- | --- |
| Offline (unreachable, no recent action) | Offline | Wake |
| Checking (registered, no observation yet) | Checking… | Wake |
| Waking (Wake sent, not yet reachable) | Waking… | none |
| Awake | Awake | Sleep, Restart…, Shut down…, Open Moonlight |
| Pending (daemon reports a countdown) | Sleeping in 12 s (or Restarting, Shutting down) | Cancel |
| Going down (fired, still reachable) | Going to sleep… / Shutting down… | none |
| Restarting (restart fired) | Restarting… | none |
| Didn't wake (a wake timed out) | Didn't wake | Wake |
| Still on (a sleep, shutdown or restart timed out) | Still on | the awake set |

- A waiting state ends at its target or its timeout, measured from when the
  action was sent: Wake 120 s, Restart 180 s, sleep or shutdown 60 s. Restart
  passes through unreachable and finishes when the daemon answers again.
- A machine waiting on an action is polled every 2 s until the state settles,
  then at the refresh interval.
- A countdown's end is placed on the Mac's clock from the daemon's own
  observation time, so a skewed PC clock does not move it.
- Labels are worded the same in the window and the menu.

Tom's feedback on 2026-11-19 is the reason: the menu showed every action all
the time, and a wake returned to "unavailable" until the next poll.

## VM disks

A virtual machine's disk is kept data, the `vms` class, and the tool reaches it
through a link the tool owns, such as `~/.colima/_lima/<profile>/disk`
([resources](resources.md#known-link-locations)). Retarget that link before the
VM starts: Lima recreates a missing disk, so a VM started with a dangling link
boots a blank machine instead of failing. `barge move --plan` lists such a link
when it points inside the moved folder, and refuses to apply until the move is
asked to retarget it ([moving a class](resources.md#moving-a-class)).

## Registry state

The registry is `machines.json` in Barge's account state directory. Its files
and shared lock follow the [account state contract](architecture.md#layout-on-a-volume).

`crates/barge-machine` keeps its own copy of that publication step and the
lock instead of depending on `crates/barge`: the storage library's dependency
tree stays what Oxide accepts, and a machine client (the CLI today, the UI
later) does not pull in the storage mechanics. The copies are changed
together; `crates/barge/src/state.rs` is the reference.

A record read from disk is validated on every read. An unreadable document, an
unknown schema, a duplicate name, a non-canonical MAC address or any other
invalid field is an unknown state — exit 3 — never an empty registry. A value
an operator gives, such as `barge machine add --port 0`, is a usage error —
exit 2 — and is refused where the machine is built.

## Protocol and trust

- Tailscale mode (the default for existing configs) binds the configured tailnet
  address and authorizes callers through Tailscale `whois` and an allowlist.
  Tailscale encrypts traffic and is recommended across networks.
- LAN mode uses a shared 256-bit key installed once on each client. Requests
  carry an HMAC of the method, path, timestamp, nonce and body digest; the
  daemon checks a 60-second clock window and rejects repeated nonces. Re-pairing
  the daemon replaces the key and invalidates old clients. The key is stored
  separately from `machines.json`, owner-only. Removing a machine removes its
  stored key before the name can be reused.
- LAN HTTP authenticates requests but does not encrypt them. Someone on the LAN
  can see that a power action happened, but cannot forge or replay it. Keep
  clocks synchronized. Bind a specific LAN address; wildcard binding requires
  `allow_wildcard` and produces a warning.
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

## Discovery and pairing

Status: design draft for Tom's sign-off, 2026-11-20. Nothing here is built.

Tom, 2026-11-20: for release, the Steam Deck plugin and the desktop app find
machines on the network themselves, with no manual entry of hosts, MACs or
keys. Today a client needs the host, port, MAC and broadcast address typed in,
plus the LAN key that `barged pair` printed, and pairing again unpairs every
client.

### Discovery

- `barged` advertises `_barged._tcp` over mDNS (DNS-SD) on its LAN address.
  TXT records carry its machine ID (a stable random ID made at first start),
  its display name, its mode (`lan` or `tailscale`) and its protocol version.
  On Bazzite it registers through Avahi, which ships with the OS; no new
  service is installed.
- `GET /v1/state` also reports the machine ID, the MAC of the interface it
  binds, and that interface's broadcast address. A client records them at
  pairing, so Wake still works when the machine is off and not advertising.
- Clients browse `_barged._tcp` themselves and list each machine they find
  that they haven't paired, with an Add button. They can't lean on the
  system resolver: SteamOS doesn't resolve `.local` names (found setting up
  the Deck on 2026-11-20), so the plugin carries its own mDNS browser and
  connects to the address it found. A paired machine is matched by ID, so an
  address change after a DHCP renewal updates itself.

### Pairing

Each client gets its own key, so adding or removing one never touches the
others. The shared key becomes the first entry of the list on upgrade, so the
Mac keeps working.

1. The client sends `POST /v1/pair` with its name and an X25519 public key.
   The daemon replies with its own public key and holds the request open for
   two minutes.
2. Both sides derive the shared key with X25519 and HKDF, and each shows a
   six-digit code computed from both public keys.
3. Someone approves the request on a device that's already trusted after
   checking the codes match. Matching codes prove no one on the LAN swapped
   the keys in between. The daemon then saves the client's key and name.

Where approval happens:

- **From a paired client.** The Mac app shows "Steam Deck wants to pair" with
  the code, and Approve or Deny. This is the normal route.
- **On the PC itself,** for the first client, when nothing is paired yet:
  `barged pair --approve` in a terminal lists pending requests with their
  codes. This is the one-time bootstrap and replaces today's printed key.

- `barge machine clients <name>` and the app's details dialog list a machine's
  paired clients and can revoke one.
- Requests stay HMAC-signed as today, with each client's own key; the daemon
  picks the key by a client ID header.
- Out of scope: discovery across networks (Tailscale has its own names) and
  pairing over Tailscale, which already identifies callers through `whois`.

### Questions for Tom

- Approval from the Mac app as the normal route, with the PC terminal only for
  the first client: is that right, or do you want approval on the PC's screen
  (a notification in Gaming Mode) as well?
- Per-client keys replacing the single shared key: OK to migrate the current
  key into the list automatically?

## UI

The Barge app is specced in [app](app.md): a menu-bar app built with Tauri,
Poodle and Longhorn, covering Machines and Storage in its first release.

## Repository layout

- `crates/barge-machine`: machine registry, Wake-on-LAN, protocol types and
  the client, shared by the CLI and UI.
- `crates/barged`: the daemon (Linux first), with its unit, polkit rule and
  example config in `packaging/barged/`.
- `crates/barge-cli`: the `barge` binary, which carries the machine verbs
  beside the storage ones.
- [`apps/barge-app`](app.md): the menu-bar app.

## Build order

1. **Built:** machine registry, Wake-on-LAN and protocol types in
   `barge-machine`; `barge machine add|list|remove|wake`.
2. **Built:** `barged`: state and power with countdown and cancel, the
   allowlist and Tailscale `whois`, logind over D-Bus, the client in
   `barge-machine`, `barge machine status|sleep|restart|shutdown|cancel`, the
   systemd unit and polkit rule in `packaging/barged/`, and the
   [install guide](../install-barged.md) for Bazzite.
3. **Built:** LAN pairing and authenticated requests, with no Tailscale required.
4. Tailscale on both machines, once Tom is ready for the sign-ins.
4a. Discovery and per-client pairing, designed in
   [discovery and pairing](#discovery-and-pairing), once Tom signs it off.
5. The Barge app: see [app](app.md#build-order).
6. Retire MoonDeck Buddy on the PC. This changes host software, so it needs
   Tom's go-ahead. The Steam Deck needs a `barged` client first, or loses
   remote control until it has one.

## MoonDeck Buddy scope

Tom used MoonDeck Buddy on the Steam Deck only for remote control of the PC
(2026-11-18), so `barged` replaces it entirely. No game-launch features carry
over. The Steam Deck becomes a client of `barged` once a client exists for it.
