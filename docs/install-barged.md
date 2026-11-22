# Install `barged` on a Linux PC

`barged` is the small daemon that lets the Mac's `barge` CLI see a Linux PC's
state and put it to sleep, restart it or shut it down. This guide sets up the
daemon on the PC and pairs it with the Mac. The test suite does not run these
steps. The design it installs is in
[machines](knowledge/machines.md#protocol-and-trust).

On an rpm-ostree image, do not layer a package for this: a user-installed
binary plus user unit files is enough and survives image updates.

## 1. Choose a connection

LAN mode works without Tailscale. Its trust model is the LAN half of
[machines](knowledge/machines.md#protocol-and-trust): requests authenticate but
traffic is not hidden, and both clocks must agree. Tailscale encrypts traffic and is
recommended across networks.

The recommended setup is both: the LAN address for the house, and the tailnet
for everywhere else. The combined config in step 3 does both at once, so the
Deck stays on the LAN and the Mac reaches the PC from anywhere. For the tailnet,
sign both machines into the same tailnet. Find the PC's tailnet address with
`tailscale ip -4`, and the Mac's stable node ID or MagicDNS name with
`tailscale status --json` if you want it in `allowed_nodes`. A paired client
needs no allowlist entry.

## 2. Build and install the binary

On the PC, build in a disposable container from the checkout. Podman and a
network connection are needed; no Rust toolchain is installed on the host.

```sh
mkdir -p ~/.local/bin
podman run --rm -v "$PWD:/src:ro,z" -v "$HOME/.local/bin:/out:Z" \
  docker.io/library/rust:1.100.0 sh -c \
  'cp -a /src /tmp/barge && cd /tmp/barge && CARGO_TARGET_DIR=/tmp/build cargo build --release --locked -p barged && install -m 0755 /tmp/build/release/barged /out/barged'
```

The explicit container build uses `/tmp/build/release/barged` inside the
container. An `oxide build --release` on the host uses Oxide-managed build
storage, so do not assume a `target/release/barged` path for that build.

The daemon uses logind over D-Bus on Linux. Its macOS build reports state but
has no power backend.

## 3. Configure authentication

Create `~/.config/barged/config.json`. For LAN mode use the PC's LAN address:

```json
{
  "listen": "192.0.2.20:9777",
  "mode": "lan",
  "name": "Linux PC"
}
```

`barged` binds this address (plus loopback `127.0.0.1:9777` for local approval
UIs) and advertises `_barged._tcp` over mDNS through Avahi, so clients find it
automatically on the LAN. `name` is optional and defaults to the host name.
Give the PC a DHCP reservation on the router, or let discovery update a
registered machine's address by its stable machine ID. On Fedora-based
desktops the default firewall zone (FedoraWorkstation) already allows high
ports such as 9777; on other distributions, open the port, for example with
`sudo firewall-cmd --add-port=9777/tcp --permanent && sudo firewall-cmd --reload`.

`barged` generates its machine ID and `clients.json` under
`~/.local/state/barged` (or `state_dir` if set). If an existing config sets
`"key_file"`, `barged` migrates that shared key into `clients.json` as the
`"legacy"` client on first start so previously paired clients keep working.

New clients pair one at a time without printing a key on the server: start
pairing on the client (for example `barge machine pair pc`), check that the
six-digit code matches, and approve it either from the desktop notification on
the PC, the Decky plugin on localhost, another already-paired client, or in a
terminal with:

```sh
~/.local/bin/barged pair --approve --config ~/.config/barged/config.json
```

To inspect or revoke paired clients locally on the PC:

```sh
~/.local/bin/barged clients list --config ~/.config/barged/config.json
~/.local/bin/barged clients remove <client-id> --config ~/.config/barged/config.json
```

For both networks, add a `tailscale` block to the LAN config. This is the
recommended shape:

```json
{
  "listen": "192.0.2.20:9777",
  "mode": "lan",
  "name": "Linux PC",
  "tailscale": {
    "listen": "auto"
  }
}
```

`"auto"` asks tailscaled for this node's tailnet IPv4 address and listens on
the LAN port beside the LAN address. Paired clients reach the PC on either
network with the same credentials, and need no entry in the allowlist. Add
`"allowed_nodes": ["laptop.example.ts.net"]` inside the `tailscale` block for
tailnet nodes that may call without pairing; the list may be left out. Pairing
stays a LAN act, so the tailnet listener refuses it. `"listen"` may also be a
fixed tailnet address, such as `"100.101.102.103:9777"`, on the same port as
`listen`: a paired client dials one port on both networks.

For the tailnet only, use this config instead (omitting `mode` also works):

```json
{
  "listen": "100.101.102.103:9777",
  "mode": "tailscale",
  "allowed_nodes": ["laptop.example.ts.net"],
  "tailscale_socket": "/var/run/tailscale/tailscaled.sock"
}
```

The allowlist matches stable node ID or MagicDNS name, case-insensitively.
The socket field is optional; inside a `tailscale` block, `"socket"` plays the
same part. A system tailscaled socket is root-only; grant this account access
with `sudo tailscale set --operator="$USER"`. A user-run socket can be
configured instead. Wildcard addresses are refused unless
`"allow_wildcard": true` is set; doing so logs a warning. Every existing config
keeps working unchanged.

## 4. Install the systemd user unit

```sh
mkdir -p ~/.config/systemd/user
cp packaging/barged/barged.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now barged
```

The unit keeps the home folder read-only except `~/.config/barged`, where
`barged` keeps its paired clients and machine ID. After upgrading from a
version without that line, copy the unit again and run
`systemctl --user daemon-reload` before restarting.

The PC should be controllable whether or not someone is logged in at the
desktop, so let the user's services run without a session:

```sh
sudo loginctl enable-linger "$USER"
```

The user unit cannot order itself after the system `tailscaled.service`. In
the combined shape that does not matter: the LAN side starts whether or not
Tailscale is up, and the tailnet side waits for tailscaled, retrying with a
backoff, and binds once it has an address. The log says once when the tailnet
side comes up. Logs: `journalctl --user -u barged -f`.

## 5. Grant exactly the three power actions

logind asks polkit before it sleeps, restarts or powers off. The daemon calls
those three methods with `interactive=false`, so it needs a rule that answers
without a prompt:

```sh
sudo install -m 0644 packaging/barged/50-barged-power.rules \
  /etc/polkit-1/rules.d/50-barged-power.rules
sudo sed -i "s/USER/$USER/" /etc/polkit-1/rules.d/50-barged-power.rules
```

The rule grants `org.freedesktop.login1.suspend`, `…reboot` and `…power-off` to
every process running as that account, not just `barged`, and leaves every
other action alone. The daemon never runs a shell command.

## 6. Register and pair the machine on the Mac

In LAN mode, discover and add the machine without typing its IP or MAC:

```sh
barge machine discover
barge machine add "Linux PC" --name pc
barge machine pair pc             # prints the 6-digit code and waits for approval
barge machine clients pc          # list paired clients on pc
barge machine revoke pc <id>      # revoke a paired client
```

Or register manually (and for Tailscale mode):

```sh
barge machine add --name pc --hostname pc.local \
  --mac 00:11:22:33:44:ff # add --tailscale-name pc.example.ts.net for Tailscale
barge machine status pc           # reports `lan` at home, `tailscale` away from it
barge machine sleep pc            # 15 s countdown; `barge machine cancel pc` stops it
barge machine restart pc          # asks first; `--yes` skips the question
barge machine shutdown pc --yes
```

`--daemon-port` overrides the port recorded for the machine (default `9777`);
it must match `listen` in the PC's config.

## Verification and troubleshooting

For the tailnet, from a node on the allowlist (a paired client signs its
requests, so `curl` works only from an allowlisted node):

```sh
curl http://pc.example.ts.net:9777/v1/state
```

- **`curl` cannot connect.** Check `systemctl --user status barged` and that
  `listen` is the PC's current Tailscale address. `barged` binds one address,
  and Tailscale addresses can change when a node is re-added to the tailnet.
- **`403 not_authorized`.** The caller's node is not on the tailnet allowlist
  (`allowed_nodes`, in the `tailscale` block when there is one), and the request
  carries no paired client's signature. The daemon could also not ask tailscaled
  who the caller is. `journalctl --user -u barged` names which. Re-check
  `tailscale status --json` and `tailscale set --operator`.
- **State has no `mac` or `broadcast`, so clients can't wake the PC.** The
  unit predates `AF_NETLINK` in `RestrictAddressFamilies`, which `barged`
  needs to read the interface's MAC. Copy the unit again, run
  `systemctl --user daemon-reload` and restart `barged`.
- **`sleep` accepted but nothing happens.** polkit denied the logind call; the
  journal shows `logind refused Suspend`. Check the rule's account name and
  whether a second active session needs the `-multiple-sessions` variant, or an
  inhibitor requires the `-ignore-inhibit` variant. Grant those variants only
  if needed; they also apply to every process of the account.
- **`barge machine status pc` says unreachable.** The Mac cannot reach the
  daemon. It is never reported as asleep: asleep has to be observed, and an
  absent daemon has not been.

## Removing it

```sh
systemctl --user disable --now barged
sudo rm /etc/polkit-1/rules.d/50-barged-power.rules
rm ~/.local/bin/barged ~/.config/barged/config.json
# In LAN mode also remove ~/.local/state/barged (and any ~/.config/barged/lan.key) and unpair on the Mac.
rm ~/.config/systemd/user/barged.service
systemctl --user daemon-reload
```
