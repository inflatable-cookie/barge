# Barge

Barge moves heavy, rebuildable developer data off the internal drive onto a dedicated external volume and keeps it there safely. Read [docs/README.md](docs/README.md), then [vision](docs/knowledge/vision.md) and [architecture](docs/knowledge/architecture.md), before changing anything.

## Ownership and work

Repository: `inflatable-cookie/barge`. Since 2026-11-17 Barge has its own planner (a Claude Code thread) and is no longer planned by the Oxide Supervisor; Oxide's planner keeps Oxide (Tom). Briefs, tasks and reviews run through Nucleus; do not create repository planning or status mirrors. Product changes go through a brief, a PR and one independent review. Scope or direction changes go to Tom.

## Guardrails

- Keep it small. The lean design in [architecture](docs/knowledge/architecture.md) is the target: cooperative holds, trash-then-delete, no process scanning, no journals, no capture harnesses, no native proof ladders. Adding machinery of that kind is a scope change for Tom.
- No Oxide coupling: no Oxide-specific modules, formats or adapters in Barge. Do not change Oxide from this repository.
- No moving live data, changing host or tool settings, or deleting data outside Barge's own roots without explicit operator approval for that action.
- Never write scratch to a home folder or a drive root. Tests use their own temporary directories.
- When a change alters current truth, update its owning knowledge file in the same change. Record operator rulings before yielding.
- Every pull request adds one short `changelog.d/<task-or-pr>-<slug>.<type>.md` fragment. Keep `CHANGELOG.md`'s `[Unreleased]` section empty.

## Layout

`crates/barge` is the library: every mechanic, no argv. `crates/barge-cli` is
the thin binary named `barge`: argument parsing, JSON output, exit codes and
the human summary. `crates/barge-machine` contains machine registry,
Wake-on-LAN and daemon protocol types, separate from storage. Toolchain
`1.97.1` and edition 2021 are pinned in `rust-toolchain.toml`; `Cargo.lock` is
committed. Oxide depends on the library at an exact Git commit.

The library follows [architecture](docs/knowledge/architecture.md#library-surface):
platform facts sit behind the `os` seam, account state under `state`, and
areas, roots, holds, pins, trash, space policy and scratch are their own
modules. A new mechanic is a module and an ordinary test, not a framework.

## Validate

Rust runs through `oxide` (see the block below).

- `effigy rust:compile` — `oxide check --workspace --all-targets --all-features --locked`.
- `effigy rust:test` — the unit and integration suites through `oxide test`.
- `effigy rust:lint` — clippy with the workspace lints in `Cargo.toml`.
- `effigy rust:format` — `oxide fmt --all -- --check`; `rust:format:apply` applies it.
- `effigy docs:check` — Northstar link and retired-concept checks, the
  changelog check, and `git diff --check HEAD`. Check its exit code.

Those are the repository's only selectors. Add one only for a check this
section names, and run it through Effigy rather than raw `cargo`.

<!-- oxide:begin -->
<!-- oxide:workspaces: . -->
## Rust commands: use `oxide`

Covers Cargo workspaces `.`, relative to this file's folder.

- Write `oxide` wherever you would write `cargo`: `oxide check`, `oxide test`, `oxide run -- <args>`, `oxide add <crate>`.
- Profiles: `quick` (default), `--profile debug` for a debugger, `--release`. Oxide places build output, so it refuses `--target-dir`.
- A tool that runs Cargo itself (Tauri CLI, npm or bun scripts): `oxide exec -- <command>`.
- A `waiting for …` line means another build holds a slot; let it wait. `oxide status` shows the pool, slots and waits.
- Use `oxide clean`, not `cargo clean`.

Managed by `oxide init`; edits between these markers are replaced.
<!-- oxide:end -->
