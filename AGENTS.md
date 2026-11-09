# Barge

Barge moves heavy, rebuildable developer data off the internal drive onto a dedicated external volume and keeps it there safely. Read [docs/README.md](docs/README.md), then the owning knowledge files, before changing anything.

## Ownership and work

Repository: `inflatable-cookie/barge`, planned inside Queue project `oxide` and owned by the Oxide workspace (Tom, 2026-10-26); there is no separate Barge project, workspace or agent. Planning, briefs, tasks and reviews live in that Queue project; do not create repository planning or status mirrors. Product implementation needs an approved Queue brief and independent exact-head review, as in Oxide. Design freezes: Tom delegated them to the Oxide Supervisor (the architect, Paseo agent in the Oxide workspace) on 2026-10-26. A design is frozen only after it passes independent review, and anything that changes Barge's scope or direction still goes to Tom.

Tom's scope and authority ruling lives in [vision](docs/knowledge/vision.md). Concepts, invariants and the Oxide boundary live in [architecture](docs/knowledge/architecture.md). Read [questions](docs/knowledge/questions.md) and [retirements](docs/knowledge/retired.toml) before planning.

## Guardrails

- No moving live data, changing host or tool settings, or deleting existing data without explicit operator approval for that action.
  Oxide's own idle-slot eviction and output placement during ordinary Oxide-routed builds are not Barge data deletion.
- Never write scratch to a home folder or a drive root. Use `/Volumes/Stash/scratch/barge/` when the Stash drive is present.
- Do not change Oxide from this repository. Oxide changes go through Oxide's own Queue project.
- When a change alters current truth, update its owning knowledge file in the same change. Record operator rulings before yielding.
- Every pull request adds one short `changelog.d/<task-or-pr>-<slug>.<type>.md` fragment. Keep `CHANGELOG.md`'s `[Unreleased]` section empty; put changelog guidance outside `changelog.d/*.md` because every Markdown file there is treated as a fragment.

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

Ordinary Rust selectors run through `oxide`; the `oxide:begin` block below lists
the commands. The named capture-only Cargo selectors below are a separately
guarded exception. Each selector covers what it names:

- `effigy rust:compile:cargo`, `rust:compile:release:cargo`, `rust:format:cargo`
  and `rust:lint:cargo` — the corresponding locked capture covering checks with
  the owner's pinned Cargo and qualified supervisor. Release compilation plus
  cfg(test) source discipline supports compile-away; it is not a binary behavior test.
- `effigy rust:test:tape:cargo` — recorder controls and the embedded harness
  fixture; `rust:test:tape-eval:cargo` runs the harness target's embedded exercise;
  `rust:test:tape-stage:cargo` builds that target without running it and emits
  artifact metadata. These checks do not qualify the private launcher, its
  isolation, or the private export producer and do not authorize consumer adoption.

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
  with 300 s admission and 600 s execution, including compilation and Cargo-lock
  waits. Expiry still forces closure; historical 300 s timeouts remain failures.
  For fixture-bound runs, set `BARGE_TEST_SCRATCH_ROOT` and `TMPDIR`/`TMP`/`TEMP`
  to the same fresh Stash scratch directory; compiler output stays in Oxide's pool.
- `effigy os:volumes:probe` — the read-only OS-layer diagnostic:
  `BARGE_OS_PROBE_PATH` names one directory on a volume, whose facts are read
  and then verified against that identity; nothing is written. Evidence, not a
  test.
- `effigy docs:check` — the Northstar link and retired-concept checks plus
  `git diff --check HEAD`. Check its exit code. The check wrapper resolves the
  installed Northstar skill from the effective account (or repository override)
  and passes its explicit path, so a private fixture `HOME` does not hide it;
  missing or ambiguous skill installations still refuse.
- `effigy barge:changelog:test` — synthetic CLI controls for validation and
  deterministic fold/dry-run behavior, using only temporary fixture repositories.
- `effigy barge:changelog:check` — validates fragment shape and an empty
  `[Unreleased]` section. `docs:check` includes this check.
- `effigy barge:changelog:fold --version <x.y.z> [--date YYYY-MM-DD]` —
  release-preparation fold; use `--dry-run` to preview. Requires the project's
  existing explicit release authority.
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
- `effigy rust:test:tape` — tape controls, a deterministic positive fixture and
  a real-host bounded-outcome exercise of the actual release-plan harness
  ([o2-tape](docs/knowledge/contracts/o2-tape.md)). Read-only: fixtures live in the
  run root and the plan changes nothing.
- `effigy rust:test:tape:pure` — the recorder's in-memory self-tests only;
  excludes the harness exercise and its real process scan. It does not replace
  the required end-to-end exercise or establish capture qualification.
- `effigy rust:test:tape:custody` — the retained fixture custody setup controls
  ([o2-tape](docs/knowledge/contracts/o2-tape.md), "Retained fixture custody"):
  ordinary cleanup versus retained lifetime on drop, an injected failing setup
  step and unwinding, the opt-in and run-root premises, the bounded kept-fixture
  line and the manifest-before-run order. No engine window, host-volume probe,
  image or native run; the mode itself is a test-only fixture lifetime.
- `effigy rust:test:tape-eval` — the harness executable's own run with no arguments,
  which is its embedded exercise; with the argv contract it runs one manifest window.
- `effigy rust:test:tape-stage` — build the default-feature harness without running it;
  a separately verified export must bind source, lockfile, toolchain and executable.
- `effigy rust:test:tape-export` — source-defined archive/build/export producer;
  held until its private wrapper's admission and resource gates are established.
- `effigy rust:test:tape-export:pure` — pure Git blob identity controls; no
  build, fixture or export.
- `effigy rust:test:tape-admission:pure` — mocked admission refusal controls;
  includes composition with the hash-bound Oxide candidate component named by
  `BARGE_TEST_CANDIDATE_COMPONENT`; no live inventory, fixture or build.
- `effigy rust:test:tape-private-export` — source-defined private wrapper; it
  requires independently reviewed admission and resource evidence and a new
  authorized block. Neither is established for the retained failed route.
- `effigy release:native:test` and `effigy release:native:plan` — F6-R1's native
  proof apparatus, effect-free: the pinned supervisor, the argv guard, the case
  table checked against its named regressions, the proof feature's declarations,
  the Stash scratch route, the stage route and the attach/custody/closure gates,
  and the held bounds. No image, no scratch and no child process.
- `effigy release:native:stage` — builds the proof-feature binary of HEAD through
  Oxide and stages a hashed copy in an exclusive directory under the designated
  Stash scratch (`/Volumes/Stash/scratch/barge`). It runs no proof; a native run
  never reads that copy.
- `effigy release:native` and `effigy release:native:diagnostic` — F6-R1's one
  bounded native for the release effect, on one owned 64 MiB APFS image under the
  qualified supervisor (300 s, 512 MiB). Each run verifies the supervisor's bytes
  and the verified Stash scratch UUID before any effect, then stages the
  proof-feature build of HEAD inside the guarded run and runs only that staged
  file; the ordinary production build is never the one tested. The guard's opt-in
  image-custody indication is published before the backing exists, and closure
  requires the ordinary detach's success and reap plus whole-backing attachment
  absence, so an uncertain attach or detach retains the image and faults the run.
  The proof feature tests release logic over synthetic
  AccountFacts and does not establish the production account lookup's
  equivalence. HELD for owner activation: the declarations activate nothing; the
  diagnostic runs first, then the counted author pass and a distinct reviewer pass,
  each separately activated at an exact head.
- `effigy outputs:native:test` and `effigy outputs:native:plan` — the Oxide
  outputs adapter's proof apparatus, effect-free: the pinned supervisor, the
  guard containment and argv guard, the case table checked against its named
  regressions in `crates/barge/src/oxide_outputs/tests.rs`, the proof feature's
  declarations and the driver's `required-features`, the Stash scratch route,
  the nine premise dispositions and the held 128 MiB / 300 s / 512 MiB bounds
  with the 30 s work stop. No image, no scratch and no child process.
- `effigy outputs:native` and `effigy outputs:native:diagnostic` — the
  adapter's one bounded native for the delegated deletion, on one owned
  128 MiB APFS image under the qualified supervisor (300 s, 512 MiB), reusing
  the release proof's staging, custody and closure helpers. Each run verifies
  the supervisor's bytes and the verified Stash scratch UUID, refuses any
  non-guard caller, stages the proof-feature driver build of HEAD inside the
  guarded run, and runs only that staged file. The guard's image-custody
  indication is published before the backing exists, work stops 30 s before
  the deadline, and closure requires the ordinary detach's success and reap
  plus whole-backing attachment absence, so uncertain custody retains the
  image and faults the run. It tests the adapter over synthetic AccountFacts
  and does not establish the production account lookup, a live drive, or any
  adoption. HELD for owner activation on the same terms as the release proof's
  natives.

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
