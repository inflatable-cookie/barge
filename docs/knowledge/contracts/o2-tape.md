# The F6-O2 B-S1 capture tape and release-plan harness

The Oxide-side capture contract (`barge-adoption.md` in Oxide's repository,
"Shared capture contract and implementation slices") fixes what Barge's B-S1
slice builds: a `cfg(test)`-only observation tape over the release planner's
leaf reads, and an in-crate harness executable that runs the actual
`barge::release::plan` against one manifest window. This file is Barge's own
record of that apparatus: what exists, what it records, its caps, and what it
does not prove.

## What exists

- `crates/barge/src/tape.rs` — the recorder and the transcript writer. It is
  declared in `lib.rs` as a `pub(crate)` `#[cfg(test)]` module, so a production
  build compiles none of it and nothing becomes public. Every call from a
  production function body normally uses a separate `#[cfg(test)]`-attributed
  statement. Where a consumed expression cannot otherwise be observed, a narrow
  test/non-test split retains the original production expression and records
  the single test computation.
  The tape is one-sided: it records what the real syscalls and parsers
  returned, and feeds nothing back.
- `crates/barge/src/tape_harness.rs` — the window runner (`run`) both the
  executable and the embedded exercise call, plus the fixture builders. Also
  `cfg(test)`-only and `pub(crate)`.
- `crates/barge/tests/tape_eval.rs` — the `[[test]]` target with
  `harness = false`, so the compiled artifact is a standalone executable whose
  argv contract is `--fixture-root --manifest --transcript-out --case`. The
  target compiles the library's own sources through `#[path]` includes with
  `cfg(test)` set; `cache.rs` and `space.rs` declare child modules, and a
  `#[path]`-included file's children resolve beside the file itself, so the
  symlink shadows `crates/barge/src/{undo_capture,effect,guard}.rs` point back
  at the real child files (`lib.rs` never declares those names, so production
  compiles none of them). The linked library rlib is never used for
  instrumented code.
- `crates/barge/src/tape.rs`'s own tests: recorder roundtrip and deterministic
  bytes, the refusal document, the caps (read rows, per-key occurrences,
  process rows, the 8 MiB document), the no-public-surface declaration shape,
  and a limited source guard check. That check does not establish per-site
  coverage; the mechanical compile-away check is `effigy rust:compile:release`.
- `crates/barge/src/tape_harness.rs`'s tests: the embedded end-to-end exercise
  (one exclusively owned directory fixture on a real selected volume, the
  actual `release::plan`, one completed window over a settled root with a
  positively absent checkout, and one refused window whose transcript is
  still written).

## What the tape records

The B-S1 leaf table, at its source symbols. Process leaves (`MacOs::process_table`,
per-fd `PROC_PIDLISTFDS` listings, per-descriptor
`PROC_PIDFDVNODEPATHINFO`/`PROC_PIDFDVNODEINFO` answers including every
bounded retry, `csops` code-signing status, effective uid, boot identity,
per-pid start identities, the mount census and mount-root proofs); `DirOps`
leaves (opens, child facts, child reads with digests, listings with per-entry
component identities, directory facts, rechecks); direct reads outside
`DirOps` (`state::read_bounded_inner`, `state::read_intents_with_deadline`,
`registry::read`, `area::read_directory` — which serves `area::show`'s
declaration reads — `area::read_holds`, `reference::read_registry`,
`reference::read_receipts_with`, `reference::read_oxide_marker`,
`reference::control_directory_with`, `reference::pools_holding`,
`reference::BargeReferences::read`, `pool_custody::read_document` with its
pre- and post-read stats, `identity::read`); lock outcomes
(`StateLock::shared_if_present`, held/absent/refused; `StateLock::exclusive`
and `state::publish_atomic_with` record effect rows, so an attempted write can
never pass as a read-only pass); enumeration and activity
(`area::scan_activity_within`, `area::list_names`, `area::resolve_by`,
`area::VolumeRoot::open_with`); custody (`custody::take_custody_by` verdicts,
`custody::decide`'s safe terminal, `pool_custody::classify` record
classifications, `os::prove_absence` verdicts); and the caller clock (every
`state::now_ms` reading, as clock rows). Read-row identity is the complete
object identity — absolute path, parent device and inode, own device and
inode; component plus observed parent; `(pid, start, uid)` for processes, never
a pid alone; UUID plus mount for volumes. Epochs are engine-local, monotonic,
assigned at the leaf; occurrences are counted per identity key, so a root-lock
reread is a second bound occurrence.

`area::show` adds no distinct read of its own: it is served by the
`area::read_declarations` rows recorded below it. The plan decides identity
through `identity::read` and `identity::compare`, not `identity::verify`;
`verify`'s own rows are recorded but a plan window does not reach them. Not
recorded, because the plan never calls them and they read no fixture object:
`random_bytes`, `set_dir_mode`, `create_child_symlink`, `sync_dir` and the
flush path.

## Caps and the honest finding about real hosts

The accepted contract's transcript caps are fixed and implemented as written:
8192 read rows, 8 MiB of document, 64 occurrences per identity key, 512
process rows. An exhausted cap is recorded in the transcript's `caps` block
(`read_rows_exhausted`, `occurrences_exhausted`, `process_rows_exhausted`,
`document_truncated`, `dropped_rows`) and makes any comparison insufficient —
never quietly complete.

The embedded exercise measured the row budget on a real development host: one
custody scan over this account's ~480 same-uid processes produces about 4700
per-fd leaf rows (`descriptor_path` and `descriptor_vnode_facts` for every
listed descriptor) plus the process rows, which alone approach or pass the
8192-row cap, and the host's full process table exceeds the 512 process-row
cap. This is the leaf tape recording exactly what the scan really reads, as
the contract requires; the consequence for the later X-S2 windows is theirs to
measure and declare — a counted comparison on a real host must either run in a
machine state whose custody scan fits the fixed caps or record its truncation
honestly (and then be insufficient). Nothing here relaxes a cap.

## The transcript

One `oxide.capture.transcript/1` document per window, always written, including
on refusal: `schema`, `engine: "barge"`, `case`, `manifest_sha256`, the
`entrypoint` (symbol and staged source binding), `outcome` (`completed` or
`refused`, the engine's own exit code and refusal vocabulary), the engine's
actual `PruneReport` under `result` when completed, `effects` (empty for the
read-only plan), `reads`, `clocks`, and `caps`. Serialization is deterministic
for the same window.

No machine-readable manifest schema is accepted yet: Oxide #240 proposes
`oxide-capture-manifest.schema.json` under the same semantic contract, and
that reviewed shape is the wire contract for B-S1 and the later X-S2
orchestration. What this harness has today is a consumption surface, not a
canonical layout. It reads exactly these manifest members
(`oxide.capture.manifest/1`, owned by the orchestrator): `case`,
`barge.volume_uuid`, `barge.namespace`, `barge.area`, and the optional
`barge.named_roots`; it binds the whole document's SHA-256 and ignores the
orchestrator's other members. The reads are cfg(test)-only apparatus, so
reconciling them to the reviewed schema is a harness change and nothing
production depends on them. A missing or
mistyped member is an apparatus error: exit 2, the reason on stderr, no
transcript — there is no engine outcome to record. The engine's own refusals
are engine outcomes: the transcript is written and the process exits with the
engine's own code (`ErrorKind::exit_code`, or `PruneReport::needs_human`'s
0/3).

## Staging and export (for the later X-S2 orchestration)

From the exact staged Barge commit, offline, committed `Cargo.lock`, default
features only — no feature enabled, the proof feature off, toolchain 1.97.1:

```sh
cargo test --locked --no-run --test tape_eval
```

The produced executable under the managed target directory is the one a window
invokes under the argv contract above; its digest, the toolchain and the
lockfile are the orchestrator's to verify before any run. No fixture, pool,
registry, root or account outside the given fixture root is read or written,
and `BARGE_STATE_DIR`/`BARGE_ACCOUNT_HOME` are never consulted: fixture
selection is explicit `StatePaths` and an `AccountFacts` implementation inside
the harness only.

## Selectors

`effigy rust:test:tape` runs the pure tape self-tests and the embedded
end-to-end exercise; `effigy rust:test:tape-eval` runs the harness executable
(no arguments: its embedded exercise). `effigy rust:compile:release` proves
the production compile-away. All are covered by `effigy rust:test` and
`effigy rust:compile` besides.

## What this does not prove

The tape and harness are apparatus, not evidence. No parity claim, no native
run, no live pool, root, registry or account read, and no comparison against
Oxide exists; that is the X-S2 slice's separately dispatched work at a
source-approved implementation head, with the counted native its own
separately activated Queue proof. A recorded transcript here never authorizes
a deletion, a custody conclusion or a live Builds comparison.
