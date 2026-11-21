# Barge

Barge moves heavy, rebuildable developer data (build output, caches, scratch)
off your internal drive and onto a dedicated external volume, and keeps it there
safely. Tools that would each reinvent drive handling link Barge's library or
call its CLI instead. It is for developers whose internal SSD keeps filling up,
and for tool authors who want placement, fallback and pruning without building
them.

## What is in the repository

- **`crates/barge`**: the storage library. Every mechanic lives here.
- **`crates/barge-cli`**: the `barge` command, a thin layer over the library
  with `--json` output for other tools.
- **`crates/barge-machine`** and **`crates/barged`**: control of other machines.
  `barged` is a small daemon that reports a Linux PC's power state and can
  sleep, restart or shut it down; `barge machine` drives it over LAN or
  Tailscale.
- **`apps/barge-app`**: a menu-bar app for machines and storage, built with
  Tauri and Svelte.

Start with the [documentation index](docs/README.md). The
[architecture](docs/knowledge/architecture.md) explains the design.

## Status

Barge is pre-1.0 and macOS-first. Storage runs on APFS volumes; `barged` targets
Linux. The platform limits are in
[architecture](docs/knowledge/architecture.md#platforms).

## Using the CLI

```sh
barge volume register --name Builds --mount /Volumes/Builds --reserve 20GiB
barge area declare --namespace oxide --name pool --volume Builds \
  --absent fallback:/Users/me/Library/Caches/oxide-pool
barge root list oxide/pool
barge prune --volume Builds --dry-run
barge scratch new --volume Builds --project effigy --label test-run --ttl 3600
barge --json status
```

`area declare` records the declaration that the library and later callers must
match. Run `barge --help` for every command. Exit codes: 0 success, 1 refused
or not found (nothing changed), 2 usage error, 3 internal error.

## Building from source

The repository pins its toolchain in `rust-toolchain.toml`; rustup installs it
on the first Cargo command.

```sh
cargo build -p barge-cli   # target/debug/barge
```

The menu-bar app's renderer uses bun. `effigy app:dev` installs its dependencies
from the committed lockfile and starts the app.

Contributors run the checks through `oxide` and `effigy`, as described in
[AGENTS.md](AGENTS.md).

## Documentation

- [Documentation index](docs/README.md)
- [Install `barged` on a Linux PC](docs/install-barged.md)
- [Licence](LICENSE): MIT
