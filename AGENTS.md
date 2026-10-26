# Barge

Barge moves heavy, rebuildable developer data off the internal drive onto a dedicated external volume and keeps it there safely. Read [docs/README.md](docs/README.md), then the owning knowledge files, before changing anything.

## Ownership and work

Repository: `inflatable-cookie/barge`, planned inside Queue project `oxide` and owned by the Oxide workspace (Tom, 2026-10-26); there is no separate Barge project, workspace or agent. Planning, briefs, tasks and reviews live in that Queue project; do not create repository planning or status mirrors. Product implementation needs an approved Queue brief and independent exact-head review, as in Oxide. Design freezes: Tom delegated them to the Oxide Supervisor (the architect, Paseo agent in the Oxide workspace) on 2026-10-26. A design is frozen only after it passes independent review, and anything that changes Barge's scope or direction still goes to Tom.

Tom's scope and authority ruling lives in [vision](docs/knowledge/vision.md). Concepts, invariants and the Oxide boundary live in [architecture](docs/knowledge/architecture.md). Read [questions](docs/knowledge/questions.md) and [retirements](docs/knowledge/retired.toml) before planning.

## Guardrails

- No moving live data, changing host or tool settings, or deleting existing data without explicit operator approval for that action.
- Never write scratch to a home folder or a drive root. Use `/Volumes/Builds/scratch/barge/` when the Builds drive is present.
- Do not change Oxide from this repository. Oxide changes go through Oxide's own Queue project.
- When a change alters current truth, update its owning knowledge file in the same change. Record operator rulings before yielding.

## Layout

`crates/barge` is the library: every mechanic, no argv. `crates/barge-cli` is
the thin binary named `barge`: argument parsing, the JSON envelope, exit codes
and the human summary. Toolchain `1.97.1`, edition 2021 and `rust-version`
follow the first consumer's manifests and are pinned in `rust-toolchain.toml`;
`Cargo.lock` is committed.

## Validate

Rust selectors run through `oxide`, never `cargo`; the `oxide:begin` block below
lists the commands. Each covers what it names:

- `effigy rust:compile` — `oxide check --workspace --all-targets --locked`.
- `effigy rust:format` — `oxide fmt --all -- --check`; run it when formatted
  Rust changed.
- `effigy rust:lint` — clippy at the RustQuality strict profile. Every finding
  is repaired or recorded as a concrete deviation in
  `docs/knowledge/contracts/rust-quality-deviations.json`.
- `effigy rust:test` — the unit and integration suites, through `oxide test`
  with a bounded admission and execution window.
- `effigy os:volumes:probe` — the read-only OS-layer diagnostic:
  `BARGE_OS_PROBE_PATH` names one directory on a volume, whose facts are read
  and then verified against that identity; nothing is written. Evidence, not a
  test.
- `effigy docs:check` — the Northstar link and retired-concept checks plus
  `git diff --check HEAD`. Check its exit code.

RustQuality strict is active for the Rust source, manifests, tests and their
documentation: `docs/knowledge/contracts/language-quality-activation.json` opts
in, `rust-quality-profile.json` declares the strict profile and its MSRV
sources, and `rust-quality-deviations.json` records every accepted deviation.

<!-- northstar:rust-quality:start -->
## Northstar Rust Quality

Scope: Rust source, Cargo manifests, build files, tests, and directly related
documentation under this directory.

Use Northstar's strict everyday-authoring route for ordinary Rust work. Resolve
the repository-owned profile and deviations under `docs/knowledge/contracts/`; never
assume a universal MSRV. Re-enter at task start and coherent batch closeout.
Preserve unrelated work. A quality audit, no-slop pass, or audit-and-fix request
is explicit audit intent; never route it through everyday authoring.
<!-- northstar:rust-quality:end -->

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
