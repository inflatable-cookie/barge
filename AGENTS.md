# Barge

Barge moves heavy, rebuildable developer data off the internal drive onto a dedicated external volume and keeps it there safely. Read [docs/README.md](docs/README.md), then the owning knowledge files, before changing anything.

## Ownership and work

Repository: `inflatable-cookie/barge`, planned inside Queue project `oxide` and owned by the Oxide workspace (Tom, 2026-10-26); there is no separate Barge project, workspace or agent. Planning, briefs, tasks and reviews live in that Queue project; do not create repository planning or status mirrors. Product implementation needs an approved Queue brief and independent exact-head review, as in Oxide. Design freezes: Tom delegated them to the Oxide Supervisor (the architect, Paseo agent in the Oxide workspace) on 2026-10-26. A design is frozen only after it passes independent review, and anything that changes Barge's scope or direction still goes to Tom.

Tom's scope and authority ruling lives in [vision](docs/knowledge/vision.md). Concepts, invariants and the Oxide boundary live in [architecture](docs/knowledge/architecture.md). Read [questions](docs/knowledge/questions.md) and [retirements](docs/knowledge/retired.toml) before planning.

## Guardrails

- No moving live data, changing host or tool settings, or deleting existing data without explicit operator approval for that action.
  Oxide's own idle-slot eviction and output placement during ordinary Oxide-routed builds are not Barge data deletion.
- Never write scratch to a home folder or a drive root. Use `/Volumes/Builds/scratch/barge/` when the Builds drive is present.
- Do not change Oxide from this repository. Oxide changes go through Oxide's own Queue project.
- When a change alters current truth, update its owning knowledge file in the same change. Record operator rulings before yielding.

## Layout

`crates/barge` is the library: every mechanic, no argv. `crates/barge-cli` is
the thin binary named `barge`: argument parsing, the JSON envelope, exit codes
and the human summary. Toolchain `1.97.1`, edition 2021 and `rust-version`
follow the first consumer's manifests and are pinned in `rust-toolchain.toml`;
`Cargo.lock` is committed.

The `proof-synthetic-account` feature (both crates) is proof-only. No ordinary build
enables it, and under it alone Oxide's reference directories come from a validated
synthetic account home ([stage 4 design](docs/knowledge/contracts/stage4-design.md),
"Proof build").

## Validate

Rust selectors run through `oxide`, never `cargo`; the `oxide:begin` block below
lists the commands. Each covers what it names:

- `effigy barge:install:test` — synthetic installer refusal and receipt fixtures; no real build or install.
- `effigy barge:install` / `effigy barge:reinstall` — planner only, after a reviewed merge; Tom's go-ahead is required for the first host install. Set `BARGE_INSTALL_PIN` and `BARGE_INSTALL_SCRATCH` as documented in the [consumer contract](docs/knowledge/contracts/consumption.md).
- `effigy rust:compile` — `oxide check --workspace --all-targets --all-features --locked`,
  so the proof-only `proof-synthetic-account` feature is compiled too.
- `effigy rust:compile:release` — `oxide check --release --workspace --bins --locked`,
  the default-feature release profile: the mechanical proof that the cfg(test)-only
  capture tape ([o2-tape](docs/knowledge/contracts/o2-tape.md)) compiles away, since a
  production build names none of it.
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
  `git diff --check HEAD`. Check its exit code. The check wrapper resolves the
  installed Northstar skill from the effective account (or repository override)
  and passes its explicit path, so a private fixture `HOME` does not hide it;
  missing or ambiguous skill installations still refuse.
- `effigy scratch:expiry:test` — the scratch native proof's own apparatus
  self-check: its case map, the hash-pinned supervisor and the premise record.
  No image, no scratch and no child process.
- `effigy scratch:expiry:shim:test` — the proof-only interposer exercise, in
  fresh caller scratch; no image and no native proof.
- `effigy scratch:expiry:native` and `effigy scratch:expiry:native:diagnostic`
  — S4's one bounded native under Oxide's qualified shared supervisor. HELD for
  owner activation: the declarations activate nothing, `BARGE_PROOF_BIN` names
  the binary, and each run is separately activated (diagnostic first, then the
  counted pass at the exact head). A missing receipt is a missing proof.
- `effigy space:native:test` — the stage 3 native proof's effect-free apparatus
  self-check; `effigy space:native:plan` prints its effect-free plan.
- `effigy space:native` and `effigy space:native:diagnostic` — the stage 3 space
  proof under the qualified supervisor. HELD for owner activation: diagnostic,
  counted author and independent reviewer runs are separately activated.
- `effigy maintain:native:test` — the `barge maintain` native proof's own
  apparatus self-check: its case map and dispatch, the hash-pinned supervisor,
  the nine-premise trace and the environment inventory. No image, no scratch and
  no child process.
- `effigy maintain:native` and `effigy maintain:native:diagnostic` — S7's one
  bounded native for `barge maintain`, on one owned 64 MiB image under the same
  qualified supervisor. HELD for owner activation on the same terms: the
  declarations activate nothing, and each run is separately activated.
- `effigy rust:test:proof-account` — the proof-only account's checks, with the
  `proof-synthetic-account` feature on. The ordinary `rust:test` never enables it.
- `effigy rust:test:tape` — the capture tape's pure self-tests and the embedded
  end-to-end fixture exercise of the actual release-plan harness
  ([o2-tape](docs/knowledge/contracts/o2-tape.md)). Read-only: fixtures live in the
  run root and the plan changes nothing.
- `effigy rust:test:tape:pure` — the recorder's in-memory self-tests only;
  excludes the harness exercise and its real process scan. It does not replace
  the required end-to-end exercise or establish capture qualification.
- `effigy rust:test:tape-eval` — the harness executable's own run with no arguments,
  which is its embedded exercise; with the argv contract it runs one manifest window.
- `effigy rust:test:tape-stage` — build the default-feature harness without running it;
  a separately verified export must bind source, lockfile, toolchain and executable.
- `effigy release:native:test` and `effigy release:native:plan` — F6-R1's native
  proof apparatus, effect-free: the pinned supervisor, the argv guard, the case
  table checked against its named regressions, the proof feature's declarations
  and the held bounds. No image, no scratch and no child process.
- `effigy release:native:stage` — builds the proof-feature binary of HEAD through
  Oxide and stages a hashed copy under the designated scratch. It runs no proof.
- `effigy release:native` and `effigy release:native:diagnostic` — F6-R1's one
  bounded native for the release effect, on one owned 64 MiB APFS image under the
  qualified supervisor (300 s, 512 MiB). Each run stages the proof-feature build
  of HEAD and runs only that staged file; the ordinary production build is never
  the one tested. The proof feature tests release logic over synthetic
  AccountFacts and does not establish the production account lookup's
  equivalence. HELD for owner activation: the declarations activate nothing; the
  diagnostic runs first, then the counted author pass and a distinct reviewer pass,
  each separately activated at an exact head.

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
