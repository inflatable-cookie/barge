# Install `barged` on Bazzite

`barged` is the small daemon that lets the Mac's `barge` CLI see the Bazzite PC's
state and put it to sleep, restart it or shut it down. This guide is the
operator step after the daemon's pull request merges; nothing here runs from a
test. The design it installs is in
[machines](knowledge/machines.md#protocol-and-trust).

Bazzite is an rpm-ostree image. Do not layer a package for this: a user-installed
binary plus user unit files is enough and survives image updates.

## 1. Choose a connection

LAN mode works without Tailscale. It authenticates requests but does not hide
traffic: someone on the LAN can see a power action happened, but cannot forge
or replay one. Keep both clocks synchronized. Tailscale encrypts traffic and is
recommended across networks. For Tailscale mode, sign both machines into the
same tailnet and find the PC address with `tailscale ip -4` and the Mac's
stable node ID or MagicDNS name with `tailscale status --json`.

## 2. Build and install the binary

On the PC, build in a disposable container from the checkout. Podman and a
network connection are needed; no Rust toolchain is installed on the host.

```sh
mkdir -p ~/.local/bin
podman run --rm -v "$PWD:/src:ro" -v "$HOME/.local/bin:/out:Z" \
  docker.io/library/rust:1.97.1 sh -c \
  'cp -a /src /tmp/barge && cd /tmp/barge && CARGO_TARGET_DIR=/tmp/build cargo build --release --locked -p barged && install -m 0755 /tmp/build/release/barged /out/barged'
```

The daemon uses logind over D-Bus on Linux. Its macOS build reports state but
has no power backend.

## 3. Configure authentication

Create `~/.config/barged/config.json`. For LAN mode use the PC's LAN address:

```json
{
  "listen": "192.168.1.20:9777",
  "mode": "lan",
  "key_file": "~/.config/barged/lan.key"
}
```

Run `~/.local/bin/barged pair --config ~/.config/barged/config.json` on the PC.
Copy the printed key securely; it is shown only then. Running pair again replaces
it and unpairs all clients. On the Mac, after registering the machine below,
run `barge machine pair pc` and paste the key into stdin, followed by Enter.
`barge machine unpair pc` removes the Mac's copy.

For Tailscale instead, use this config (omitting `mode` also works):

```json
{
  "listen": "100.101.102.103:9777",
  "mode": "tailscale",
  "allowed_nodes": ["macbook-pro.tail1234.ts.net"],
  "tailscale_socket": "/var/run/tailscale/tailscaled.sock"
}
```

The allowlist matches stable node ID or MagicDNS name, case-insensitively.
The socket field is optional. A system tailscaled socket is root-only; grant
this account access with `sudo tailscale set --operator="$USER"`. A user-run
socket can be configured instead. Wildcard addresses are refused unless
`"allow_wildcard": true` is set; doing so logs a warning.

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
  --mac 00:11:22:33:44:ff # add --tailscale-name pc.tail1234.ts.net for Tailscale
barge machine status pc
barge machine sleep pc            # 15 s countdown; `barge machine cancel pc` stops it
barge machine restart pc          # asks first; `--yes` skips the question
barge machine shutdown pc --yes
```

`--daemon-port` overrides the port recorded for the machine (default `9777`);
it must match `listen` in the PC's config.

## Verification and troubleshooting

For Tailscale, from the Mac on the tailnet:

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
# In LAN mode also remove ~/.config/barged/lan.key and unpair on the Mac.
rm ~/.config/systemd/user/barged.service
systemctl --user daemon-reload
```
