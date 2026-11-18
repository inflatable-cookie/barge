# Install `barged` on Bazzite

`barged` is the small daemon that lets the Mac's `barge` CLI see the Bazzite PC's
state and put it to sleep, restart it or shut it down. This guide is the
operator step after the daemon's pull request merges; nothing here runs from a
test. The design it installs is in
[machines](knowledge/machines.md#protocol-and-trust).

Bazzite is an rpm-ostree image. Do not layer a package for this: a user-installed
binary plus user unit files is enough and survives image updates.

## 1. Prerequisites

- Tailscale is up on the PC and on the Mac, signed in to the same tailnet.
- The PC's Tailscale address: `tailscale ip -4` (for example `100.101.102.103`).
- Each client's identity, for the allowlist:
  - stable node ID from `tailscale status --json` (`SelfNode.ID` on the client,
    `Peer[].ID` on the PC; either the `ID` or the `StableID` value looks like
    `n1234ABCD`);
  - or the node's MagicDNS name (`pc.tail1234.ts.net`).

## 2. Build and install the binary

Build `barged` from the Barge checkout. On the PC, rustup or a container can
build it; the crate targets Linux and needs nothing but logind and a D-Bus
socket at run time.

```sh
oxide build --release -p barged        # in the Barge checkout
mkdir -p ~/.local/bin
install -m 0755 target/release/barged ~/.local/bin/barged
```

`barged` also builds on macOS so the workspace checks cover it. That build
reports state but has no power backend: logind exists only on Linux, and the
Linux backend sits behind `cfg(target_os = "linux")`.

## 3. Let `barged` ask Tailscale who is calling

The daemon resolves the caller's node with tailscaled's local API (`whois`) over
its unix socket. A system tailscaled's socket is root-only, so grant your own
account operator access:

```sh
sudo tailscale set --operator="$USER"
```

If you run tailscaled yourself, its socket lives under `$XDG_RUNTIME_DIR` and
needs no operator change; point `tailscale_socket` at it instead.

## 4. Write the config

```sh
mkdir -p ~/.config/barged
cp packaging/barged/config.example.json ~/.config/barged/config.json
$EDITOR ~/.config/barged/config.json
```

```json
{
  "listen": "100.101.102.103:9777",
  "allowed_nodes": ["macbook-pro.tail1234.ts.net"],
  "tailscale_socket": "/var/run/tailscale/tailscaled.sock"
}
```

- `listen` is the machine's own Tailscale address and the port the Mac dials.
  `barged` refuses to start on `0.0.0.0` or `[::]`: reaching the daemon is
  supposed to require the tailnet.
- `allowed_nodes` is the allowlist. A node matches by stable ID or by MagicDNS
  name, case-insensitively. An empty list is refused: an open daemon is not a
  useful default.
- `tailscale_socket` defaults to `/var/run/tailscale/tailscaled.sock`.

## 5. Install the systemd user unit

```sh
mkdir -p ~/.config/systemd/user
cp packaging/barged/barged.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now barged
```

The PC should be controllable whether or not someone is logged in at the
desktop, so let the user's services run without a session:

```sh
sudo loginctl enable-linger "$USER"
```

Logs: `journalctl --user -u barged -f`.

## 6. Grant exactly the three power actions

logind asks polkit before it sleeps, restarts or powers off. The daemon calls
those three methods with `interactive=false`, so it needs a rule that answers
without a prompt:

```sh
sudo install -m 0644 packaging/barged/50-barged-power.rules \
  /etc/polkit-1/rules.d/50-barged-power.rules
sudo sed -i "s/USER/$USER/" /etc/polkit-1/rules.d/50-barged-power.rules
```

The rule grants `org.freedesktop.login1.suspend`, `…reboot` and `…power-off` to
that account and leaves every other action alone. Nothing else is granted, and
the daemon never runs a shell command.

## 7. Register the machine on the Mac

```sh
barge machine add --name pc --hostname pc.local \
  --mac 00:11:22:33:44:ff --tailscale-name pc.tail1234.ts.net
barge machine status pc
barge machine sleep pc            # 15 s countdown; `barge machine cancel pc` stops it
barge machine restart pc          # asks first; `--yes` skips the question
barge machine shutdown pc --yes
```

`--daemon-port` overrides the port recorded for the machine (default `9777`);
it must match `listen` in the PC's config.

## Verification and troubleshooting

From the Mac, on the tailnet:

```sh
curl http://pc.tail1234.ts.net:9777/v1/state
```

- **`curl` cannot connect.** Check `systemctl --user status barged` and that
  `listen` is the PC's current Tailscale address. `barged` binds one address,
  and Tailscale addresses can change when a node is re-added to the tailnet.
- **`403 not_authorized`.** The caller's node is not on `allowed_nodes`, or the
  daemon could not ask tailscaled who it is. `journalctl --user -u barged` names
  which. Re-check `tailscale status --json` and `tailscale set --operator`.
- **`sleep` accepted but nothing happens.** polkit denied the logind call; the
  journal shows `logind refused Suspend`. Check the rule's account name and
  whether a second active session needs the `-multiple-sessions` variant.
- **`barge machine status pc` says unreachable.** The Mac cannot reach the
  daemon. It is never reported as asleep: asleep has to be observed, and an
  absent daemon has not been.

## Removing it

```sh
systemctl --user disable --now barged
sudo rm /etc/polkit-1/rules.d/50-barged-power.rules
rm ~/.local/bin/barged ~/.config/barged/config.json
rm ~/.config/systemd/user/barged.service
systemctl --user daemon-reload
```
