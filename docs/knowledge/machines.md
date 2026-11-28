# Machines

Status: design outline approved by Tom, 2026-11-18. Build steps 1, 2, LAN authentication, the daemon/CLI step of discovery and pairing, and `barged` serving the LAN and the tailnet together ([networks](#networks-and-listeners)) are complete; the Mac app's routes and the Decky plugin UI remain to be deployed or built.

Barge manages the machines it distributes resources across. The first is the
A Linux PC: wake it, see its state, and sleep, restart or shut it down. This
replaces MoonDeck Buddy, which is no longer maintained, and Tom's MoonDeck
Remote menu-bar app ([vision](vision.md#direction)).

## Parts

- **`barge` CLI on the Mac:** the `machine` verbs listed in
  [architecture](architecture.md#cli). Wake-on-LAN is a client operation and
  needs nothing running on the PC. JSON restart and shutdown requests require
  `--yes` rather than prompting.
- **`barged` on the PC:** a small Rust daemon. It reports state and performs
  power actions. On Linux it runs as a systemd user unit and asks logind over
  D-Bus for sleep, restart and shutdown, with a polkit rule granting only those
  actions. It never runs a shell command it was sent. It serves its LAN address and, when configured, its tailnet address at once: paired LAN clients on the LAN, and paired clients or allowlisted nodes on the tailnet. [Install guide](../install-barged.md).
- **Barge app:** a menu-bar Tauri app built with Poodle and Longhorn,
  covering machines and storage first ([app](app.md)).

The storage library stays daemon-free: Oxide never depends on `barged` or the UI.
See [Platform runs](runs.md#machine-leases) for machine leases and operator holds.

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

## Networks and listeners

Tom, 2026-11-21: `barged` serves the LAN and the tailnet at once. The Deck
stays on the LAN at home, and the Mac reaches the PC from anywhere on the
tailnet, with nothing typed on either side. The Barge planner dispatches the
change.

A config takes one of three shapes ([install guide](../install-barged.md#3-configure-authentication)):

- **LAN only:** `mode: "lan"`. `listen` is the LAN address.
- **Tailnet only:** no `mode` (or `mode: "tailscale"`) and no `tailscale`
  block. `listen` is the tailnet address and `allowed_nodes` is required.
  Every config written before the combined shape works unchanged.
- **Both (recommended):** `listen` is the LAN address and a `tailscale` block
  names the tailnet listener. Its `listen` is `"auto"`, which asks tailscaled
  for this node's IPv4 address and uses the LAN port, or an explicit
  `"addr:port"` on the same port: a paired client dials one port on both
  networks. Its `allowed_nodes` is optional. `mode` may be `lan` or
  absent; `mode: "tailscale"` and a top-level `allowed_nodes` are refused
  beside the block.

Both listeners serve the same state, power and countdown. What each surface
serves and who may call on it:

- **Pairing** (`POST /v1/pair`, `GET /v1/pair/<id>` and the pending, approve
  and deny endpoints) is served only on the LAN listener. Pairing is a LAN act,
  so physical presence keeps counting. The tailnet listener refuses it with
  `400`.
- **Client management** (`/v1/clients`) is also LAN only. An authenticated
  paired client gets `400` on the tailnet.
- **On the LAN,** a caller is a paired client whose HMAC signature checks out.
- **On the tailnet,** a request is accepted when it carries a paired client's
  signature, with the same credentials as on the LAN and no allowlist entry
  needed, or when its Tailscale `whois` matches `tailscale.allowed_nodes`.
  Anything else gets the same `403` as before.
- **Tailscale not up yet:** the LAN starts regardless. The tailnet side retries
  with a backoff that starts at one second and doubles to a cap of one minute,
  logs once when it fails and once when it comes up, re-reads its address every
  minute, and rebinds if the address changes.
- **State** (`GET /v1/state`) reports `tailscale_name` (the MagicDNS name, without
  its terminal dot) and `tailscale_ip` (the IPv4 address) as optional fields of
  `barge.machine-state/2` while the tailnet side is up. The mDNS TXT record
  carries `tailscale=<name>` when the name is known at start.

Tailscale encrypts traffic and is recommended across networks. LAN HTTP still
authenticates without encrypting, as described below.

## Routes

A paired machine has two routes with the same credentials: its LAN address
(`hostname`) and its tailnet name (`tailscale_name`). The CLI and the app try the
LAN address first with a connect timeout of 1.5 seconds, then the tailnet name.

- Only a connection that was never made moves on to the next route. Once a
  request has gone out, the daemon's answer, or a failure to read it, is the
  answer. An action is never sent twice.
- The route that answered is recorded on the machine (`last_path`) and tried
  first next time.
- `barge machine status` and the app show the route in use, as `LAN` or
  `Tailscale`.
- The tailnet name is stored from the daemon's state the first time a paired
  daemon reports one, and again if it changes. Nobody types it.
- Wake stays LAN broadcast only. Listing and revoking clients and pairing use
  the LAN route alone. An unpaired machine uses the one address its record names.

## Protocol and trust

- LAN mode assigns each paired client its own 256-bit key (`clients.json`,
  owner-only). Requests carry `x-barge-client-id` alongside an HMAC of the
  method, path, timestamp, nonce and body digest; the daemon checks a
  60-second clock window and rejects repeated nonces. On upgrade from a shared
  `key_file`, the existing key migrates automatically into `clients.json` as
  the `"legacy"` client so existing clients without `x-barge-client-id` keep
  working until revoked. On the client, credentials are stored separately from
  `machines.json`, owner-only. Removing a machine removes its stored
  credentials before the name can be reused.
- LAN HTTP authenticates requests but does not encrypt them. Someone on the LAN
  can see that a power action happened, but cannot forge or replay it. Keep
  clocks synchronized. Bind a specific LAN address; wildcard binding requires
  `allow_wildcard` and produces a warning.
- Endpoints: `GET /v1/state` (`barge.machine-state/2`, reporting `machine_id`
  and the bound interface's `mac` and `broadcast` when known), `POST /v1/power`
  (`{action, delay_s}`), `POST /v1/power/cancel`, plus the LAN pairing and
  client endpoints (`POST /v1/pair`, `GET /v1/pair/<id>`, `GET /v1/pair/pending`,
  `POST /v1/pair/<id>/approve`, `POST /v1/pair/<id>/deny`, `GET /v1/clients`,
  `DELETE /v1/clients/<id>`). Every response carries a `schema` field, under the
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

Status: daemon and CLI step built; the Mac app and Decky plugin UIs follow
against this protocol.

Tom, 2026-11-20: for release, the Steam Deck plugin and the desktop app find
machines on the network themselves, with no manual entry of hosts, MACs or
keys.

### Discovery

- `barged` advertises `_barged._tcp` over mDNS (DNS-SD) on its LAN address.
  TXT records carry:
  - `id`: stable random machine ID (32 lowercase hex characters created on first start)
  - `name`: display name (from config `name`, defaulting to the short OS hostname)
  - `mode`: `"lan"`
  - `version`: `"1"` (protocol version)
  - `tailscale`: the node's MagicDNS name, when the tailnet side is up at start
    (optional; see [networks](#networks-and-listeners))
  On Linux it registers through Avahi over D-Bus (`org.freedesktop.Avahi`),
  which ships with the OS; no new service is installed. If Avahi is absent,
  `barged` logs once and keeps serving.
- `GET /v1/state` (`barge.machine-state/2`) also reports `machine_id`, the
  bound interface's `mac`, and that interface's `broadcast` address. A client
  records them at pairing, so Wake still works when the machine is off and not
  advertising.
- Clients browse `_barged._tcp` themselves and list each machine they find
  that they haven't paired, with an Add button. They can't lean on the
  system resolver: SteamOS doesn't resolve `.local` names (found setting up
  the Deck on 2026-11-20), so the client carries its own mDNS browser and
  connects to the address it found. A paired machine is matched by ID, so an
  address change after a DHCP renewal updates itself.
- A found machine is matched to a registered record by the first rule that
  hits:
  1. the machine ID;
  2. the MAC the found machine reports on `GET /v1/state`, read without
     credentials. A LAN daemon refuses an unsigned read, so this rule matches
     only a daemon that answers one;
  3. the record's hostname resolving to the found address, on the same daemon
     port;
  4. the record's hostname equal, ignoring case, to the SRV host (for example
     `Linux-PC.local`).
  Rules 2 to 4 consider only records with no ID yet. A record that holds another
  ID is a different machine, so it is listed as found and never matched by name,
  address or MAC.
- A registered machine that answers `GET /v1/state` has the machine ID, MAC and
  broadcast it reports recorded where the record lacks them, so rule 1 holds
  from then on. An ID already on the record is never overwritten; a different
  ID is logged, nothing is written from that machine, and the found machine is
  listed separately.

### Pairing

Each client gets its own key, so adding or removing one never touches the
others. Any existing `key_file` shared key becomes the first entry of the list
(`id = "legacy"`, `name = "legacy"`) on upgrade, so existing clients keep working.

1. The client sends `POST /v1/pair` (`barge.pair-request/1`) with
   `client_name` and `public_key` (32-byte X25519 public key as 64 lowercase
   hex characters). The daemon replies `202` (`barge.pair-response/1`) with
   `request_id`, its ephemeral `public_key` (64 lowercase hex characters), and
   `expires_at_unix_ms` (two minutes from creation).
2. Both sides derive the 256-bit HMAC key and the 6-digit verification code as
   follows:
   - **Shared secret:** `shared = X25519(local_secret, peer_public)` (32 bytes;
     reject all-zero / non-contributory outputs).
   - **Key derivation:** `HKDF-SHA256` (RFC 5869) with `IKM = shared`,
     `salt = client_public || daemon_public` (64 bytes, raw client public key
     followed by raw daemon public key), `info = "barge-pair-v1"` (ASCII),
     output length `32` bytes.
   - **Six-digit code:**
     `digest = SHA-256("barge-pair-code-v1\0" || client_public || daemon_public)`
     (19-byte prefix including the NUL byte, followed by the two 32-byte raw
     public keys). Interpret `digest[0..4]` as a big-endian unsigned 32-bit
     integer `n`, and format `n % 1_000_000` as 6 zero-padded decimal digits.
   - **Fixed test vector** (for cross-client verification):
     - `client_secret`: `77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a`
     - `client_public`: `8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a`
     - `daemon_secret`: `5dab087e624a8a4b79e17f8b83800ee66f3bb1292618b6fd1c2f8b27ff88e0eb`
     - `daemon_public`: `de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b4f`
     - `derived_key`: `18d60e9b82dc130c6972596a7b47f3c1381520faa8704ded8429c85231323ea8`
     - `code`: `340092`
3. The client polls `GET /v1/pair/<request_id>` (`barge.pair-status/1`) until
   `status` is `approved` (which returns `client_id`), `denied` or `expired`.
4. Someone approves or denies the request on a trusted surface after checking
   the codes match (`GET /v1/pair/pending`, `POST /v1/pair/<id>/approve`,
   `POST /v1/pair/<id>/deny`), allowed for any signed paired client or callers
   on localhost (`127.0.0.1` / `::1`).

Where approval happens (Tom, 2026-11-20: a seamless experience needs a UI on
the PC, not a terminal):

- **On the PC, in Gaming Mode:** the Barge Decky plugin, installed on the PC
  as well as the Deck, shows "Steam Deck wants to pair" with the code, and
  Approve or Deny. The PC runs Decky Loader, so this reuses the plugin; on
  the PC it talks to its own `barged` over localhost.
- **On the PC, in Desktop Mode:** `barged` sends a desktop notification with
  the code and Approve and Deny actions, through the session's
  `org.freedesktop.Notifications` service (no-op when absent). It already runs
  as a user unit in that session.
- **From any paired client,** such as the Mac app, with the same code and
  buttons, for when the PC is in another room.
- `barged pair --approve` in a terminal stays as the fallback when no UI is
  running.

- `barge machine clients <name>` (`GET /v1/clients`) and the app's details
  dialog list a machine's paired clients and can revoke one (`barge machine
  revoke <name> <client>`, `DELETE /v1/clients/<id>`, or locally `barged
  clients remove <id>`).
- Requests stay HMAC-signed as today, with each client's own key; the client
  sends `x-barge-client-id: <id>` and the daemon picks the matching key (or
  `"legacy"` when the header is omitted).
- Out of scope: discovery across networks (Tailscale has its own names) and
  pairing over Tailscale, which already identifies callers through `whois`.

### Rulings

Tom, 2026-11-20, approving this design:

- Approval happens on the PC through the Decky plugin in Gaming Mode and a
  desktop notification in Desktop Mode, and from any paired client.
- The current shared key migrates into the per-client list automatically, so
  the Mac and the Deck keep working without pairing again.

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
   [install guide](../install-barged.md) for Linux.
3. **Built:** LAN pairing and authenticated requests, with no Tailscale required.
4. **Built (2026-11-21):** Tailscale alongside the LAN: the combined config, the
   tailnet listener, and the routes the CLI and app try. The PC's config and the
   deploy follow the merge.
4a. Discovery and per-client pairing, designed in
   [discovery and pairing](#discovery-and-pairing): `barged` and the CLI
   first, then the Mac app and the Decky plugin.
5. The Barge app: see [app](app.md#build-order).
6. Retire MoonDeck Buddy on the PC. This changes host software, so it needs
   Tom's go-ahead. The Steam Deck needs a `barged` client first, or loses
   remote control until it has one.

## MoonDeck Buddy scope

Tom used MoonDeck Buddy on the Steam Deck only for remote control of the PC
(2026-11-18), so `barged` replaces it entirely. No game-launch features carry
over. The Steam Deck becomes a client of `barged` once a client exists for it.
