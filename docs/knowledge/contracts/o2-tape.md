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

The provisional instrumentation includes the following source symbols; it is
not a complete source-site audit. Process leaves (`MacOs::process_table`,
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
and `state::publish_atomic_with` record effect rows); enumeration and activity
(`area::scan_activity_within`, `area::list_names`, `area::resolve_by`,
`area::VolumeRoot::open_with`); custody (`custody::take_custody_by` verdicts,
`custody::decide`'s safe terminal, `pool_custody::classify` record
classifications, `os::prove_absence` verdicts); and the caller clock (every
`state::now_ms` reading, as clock rows with the test build's caller location).
Required identity includes parent and
own device/inode, process start and uid, and volume UUID; many current rows do
not yet bind all of these facts and are explicitly unproven. Epochs are engine-local, monotonic,
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

The recorder tracks fixed caps of 8192 read rows, 8 MiB per document, 64
occurrences per identity key and 512 process rows. Its internal document has a
`caps` block with exhaustion flags and dropped-row counts. The shared-wire
transcript instead carries `truncation` flags for read rows, occurrences,
process rows and document truncation. Exhaustion makes comparison insufficient,
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

The harness converts the recorder's internal document to the reviewed
`oxide.capture.transcript/1` shape for a normal window. It emits `schema`,
`version`, `engine`, `case`, `manifest_sha256`, the entrypoint symbol,
`outcome.state`, the engine outcome and result under `outcome`, `truncation`,
`effects`, `reads`, `clocks`, classifications, recorded holders, fixture
capture and notes. The source binding is recorded in notes. The internal
`caps`, obligation diagnostics and `observation_proven` fields are not shared
wire fields. A failed conversion retains the internal recorder document and
returns an apparatus error; it does not invent an engine result. The bounded
apparatus-refusal document for a failure before the engine runs is also an
internal diagnostic, not a conforming shared transcript.

The copied Oxide 283 parser validates the reviewed manifest wire structure,
including `parent_required` on each mandatory observation. The harness then
extracts `case`, `barge.volume_uuid`, `barge.namespace`, `barge.area`, and
optional `barge.named_roots` to invoke the engine. It also binds the complete
manifest digest and attaches obligation and effect diagnostics; it does not
claim that the parser fully validates every substructure or establishes
completeness. Missing or mistyped required members cause an apparatus error.
Engine refusals retain the engine's own error and exit code in the outcome.

## Staging and export (for the later X-S2 orchestration)

From the exact staged Barge commit, offline, committed `Cargo.lock`, default
features only — no feature enabled, the proof feature off, toolchain 1.97.1:

`effigy rust:test:tape-stage` declares a locked, default-feature `tape_eval`
build without running it. `rust:test:tape-export` declares the source exporter:
it archives an exact Git commit, hashes its tree members and lock/toolchain,
compares the archive's complete file set, Git blob identities and executable
modes against `git ls-tree`, builds through the stage selector, refuses changed source or a missing unique
default-feature executable, and records the executable, compiler and private
Oxide digests. The external harness requires the receipt and its independently
supplied digest and an independently supplied export-closure digest; it checks
the executable and every listed source byte before
the engine window, including the recorded executable mode and absence of
extra entries. It also compares the provisional manifest's Barge pin,
slice commit, complete `crates/barge/src/` file digest map, staged lock,
toolchain and Barge executable digest to that receipt, refusing missing or
mismatched fields. This prepares the proposed source binding; it is not
validation of the complete shared schema. Its binding is the receipt digest,
not an evaluator leaf.
`scripts/tape/private-wrapper.py` and `admission.py` describe a private launcher
with one-use, externally authorized `dry`, `validate` and `export` blocks. A
missing block, source audit, retained-root ledger, debug-build evidence, or
binding refuses. These documents and their expected SHA-256 values must be
provided through an authenticated independent channel; a digest or a string
inside a self-authored document is not authorization or independent review.
No block has been authorized and the route has not been executed or qualified.
Admission subprocess isolation and aggregate scratch accounting remain
unproved until the required evidence is independently reviewed and the real
gate is run under a new covering block.
The closure document is written only after a successful child and equal live
inventories; it binds their digests, the export receipt and the wrapper source.
Its scope is explicitly only that inventory comparison: it does not attest
admission isolation, elapsed budget or scratch accounting. A missing or changed
closure refuses before the engine runs.
The retained failed inventory remains a failure. No fixture, pool,
registry, root or account outside the given fixture root is read or written,
and `BARGE_STATE_DIR`/`BARGE_ACCOUNT_HOME` are never consulted: fixture
selection is explicit `StatePaths` and an `AccountFacts` implementation inside
the harness only.

### Private admission source map and preparation

The source map is a preparation guide, not an isolation finding. At pinned
Oxide `3de241325dd286ae44a5759fa6825b05a81c7b4e`, tree
`452a9c2dc6b1432c643c5053655e029955977754`, `src/bin/oxide/config.rs`
163–187 and 261–275 implement the debug test-home control and home selection;
328–345 and 467–541 resolve configuration and storage roots, and 1232–1255
read the state pool registry. `src/bin/oxide/main.rs` `run_status`,
`print_settings`, and `print_other_pools` implement the recorded status probe.
`src/bin/oxide/scratch_io.rs` resolves `OXIDE_SCRATCH_ROOT` for the actual
execution path. The pinned `scripts/bounded-test.py` adopts
`OXIDE_TASK_SCRATCH_ROOT` and passes its effective environment to its leader;
the retained `isolation-source-audit.md` identifies the relevant source and
the build-script paths that require further independent analysis. Effigy
admission, Cargo/rustup, compiler/linker, every invoked build script and Git
remain separate audit boundaries: the launcher binds their bytes and requires
source paths, digests, line references, analysis and an independent review
reference for each boundary. A conclusion field alone cannot establish that
analysis or verify the reviewer.

For a future authorized run, prepare a fresh owner-only root under
`/Volumes/Builds/scratch/barge/`, with distinct `home`, `cache`, `tmp`,
`scratch`, `bin`, `oxide-source` and `barge-source` directories and a private
`config.toml`. Prepare source archives from the exact named commits; the
launcher compares their complete file sets, modes and Git blob identities to
the commit trees. Supply a separately qualified debug Oxide built from the
pinned source and lock with Rust 1.97.1, together with its build log, raw
supervisor closure, source and executable hashes, and equal independently
bound live inventories. The earlier failed bootstrap is not such evidence.
The executable, evidence and the pinned supervisor are inside the private
root. The required audit must trace actual admission and build child paths,
including the executable PATH, and identify where all subprocesses resolve
config, state, registry, cache, pool, scratch and home. Missing evidence is a
runtime refusal, not a claim of isolation.

Provide a `barge.private-roots/1` ledger naming every retained task-private root
and the new root, with source-bound root-discovery evidence and independent
review reference. The launcher checks the historical ledger's named log paths,
known retained roots and current task PR-body roots, rejects overlaps and
missing paths, and counts allocated blocks once per inode without following
symlinks. The accounting includes retained evidence as well as the new root;
it cannot determine an earlier peak or establish that an unlisted root does not
exist. Discovery completeness is an independent review obligation. Shared live
pool and output roots are not counted as private scratch and their sizes remain
unknown. The private root is supervised against the prospective 5 GiB ceiling less other
retained private allocations. Because the supervisor monitors the new root,
the independently reviewed `retained_roots` audit must also establish that all
other counted roots remain quiescent for the block; before/after counts alone
cannot prove an intervening aggregate peak. No cleanup or refund follows.

An externally authenticated `barge.validation-block/1` document binds the
task, exact Barge head and tree, wrapper, debug Oxide, source audit and root
ledger hashes, operator authorization reference, unique block ID, finite
seconds, 5 GiB scratch cap, private pool cap and ordered selectors. Its digest,
the audit and ledger digests, and paths are supplied independently via
`BARGE_VALIDATION_BLOCK[_SHA256]`, `BARGE_VALIDATION_SOURCE_AUDIT[_SHA256]`,
and `BARGE_VALIDATION_ROOT_LEDGER[_SHA256]`. Set the pinned supervisor, Git,
rustc, Oxide origin, rustup home and toolchain via the launcher inputs; its
allowlisted child environment explicitly sets private `HOME`, `CARGO_HOME`,
`TMPDIR`, `OXIDE_CONFIG`, `OXIDE_POOL`, `OXIDE_SCRATCH_ROOT`,
`OXIDE_TASK_SCRATCH_ROOT`, and debug `OXIDE_TEST_HOME_FROM_ENV`. No inherited
Oxide, XDG, compiler-flag or dynamic-loader setting is forwarded. The host-wide
`EFFIGY_ADMISSION_HOOK` and `QUEUE_ADMISSION_SOCKET` are forwarded only when
they exactly match the independently digested source audit, which must bind
the actual hook executables and their source and child paths. Missing or
changed bindings refuse before the dry gate. `BARGE_VALIDATION_EFFIGY` names
the directly executed, audited Effigy binary; a shell launcher is refused and
the child PATH must resolve to that same binary. The block is
claimed once even on refusal. It starts a new prospective allowance only; the
historical 1200 + 600 + 600 seconds remain charged and exhausted.

Operator decision `578e728a-6322-4143-b1ea-c9ad2a67e7b2`, version 2,
answered by the operator on 2026-11-02, approves one additional prospective
1,200-second block for the reviewed private route. The block includes setup,
at most one 300-second debug-Oxide bootstrap, one inventory-bracketed status
gate and named safe selectors; it does not renew the historical charges or
increase the then-authorized 2 GiB private scratch cap. A changed live inventory ends the
block without repair, retry or attribution. The planner must still bind and
issue the one-use block and the independently reviewed inputs. The decision
does not authorize a full live-reading suite, native/image proof, installation,
production redirect, parity or merge. The host-admission correction after the
decision requires exact-head source review and planner binding; no execution
is inferred from this record.

Operator decision `af038a7e-7ff7-428e-8899-534a90e97bc2`, version 2,
answered by Tom on 2026-11-02, approves a prospective aggregate Barge
scratch ceiling of 3 GiB and Oxide ceiling of 8 GiB, with one further shared
diagnostic and at most one 300-second debug bootstrap within the same
remaining allowance. Earlier 2 GiB sessions and their evidence retain their
original bounds. This changes no watchdog, admission, identity, one-use or
isolation gate; it is not a new clock, a cleanup permission or runtime proof.
That 3 GiB allowance and the subsequent 4 GiB Barge / 8 GiB Oxide allowance
remain historical. The planner's prospective correction sets aggregate private
ceilings of 5 GiB for Barge and 9 GiB for Oxide. A new Barge block must bind
exactly 5 GiB; its independently bounded pool remains at most 4 GiB, and the
actual issued pool remains 256 MiB. Each run's private-root supervisor limit is
the aggregate ceiling less measured retained allocations; the native guard
remains at most 512 MiB. Earlier sessions retain their original bounds.

Within the prospective block, the launcher brackets source verification and
each actual Effigy child with bounded live before/after inventories. A changed
inode, contents or other recorded metadata refuses without repair or causal
claim. The first selector is `rust:test:tape-private-status`, which invokes
the debug Oxide's real `status` through Effigy and the pinned supervisor. It
must show the selected private config, home and pool and unique private state
registry and cache sentinels. Only then can the explicitly authorized safe
selectors run. `dry` admits only status; `validate` runs named safe checks;
`export` requires export last. Full `rust:test`, live-reading CLI paths and
the unbounded process-scan exercise are not admitted by this source. They need
their own disposition. The independently reviewed source route and a future
positive gate are prerequisites, not current validation evidence.

The Barge source archive is checked exactly before any command. Afterwards,
the snapshot admits only the newly generated top-level `target` tree: Oxide's
`apply_storage` names `<checkout>/target` for Cargo, and its output views can
contain links. Every output destination must resolve, without following an
escaped link, to owned entries inside the private root. Output bytes are not
treated as source. All original source bytes, executable modes and symlink
targets remain checked, and unrelated additions refuse. The unbuilt Oxide
archive keeps the strict snapshot with no generated-output exception.

For the pinned Effigy `737a6d0` foreground route, a baseline-bound Barge
archive may also acquire only the checkout gate directories and base32-named
scope and resource gate files under `.effigy/state/gates`. The separate
no-follow gate check requires owned private containment, private directory and
file modes, one link, bounded names, entries and file sizes; diagnostic contents
may be rewritten in place and are not authority. Existing archive entries remain
in the source snapshot, and other Effigy state remains forbidden. This applies
to the reviewed foreground selectors and nested tape-stage, not to durable or
other execution classes. The exporter uses the same snapshot oracle around its
nested stage while retaining its exact Git inventory and receipt bindings.

The separate candidate route consumes the provider's
`oxide.private-build-validation.candidate/1` receipt as an **unqualified**
candidate, not as an in-root debug bootstrap. Its origin and copied executable
are independently bound; no Cargo build or bootstrap receipt is fabricated.
The launcher requires an externally digested Oxide #240 candidate verifier,
its reviewed source commit and hash, and separately digested positive
qualification, case/guard and one-use issuance documents. The verifier is
Oxide #240's `verify_candidate_facts(gate, candidate, qualification, digests,
product_head, product_tree)` at `2f64d5aa`. Barge binds the component and its
provider import by their reviewed hashes before loading. That fact check is not
admission: Barge also requires actual raw guard and case receipts, nonempty
equal live inventories, the status sentinel, origin/copy bindings and a
separately issued one-use token. Its own block, 5 GiB ledger, host admission
and selector gates remain in force. An issuance's absolute use expiry may be
later than the active guard deadline, but must still be in the future. Actual
execution remains bounded by the earlier deadline and by the block, producer
and issuance allowances; use expiry grants no extra runtime. Guarded covering
uses the source-bound shared offline seeder; missing or changed evidence refuses. This source path
does not grant a runtime block or prove parity.

## Selectors

Tom's 2026-11-04 decision `c6e3e9c8-0d43-458e-8fa8-6d8fd3aa2190` v2
separates capture delivery from private launcher qualification. The capture-only
Effigy selectors `rust:compile:cargo`, `rust:compile:release:cargo`,
`rust:format:cargo`, `rust:lint:cargo`, `rust:test:tape:cargo`,
`rust:test:tape-eval:cargo` and `rust:test:tape-stage:cargo` use the owner's
pinned Cargo under the qualified supervisor with locked offline dependencies.
They cover compilation, formatting, lint, recorder and embedded harness
behavior, and artifact metadata. Source discipline plus release compilation
supports cfg(test) compile-away, not a binary behavior proof. Ordinary Rust
selectors remain Oxide-backed. Direct capture checks do not qualify private
launcher isolation, the current export producer or consumer adoption; the
stage artifact is not a `barge.tape.export/1` receipt.

`effigy rust:test:tape` runs the tape controls, a deterministic positive
end-to-end fixture over actual `release::plan` with explicit synthetic process
facts, held/unknown/bounded-writer counterfactuals, and a separate real-host
exercise. File and volume facts remain real in the positive fixture; it must
produce the complete plan with its settled root eligible and fit the fixed caps.
`effigy rust:test:tape-eval` runs the harness executable (no arguments: its
real-host self-check). That self-check accepts the actual engine outcome or an
honestly recorded fixed-cap capture refusal; neither establishes qualification.
Ordinary argv callers still use the real MacOs observer and refuse exhausted
captures. No process is omitted from that route and no cap changes. `effigy rust:compile:release` proves
the production compile-away. All are covered by `effigy rust:test` and
`effigy rust:compile` besides.

Capture refusal errors include the transcript path and diagnostics from the
internal document; those fields may be null for shared-wire truncation. The embedded exercises copy useful transcripts beside their unique
fixture directories within the caller's run root before fixture teardown, so
both complete synthetic controls and insufficient real-host captures are retained.
The tests do not qualify capture, comparison, native behavior or parity.

Two errno-capture controls use a safe metadata lookup on an empty path to set
and verify `ENOENT` before checking that the earlier captured error is recorded.
The lookup creates no fixture or file; the captured-error assertions remain.

## What this does not prove

The tape and harness are apparatus, not evidence. No parity claim, no native
run, no live pool, root, registry or account read, and no comparison against
Oxide exists; that is the X-S2 slice's separately dispatched work at a
source-approved implementation head, with the counted native its own
separately activated Queue proof. A recorded transcript here never authorizes
a deletion, a custody conclusion or a live Builds comparison.

## Observation identity limits

The internal recorder marks individual observations according to their
available identity; the shared-wire adapter does not emit that internal flag.
The window remains incomplete independently of the engine result. Fully identified process rows and classified/retry process observations,
and scalar uid, boot and complete process-table observations can be marked proven; refused rows and families without audited
identity rules remain unproven. It cannot satisfy mandatory
coverage or establish parity. Path observations do not infer a parent with a
later filesystem lookup. The macOS child-file reader records its checked
parent and opened-file device/inode with the bytes digest after its existing
stability checks. The macOS child-stat leaf emits one present, absent or
refused row for its `fstatat`, with the observed parent identity and, when
present, the child's device/inode. Handle listings and descriptor facts carry
the device and inode actually supplied by the handle or descriptor; a pathname and parent
identity are not invented. A non-UTF-8 path is not converted to a lossy
identity; an unrepresentable component is refused. Refusals are not successful
observations.

The unchanged `state::read_bounded_inner` reads metadata and opens the target by
path; it does not expose an observed parent descriptor. `StateLock::shared_if_present`
opens its target by path and an absent open supplies no parent identity.
`reference::read_receipts_with` receives a `ReadDir` and entries but does not
expose a parent descriptor identity for those entries. `MacOs::dir_facts` and
`recheck_dir` observe a descriptor's own identity but `DirHandle` carries no
path or parent identity. These sites cannot establish the required binding by a
later stat or a lexical path. A separately reviewed change would need to carry
an already opened parent descriptor and its identity through these existing
operations, binding each child observation to that descriptor and recording
its actual result, without a second lookup; no such change is authorized here.
Tom's decision d2c34987 scopes parent-descriptor instrumentation to separate,
independently reviewed work. The tape records the evaluator's classified identity and each retry identity
as separate ordered rows, and records each returned descriptor number and type. Vnode rows preserve the device and inode returned in the descriptor buffer,
but remain unproven and do not inherit either process identity: the retry path can switch process identities while the caller retains its
original classification, and neither identity proves the later observation. The
write-side recorder captures attempts and outcomes for the four reviewed
DirOps verbs, including publish writes, rename and cleanup. Atomic state
publication also records its create, write, rename and cleanup attempts, and
exclusive lock opens are recorded as attempts without inferring creation. Child effects
carry the already-open parent handle identity. File and directory sync results
are separate observations rather than fabricated write effects. The concrete
plan and publication paths are inventoried below; unbound effect identities
remain insufficient for comparison. The source-count test is not a
compile-away proof.

The recorder keeps the original path privately to bind a syscall result to its
attempt, including when the path is not UTF-8. It does not serialize that
private matching value: an unrepresentable effect path remains absent from the
internal document and cannot establish a complete effect identity. The
non-UTF-8 outcome control does not qualify that identity.

The declared L1 observation calls `File::metadata` once on an already-held
lock descriptor. Failure records the error and leaves the lock held with its
identity unproven. Area and intent entry recorders do not stat a pathname
after listing; receipts entries can carry the earlier precheck as `extracted`
context, not as an observed parent. The absence proof's stored `proved_ms` is
recorded without another clock read. Occurrence keys use structured identity
fields so delimiter characters do not merge distinct observations.

There are additional known apparatus gaps. `missing_obligations` can compare
an internal source-site and identity tuple to proven rows, and refuses
to satisfy one with an unproven or refused row. A provisional projection of
`mandatory_observations` reports missing and unmapped manifest indices after
capture, while its status remains `insufficient` regardless of matches. Unknown
identity fields, unsupported volume binding, missing input and the 256-key cap
are explicit; no field is dropped to produce a positive match. This projection
does not reconcile extra reads, dynamic facts or effects and is not a complete
shared-schema reconciliation. It does not establish obligation coverage. The
harness emits a
bounded apparatus-refusal document for manifest parse, case-mismatch,
unreadable manifest, state-path resolution and window-open failures once an
output path is known. Malformed argv also writes one when it identifies exactly
one unambiguous transcript destination. It records that the engine did not run; an unwritable
output or missing output argument cannot carry a document. Its internal
diagnostic fields are not the reviewed shared-wire shape. If
effects, clocks and the result exceed the document cap after all read rows are
dropped, `finish` emits a bounded refusal with omitted counts and original
engine status. It carries the original outcome when that fits within a
64-KiB summary bound; otherwise it records its digest and explicitly marks it
omitted. That fallback is an internal diagnostic, not shared-wire conformance.
Any exhausted capture cap or dropped read causes an apparatus error after the
transcript is written; the bounded fallback preserves the engine outcome or
its digest and omission marker as described above. If a completed
report cannot be serialized, the document records its omission and the real
engine status and exit code; it does not invent a report.
The internal recorder's document byte count is its serialized length, including
the length field itself. When that document is too large, deterministic tail
batches are omitted and counted rather than reserializing once per row; the
result is explicitly truncated and is not coverage.

The Oxide 283 parser validates the reviewed shared manifest's structure,
including `engine`, `site`, `identity` and `parent_required` on obligations.
The internal projection keeps unsupported identity fields unmapped; it does
not turn a volume or context mismatch into a match. The parser's successful
structure check is not completeness or runtime coverage.

The shared manifest's `expected_effects` entries have kind, site, path and attempted
but no engine. A provisional diagnostic treats only an explicitly empty
array as unambiguous: every observed Barge write attempt, including a failed
one, is listed as unexpected. Missing input, a set beyond 8192 entries, and a
nonempty set are reported separately; a nonempty set is not silently assigned
to Barge. All these diagnostics remain insufficient and are not dynamic effect
reconciliation or proof that no unrecorded write occurred.

The shared parser does not fully validate all apparatus-error and auxiliary
substructures. No successful comparison follows from these gaps.
The export receipt and binding checks are source only: until the private route
is qualified and the exact-head export is built and independently checked,
they are not evidence of an actual staged executable or source binding.

Descriptor listing, entry and vnode rows without operation-time process
identity remain unproven. The tape does not reuse a classified or retry
identity for a later listing or vnode read. The existing `state::sync_directory`
open-and-sync result is recorded without another stat; its identity remains
unbound, and an earlier publish stays an effect if sync fails.

## Production statement audit

The O1 pin `4af9360bd09629e970c143080ca159de8eef2562` and the planning
base `39064bafbc14b2ba7bd876e286e9752f12583dd1` have no diff under `crates/`,
workspace manifests, lockfile or toolchain. Source review at this branch restores
non-test match arms and expressions at the instrumented read sites. A line-level
O1 diff has no removed source lines after these splits; that check alone does
not prove that newly added statements compile away or preserve behavior. A
per-statement equivalence review and release compile remain required before
acceptance.

The O1-to-planning difference is not empty across the entire repository:
`scripts/native/release-proof.py` changed in commits `56b067b`, `b966777` and
`3a21036` for the native release-proof dependency guard, remount replay and R12
fixture thresholds. It is a separate executable proof apparatus, not the
library evaluator; there is no additional change to that script between the
planning base and this branch. Those historical changes are not evidence for
this tape or permission to execute a native proof.

Those script deltas are executable: staging drops inherited Rust flags and
disables automatic toolchain installation; fixture startup scrubs inherited
Oxide, XDG and Barge variables and explicitly selects the synthetic account;
attach marks custody before invoking `hdiutil`. R11 now saves content, marker
and identity, validates remount-relative rebinding and rollback, and stops on
unknown detach custody. R12 first creates a fresh private registration only
after R11 passes. Dispatch marks dependent cases skipped, distinguishes
unknown or unexecuted cases, retains unknown custody without repair, and adds
pure controls for the rebind and dispatcher. These are prior proof-apparatus
behavior changes, not non-test library statement equivalence.

The current source map against that O1 pin, for reviewer inspection, is:

| File | Source sites changed | Non-test basis and remaining review |
| --- | --- | --- |
| `area.rs` | `VolumeRoot::open_with`, `resolve_by`, `read_directory`, `read_hold_directory`, `list_names`, `scan_activity_within` | Volume verification/open and resolution keep the O1 calls and returns, with test-only observations after results. In `read_directory`, the original NotFound arm and original error return are retained in `cfg(not(test))`; the test arms retain the same return and error, and other additions only record already obtained results. `read_hold_directory` records the existing uid and metadata results. `list_names` preserves the original missing-directory return in its non-test arm and reconstructs the same return in its test arm; the completed-list observation precedes the unchanged result. `scan_activity_within` records immediately before its existing incomplete and final returns. |
| `custody.rs` | `take_custody_by`, `decide` | The O1 `Ok(evidence)` and two `Err(Refusal::...)` match arms remain verbatim in `cfg(not(test))`; test arms record then return the same constructors. `decide` adds one test-only call before its original `Ok(CustodyEvidence { ... })`. |
| `identity.rs` | `read`, `verify` | The original `read_child_file` result is recorded after its `?` and before the original `child_facts`. The O1 `Ok(None)` and `Err(_)` arms remain verbatim under `cfg(not(test))`; the test arms return the same unknown reasons. Duplicate UUID, missing mount and final compare retain their original order with test-only calls immediately before their existing statements. |
| `os/absence.rs` | `prove_absence`, `prove_absence_inner` | The O1 `let proof = prove_absence_inner(...)?` remains verbatim in the non-test branch; the test branch calls it once and returns the same error. Serialization and cap checks remain in their original order, with test-only refusal records. The original proof clock expression remains in the struct; the test row reads the already stored `proved_ms`. The added regression is test-only. |
| `os/macos.rs` | mount/volume, process and descriptor observations; `DirOps` | Mount census, lookup, boot and root checks keep the original syscalls and returns with test-only observations. `effective_uid` retains the original `geteuid` expression under `cfg(not(test))`; its test branch calls it once and records the returned uid. Process/descriptor additions record existing rows, errno values and retry identities. `path_facts`, `child_facts` and `open_directory` retain the original `last_os_error()` returns under `cfg(not(test))`; their test branches capture errno immediately and return through the original helper. `create_child_dir`, `publish_child_file`, `dir_facts`, `recheck_dir` and `read_identity` retain original non-test operations in cfg splits; test branches call each operation once. For publish the original `write_all`, file sync, `renameat`, directory sync and cleanup `unlinkat` are individually preserved in non-test branches, with corresponding single-operation test branches. Removal and rename retain original syscalls and record attempts/results around them. Directory listings and child reads record already obtained entries, handles and bytes. This is a source map, not a compiled cfg-equivalence proof. |
| `pool_custody.rs` | `classify`, `read_document` | The classification loop adds a test-only row before the original duplicate check. The three O1 `read_bounded` arms remain verbatim under `cfg(not(test))`; test arms record the consumed outcome then return the same value. The original `if identity(path)? != before` remains verbatim in the non-test branch; the test branch evaluates `identity(path)?` once, records it, then makes the same comparison and return. |
| `reference.rs` | `BargeReferences::read`, `control_directory_with`, `pools_holding`, `read_registry`, `read_receipts_with`, `read_oxide_marker` | Original placement-read, removable-pool metadata match, registry read and parse matches, and both marker child-facts/read matches remain verbatim under `cfg(not(test))`. Their test counterparts perform each original call once and return the same values and errors, adding observations. `control_directory_with` records its stored inspect result. The receipt `Ok(Some(receipt))` push remains verbatim in the non-test arm; the test arm records then pushes the same receipt. Other receipt additions read the already obtained path's component and add no filesystem operation. |
| `registry.rs` | `read_with_deadline` | The complete original `let bytes = match state::read_bounded(...)` and `let document = serde_json::from_slice(...).map_err(...)?` statements remain verbatim in `cfg(not(test))`. Test branches use the same read and parser once, record their consumed outcomes, and return the same named errors. Validation follows both branches unchanged. |
| `state.rs` | `read_bounded_inner`, `publish_atomic_with`, `write_and_publish`, `sync_directory`, `now_ms`, `StateLock`, `wait_for_lock`, `read_intents_with_deadline` | The O1 NotFound arms of bounded read, shared lock and intent directory read remain verbatim in non-test branches; test arms return the same absent values after recording. Other read and lock additions observe existing values or error branches. Publication retains the original create, cleanup unlink, write, file sync, rename and directory-sync statements in non-test branches; test branches execute each original operation once through the recorder and retain its error handling. `now_ms` retains its original expression under `cfg(not(test))`; the test branch evaluates it once and returns the recorded value. Intent enumeration uses the existing entry name and preserves its sorted result. |
| `release.rs` | no tape delta against O1 | Caller clock observations come from `state::now_ms`; release's original calls remain in place. |
| `lib.rs`, `Cargo.toml`, `tests/tape_eval.rs`, `tape.rs`, `tape_harness.rs` | test-only modules, harness target and recorder | The O1 library declarations remain intact; `lib.rs` adds only `cfg(test)` crate-private tape and harness modules. The Cargo addition is a `[[test]]` target, with no dependency or feature change. `tests/tape_eval.rs` includes the existing library source as test-target modules; the two new recorder modules are test-only. Release compile-away remains unverified. |
| `effect.rs`, `guard.rs`, `undo_capture.rs` | source-link shadows for the included test target | These are Git symlinks respectively to `space/effect.rs`, `space/guard.rs` and `cache/undo_capture.rs`. The original production child-module declarations remain in their original parents; `lib.rs` adds no top-level declarations for these shadows. They resolve the same existing bytes for the included test-target parent modules. Their actual target/build reachability remains unverified until compilation. |

This map identifies the changes for review; it is not a completed proof that
every newly added item compiles away. In particular, a textual diff cannot
prove cfg reachability, and no current-head release compile has run.

The named write helpers are outside the direct plan body: `release::plan` calls
`plan_observed` (release.rs:358, 387), while effect execution starts at
`release::execute` (release.rs:1854). `identity.rs:122,284,318` and
`area.rs:1297,1745,2259,2261,2361` locate random and sync operations in
identity creation and area mutation paths, rather than by themselves proving
plan reachability. `set_dir_mode` and `create_child_symlink` are DirOps mutation
implementations in `os/macos.rs:4362,4403`. Their absence from the concrete
plan call path follows from the trace below, not merely from their names.

The observed lock branch is narrower than the exclusive mutation branch:
`release::read_registry` calls `LockSet::observe_registry`, and
`plan_observed`/`decide_root` call `observe_volume`, `observe_root` or
`observe_unit` (release.rs:747, 514, 800). Those methods lead to
`StateLock::shared_if_present` (state.rs:970, 1182–1225), whose `OpenOptions`
has `read(true)` and no create/write flags; an absent lock returns `None`.
That does not reach `StateLock::exclusive`'s create/open or the publication
helpers. `release::plan_observed` calls `area::show`, `resolve_by`,
`list_names`, `BargeReferences::read`, `OxideSnapshot::read_within`,
`pending_intents` and `decide_root` (release.rs:404–550). `area::show` reads
declarations (area.rs:2867, 3167), while `resolve_by` uses
`walk_folder_chain` (area.rs:1041, 1509); neither call selects a creation
entry point. Their descendants and the concrete platform callbacks are traced
below; the tape records a failed write as possibly partial rather than asserting
that it changed nothing.
The placement census branch goes from `BargeReferences::read` to
`cache::read_placements` (reference.rs:277; cache.rs:676), which enumerates
via `area::read_directory`, reads bounded placement documents, validates and
returns the set; it does not call the cache adoption or publication methods.
The Oxide snapshot branch (reference.rs:628) calls `control_directory`,
`read_registry`, `read_receipts`, `pools_holding` and `PoolCustody::read`;
the latter reads pool records through `read_pool` and `read_document`
(pool_custody.rs:129, 192, 394). This branch is observational and does not call
the state publication helpers. Root custody goes through
`custody::take_custody_by` → `decide` (custody.rs:403, 644), including hold,
marker, identity and process-observer checks. The platform leaves are traced
below; refusals return from these read paths without a cleanup write.

Within `custody::decide` (custody.rs:644), the state operations are hold and
registry reads. It opens the area and parent, obtains child facts and a
directory handle, reads the marker, asks for the current pid/start and scans
descriptors; it does not call a `DirOps` create, publish, rename, unlink or
sync operation. Its early returns do not run cleanup writes. The area folder
walk is `walk_folder_chain` → `walk_folder`, using child facts and directory
opens (area.rs:1041); it reports the remainder when absent, it does not create
it. `StatePaths::resolve` also constructs paths without initialization
(state.rs, `StatePaths::resolve`); its separate `initialize` method is not
called from the harness or plan. At the macOS `DirOps` boundary, `open_path`
opens a directory; `path_facts` uses `lstat`; `open_child_dir` uses `openat`
with `O_RDONLY | O_DIRECTORY | O_NOFOLLOW`; `child_facts` uses no-follow
`fstatat`; `read_child_file` uses read-only no-follow `openat` and reads; and
`list_children`/`list_names` duplicate the descriptor and use `readdir`
(os/macos.rs:3647–4263). `dir_facts` and `recheck_dir` only query the open
descriptor (os/macos.rs:4342, 4383). The mutation methods in that implementation
are separate: create/publish/remove/rename (os/macos.rs:3803, 3831, 4264,
4296), plus `set_dir_mode` and `create_child_symlink` (os/macos.rs:4362, 4403).
The concrete harness supplies `MacOs` for every platform trait, rather than a
caller-provided mutation callback. Its descriptor scan starts with
`process_table` and `scan_processes` (os/macos.rs:958, 1015, 2817); the
production call supplies an empty fault list. Its process table and individual
identity `sysctl` calls pass a null new-value pointer and zero new-value size,
and descriptor enumeration uses `proc_pidinfo`/`proc_pidfdinfo` and
`csops(CS_OPS_STATUS)`. Retry paths re-observe or sleep; they do not write a
fixture. The process-table allocation before the cap remains separately
unbounded and is not qualified by this source review. Volume identification
uses read-only directory opens, `getfsstat`, `fstatfs`, `fstat` and volume UUID
attribute reads (os/macos.rs:176–415, 445–505, 4480–4680). The volume-flush
fork and sync path (os/macos.rs:552 onward) is a separate operation, not called
by those identification methods or `release::plan`.

For this concrete `release::plan` window, the reviewed branches therefore do
not call a data-mutating operation: its effect set is empty if that source
trace holds at execution. The excluded publication/effect entry points still
record their attempts and outcomes when exercised by tests; an attempted write
is never removed to make a transcript empty. This is a static source finding,
not a runtime receipt or proof that the fixed tape captures every read.

The separate state publication path is `publish_atomic` →
`publish_atomic_with` → `write_and_publish` (state.rs:654, 660, 763). It
records temporary-file create, `write_all`, and rename attempts with their
actual outcomes; a failed save retains possible partial effects. Between the
save and rename, the test branch calls `file.sync_all()` once and records its
returned result at `state::write_and_publish`, without a path stat or inferred
descriptor identity. The original non-test statement remains verbatim. A
successful rename remains an effect even if `sync_directory` subsequently
fails (state.rs:750, 843); the directory sync result is recorded separately.
Before rename, an error triggers a recorded best-effort temporary-file unlink;
its failure does not replace the original publication error. `StateLock::exclusive`
(state.rs:946) records the create/open attempt and outcome; the concrete plan
uses only shared observation locks. The corresponding descriptor-relative
publish path records temporary create, save, file sync, rename, directory sync
and any cleanup unlink in `MacOs::publish_child_file` (os/macos.rs:3831–3963).
These remain source findings and do not establish native qualification.

Effect rows carry the call-site file and line separately from the closed
effect kind. A failed operation records its actual error and
`partial_effect_possible`; in particular, `write_all` does not report whether
earlier writes succeeded before an error. This preserves uncertainty instead of claiming
either a successful full save or no mutation. The row and field are provisional
wire details pending shared-schema review.

The ordinary test suite includes
`ordinary_account_tests::an_ordinary_build_takes_oxide_directories_from_the_password_database_home`.
Tom authorized that one test's read-only password-database lookup by decision
`d130b2b6-04b0-4e25-b21c-4a6b4b71ec9d`, under the private wrapper and live-state
inventory gate. The ordinary suite keeps the proof feature off; the ruling does
not authorize other live account reads or redirect production account lookup.

`MacOs::process_table` first takes the kernel's process-table byte count, adds
slack and allocates its buffer; its `SCAN_PID_CAP` check follows the second
sysctl. The tape's 512-row cap and the scan's deadline do not bound that prior
allocation. Consequently the process-census capture does not establish the
accepted allocation bound. A bounded production allocation requires separate
design and review; the tape does not inject a limit or alter evaluator flow.

### Shared-wire source and limits

The test-only wire parser and canonical vector files are adapted from Oxide
commit `a35aef84bc3b8cb0ac943dfa005456a45167399b`;
`src/bin/oxide/capture_wire.rs` there hashes to
`6c0d4b47779af389f33072a7e397f5799a27002282bc8b23436ea617c1b77c8f`,
and its vector `SHA256SUMS` hashes to
`bb50822c3393bb0cb9e8bc53142c821dd1cdfa1a4d4828186c7b90d375099091`.
The local copy uses Barge's digest implementation and local vector paths. It
is compiled only in the test tape. The parser checks structure; its vector
controls do not establish runtime coverage or parity. Clocks, classifications,
truncation, notes and fixture-capture substructures remain outside its full
validation. Historical packets with provisional field names are not migrated.

Current recorder identities use `parent_source` and `own_source`: a parent
held for a descriptor-relative operation is `handle`; the separate receipts
precheck is `extracted`; other contextual facts are `context`; missing facts
are `none`. Those labels describe observation provenance, not proof of K1.
The K1 lifetime premise and mandatory parent obligations remain unqualified.
Process descriptor rows lacking an identity at the operation remain unproven;
an earlier classification is not reused as a later listing identity. L1
adds one metadata call on a held lock; failure leaves the lock held and its
identity unproven. Schema conformance alone does not establish full capture.

| Remaining obligation | Current limit |
| --- | --- |
| K1 lifetime | Held parent identity is recorded, but lifetime and non-reassignment across the required interval are not qualified. |
| Direct-reader parents | Path prechecks and opens resolve separately; their parent remains unproven. The per-caller compatibility disposition (18 read sites, 2 lock sites) is in `oxide-capture-parent-identity.md`, "Route B compatibility disposition"; it is a source-backed recommendation to defer, pending planner ruling R-K2. |
| Process and descriptor binding | A listing or vnode read without identity observed at that operation remains unproven, even when a classified pid was read earlier. |
| Complete shared reconciliation | The copied parser and vectors check their declared surfaces; clocks, classifications, truncation, notes and fixture capture are not fully validated. |
| Real capture qualification | The retained host exercise is bounded evidence, not native, parity, producer, or adoption qualification. |
