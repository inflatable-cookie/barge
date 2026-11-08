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
  returned, and feeds nothing back. The module writes two documents: its own
  `barge.capture.recorder/1` diagnostic (caps, per-row provenance flags, the
  fixture capture and the notes) and, through the adapter below, the reviewed
  `oxide.capture.transcript/2` capture. The recorder document is not a capture
  schema and is never proof input.
- `crates/barge/src/capture_wire.rs` — the copied, reviewed shared-wire parser
  and completeness rules for `oxide.capture.transcript/2` and
  `oxide.capture.manifest/2`, with the hashed `/2` vectors under
  `crates/barge/tests/fixtures/capture-vectors/v2/`. It is a `#[path]` child
  module of `tape.rs`, compiled only in the test tape.
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
  bytes (the measured `window_elapsed_ns` aside), the apparatus diagnostic, the
  caps (read rows, per-key occurrences, process rows, the 8 MiB document), the
  no-public-surface declaration shape, the emitted `/2`-shape conformance
  regression, the delegated completeness controls, the fixture-capture net
  change, the PCF-4/PCF-5 controls, and a limited source guard check. That
  check does not establish per-site coverage; the mechanical compile-away check
  is `effigy rust:compile:release`.
- `crates/barge/src/tape_harness.rs`'s tests: the embedded end-to-end exercise
  (one exclusively owned directory fixture on a real selected volume, the
  actual `release::plan`, one completed window over a settled root with a
  positively absent checkout, and one refused window whose transcript is
  still written).

## What the tape records

The instrumentation includes the following source symbols; it is not a
complete source-site audit. Process leaves (`MacOs::process_table`,
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
cap. The wire's narrower identity key then truncates the same families again:
the retained real-host window reports `read_rows_truncated`,
`process_rows_truncated` and `occurrences_truncated`, emits about 1,860 of its
8,192 recorded rows, and is therefore insufficient by rule. That is a finding
about the shared key, recorded under "Shared-wire source and limits", not a
relaxation of any cap. This is the leaf tape recording exactly what the scan really reads, as
the contract requires; the consequence for the later X-S2 windows is theirs to
measure and declare — a counted comparison on a real host must either run in a
machine state whose custody scan fits the fixed caps or record its truncation
honestly (and then be insufficient). Nothing here relaxes a cap.

## The transcript

`/2` is the current proof input (Oxide 284, merged at
`923d01f803cc7708569985cd5217edc294a1189c`). A `/1` transcript or manifest is
refused by name and never upgraded; the `/1` fixtures and their hashes stay
unchanged under `crates/barge/tests/fixtures/capture-vectors/` as history and
are refused as current proof inputs.

The harness emits one conforming `oxide.capture.transcript/2` document per
window, including an engine refusal, from the recorder's rows:

- `schema` and `version` are both `oxide.capture.transcript/2`; `engine` is
  `barge`; `manifest_sha256` binds the whole manifest; `entrypoint` names the
  called symbol; `window_monotonic_start_ns` and `window_monotonic_end_ns` are
  explicit `null` because no absolute origin is proven, while
  `window_elapsed_ns` is the recorder's real measured elapsed time.
- `outcome` is `completed` with the engine's own report object, or `refused`
  with the engine's refusal reason, exit code and `source: engine`.
- `reads` rows carry `kind` equal to the observed `site`. The wire requires the
  two to agree, and one Barge source symbol records several leaf families, so
  Barge's wire site is `<source symbol>#<observation family>` (for example
  `MacOs::dir_facts#stat`, `state::read_bounded_inner#file`). The recorder
  document keeps the bare symbol and the family separately, and a manifest
  obligation for Barge must name the same string. Every row also carries a
  complete 15-field identity (`null` where a fact is missing), one of the
  shared outcome shapes, `cap` as `null` when no cap applies (a real `0` stays
  `0`), `epoch` and `occurrence`. The recorder counts occurrences per complete
  internal identity (which includes fd, pid, start, uid and volume UUID); the
  wire key is narrower, so the adapter renumbers occurrences per emitted key in
  emitted order and stops emitting rows for a key at the shared 64 cap,
  recording `occurrences_truncated`. Every row keeps its outcome and epoch, and
  the recorder document keeps every recorded row.
- `effects` rows are attempts (`result: unknown`) or observed results.
  `succeeded` is only a completed observation. Any error is recorded as
  `partial`, because this recorder cannot prove that no bytes were written, so
  it never claims `failed`; that vocabulary remains a parser and completeness
  shape exercised by the vectors. A row whose path cannot be represented is
  emitted as `descriptor-effect` with no path and its observed result, so its
  identity stays unproven rather than being invented.
- `clocks` rows carry the `now_ms` value that was returned to its caller.
  `classifications` is empty because this evaluator keeps no separate
  classification family.
- `recorded_holders` rows carry the record id, pid, recorded start, and an
  explicit `null` `kernel_start` and `uid` unless this run observed them.
- `fixture_capture` is the apparatus's bounded pre/post walk of the owned
  fixture tree (4096 entries, 32 MiB of hashed bytes) with the wire identity
  names `device` and `inode`, plus the net change as `fixture-diff` effects:
  `attempted: false`, `epoch: 0`, `result: unknown`. A diff corroborates a
  persistent change; it never replaces a temporal observation, and a truncated
  capture is a completeness issue.
- Epochs start at one, are assigned at the leaf and are unique across reads,
  effects, clocks and holders.

Completeness is not reimplemented here. After emission, the harness calls the
copied shared `capture_wire::assess_completeness` over the emitted document and
the manifest, and reports the resulting issues in its run summary. The
in-window identity and effect-parity rules are the shared parser's own
functions, so no second projection can diverge.

Two distinct diagnostic documents are never captures:

- `oxide.capture.apparatus/2` (`schema`, `version`, `case`, `manifest_sha256`,
  a bounded `reason` and the reason's `reason_sha256`) records a refusal
  before the engine ran, or a fallback where the engine's report could not be
  carried into a conforming transcript. The transcript parser refuses its
  schema by name.
- `barge.capture.recorder/1` is the recorder's own diagnostic document. It is
  where the caps, the per-row `observation_proven` flag, the effect parent and
  rename-destination facts, the clock site, the digest size and the fixture
  pre/post arrays that have no `/2` field are kept. It is written beside the
  apparatus document (`<transcript>.recorder.json`) when a conversion fails;
  it is never written under a capture schema.

The copied Oxide 284 parser validates the strict `/2` manifest (typed source
binding, fixture entries by kind, complete obligation identities, mandated
`parent_required`, `attempted` on every declared effect, window order). The
harness then extracts `case`, `barge.volume_uuid`, `barge.namespace`,
`barge.area`, and optional `barge.named_roots` to invoke the engine, and binds
the complete manifest digest. Missing or mistyped required members cause an
apparatus error. Engine refusals keep the engine's own reason and exit code.

Known shared-wire limits, stated rather than worked around: a volume row can
observe a numeric device without an inode, and the wire's device/inode pair is
both-or-neither, so that number stays in the recorder document and the wire
states no own identity; effect parent identity and rename destinations, the
clock site and the digest's byte size have no `/2` field; and the surface never
carries an internal provenance flag.

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

Capture refusal errors include the transcript path and its truncation flags; a
refusal before the engine ran writes the distinct apparatus diagnostic, which
names its bounded reason and the reason's digest. The embedded exercises copy
useful transcripts beside their unique fixture directories within the caller's
run root before fixture teardown, so both complete synthetic controls and
insufficient real-host captures are retained. The tests do not qualify capture,
comparison, native behavior or parity.

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
The completeness rules the harness applies are the shared parser's own, and
`parent_required`, a holder without a kernel start, an unvalued clock and an
unproven window origin are all named issues. The window remains incomplete
independently of the engine result. Fully identified process rows and classified/retry process observations,
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
path identity, so the wire row names the handle's observed identity as its
`context` parent and never a pathname. These sites cannot establish the
required binding by a later stat or a lexical path. A separately reviewed change would need to carry
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

There are additional known apparatus bounds. Completeness is no longer a
Barge projection: the harness applies the copied shared
`assess_completeness` to the emitted `/2` document and manifest, so a missing
obligation, a wrong identity, an unresolved `parent_required`, an unproven
holder lifetime, an unvalued clock, an unproven window origin and any effect
parity failure are reported by the shared rules, never by a second copy. That
report is a run-summary diagnostic; the transcript itself carries only the
shared fields.

The harness writes a bounded `oxide.capture.apparatus/2` diagnostic for manifest
parse refusal, case mismatch, an unreadable manifest, state-path resolution,
window-open failure, an unserializable engine report, and a recorder document
that cannot be converted or exceeds the document cap. Malformed argv also writes
one when it identifies exactly one unambiguous transcript destination. The
diagnostic records that the engine did not run (or its real status when it did),
with a bounded reason and the reason's digest; an unwritable output or missing
output argument cannot carry a document. It is never a transcript. Any exhausted
capture cap or dropped read causes an apparatus error after the transcript is
written.

The recorder's document byte count is its serialized length, including the
length field itself. When that document is too large, deterministic tail
batches are omitted and counted rather than reserializing once per row; when it
still does not fit, the result is the bounded apparatus diagnostic, which names
the engine status, exit code, outcome digest and the omitted counts. A truncated
recorder document was never written under a capture schema.

The copied Oxide 284 parser validates the reviewed shared manifest's complete
structure: typed source binding, fixture entries by kind, the four obligation
identity alternatives, mandatory `engine`, `site`, `identity` and
`parent_required`, declared `attempted` on every expected effect, window order
and every cap. Its successful structure check is not completeness or runtime
coverage, and it is the same parser and reconciler implementation the vectors
exercise.

The shared manifest's `expected_effects` entries have kind, site, path and
`attempted`. Only a declared effect with `attempted: false` can be completed
parity; a declared attempt is named and never matched. An observed attempt, a
`partial` row or an unpathable descriptor effect is not completed parity, and a
repeated completed row is an extra effect. The run summary reports those issues;
it does not claim dynamic effect reconciliation.

Effect rows carry the call-site file and line separately from the closed
effect kind. A failed operation records its actual error in the recorder
document and `partial_effect_possible`; in particular, `write_all` does not
report whether earlier writes succeeded before an error, so every recorded error
is emitted as `partial`, never as `failed` or as a completed effect.

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

The test-only wire parser, its completeness rules and the canonical `/2`
vector files are adapted from Oxide 284, merged at
`923d01f803cc7708569985cd5217edc294a1189c`:
`src/bin/oxide/capture_wire.rs` at that commit hashes to
`9a80201bb603e7fc04a9ac3eb2b6b10627cf8b5f097cb671b55482683acb5c80`, and
`docs/knowledge/contracts/capture-vectors/v2/SHA256SUMS` hashes to
`e7018741921f795bc461c96b23a552ddf2d1e1752ff78e70b650104749380d76`
(48 files: eight valid transcripts, eighteen parser refusals, twelve
parser-positive completeness failures, three valid and seven invalid
manifests). The two canonical manifest examples are copied too:
`capture-examples/v2/manifest-example.json` hashes to
`8025455c4f84bb7640ba2b802bee48282ca09a7148ccc595a627512ed0c42f33` and
`manifest-effects-all-kinds.json` to
`517afc326b27be3e66297a5ebbb9d90dc21342153b1ecb9cded2488f6f5286bf`.
Barge's copy differs from that source only in its digest implementation
(`crate::digest::sha256_hex` instead of `sha2`) and its local vector paths, plus
the `expect`-free base-vector accessor the crate's lint policy requires. The
local `SHA256SUMS` copy hashes identically to the source's, so the catalog still
binds every vector byte.

The parser validates the whole `/2` transcript and manifest structure,
including truncation flags, identity pairs and provenance, effect results,
holder and clock shapes, fixture entries and the manifest's typed source,
fixture, dynamic, obligation and expected blocks. Its own vector controls, and
Barge's emitted-shape regression, use those same bytes; they do not establish
runtime coverage, parity or completeness. The `/1` fixtures and their
`SHA256SUMS` are retained unchanged as history and are refused as proof input.

Current recorder identities use `parent_source` and `own_source`: a parent
held for a descriptor-relative operation is `handle`; a site's own production
precheck stat is `extracted`; the R2/S8/R8 links to an already-open handle are
`context`; missing facts are `none`. Those labels describe observation
provenance, not proof of K1. The K1 lifetime premise and mandatory parent
obligations remain unqualified. Process descriptor rows lacking an identity at
the operation remain unproven; an earlier classification is not reused as a
later listing identity. L1 adds one metadata call on a held lock; failure
leaves the lock held and its identity unproven. Schema conformance alone does
not establish full capture.

Two accepted observation deviations from the 042/045 apparatus are closed here
because current source supplies the fact, and no other filesystem operation was
added for them:

- `area::read_directory` entry rows now bind the parent from that site's own
  production `symlink_metadata` precheck as `extracted` (`Identity::file`'s
  own-fact rule is unchanged; no second lookup is made). Its precheck refusal
  and non-directory refusal keep their existing rows.
- R2 (`release::decide_root`'s root open) records a `context` link from the
  opened root's own observed identity to the identity of the area handle the
  resolution already holds; S8 (`MacOs::duplicate_dir`) and R8
  (`MacOs::dir_facts`, `MacOs::recheck_dir`) record the descriptor's own
  `handle` identity with the source handle's identity as their `context`
  parent. The shared wire has no epoch-reference field, so the link is the
  observed identity and its provenance, not an epoch cross-reference; a
  mandatory parent stays unproven.
- PCF-4 and PCF-5 are pure controls over recorded rows:
  `laundered_parent_rows` names a row that claims a `handle` parent at a
  direct-read site, and `incoherent_handle_rereads` names a handle
  self-observation whose own identity differs from its handle context. Both are
  tested with real recorder rows and with a doctored row, and neither repairs or
  suppresses anything.

| Remaining obligation | Current limit |
| --- | --- |
| K1 lifetime | Held parent identity is recorded, but lifetime and non-reassignment across the required interval are not qualified. |
| Direct-reader parents | Path prechecks and opens resolve separately; their parent remains unproven. The per-caller compatibility disposition (19 read call sites in 18 functions, 2 lock sites) is in `oxide-capture-parent-identity.md`, "Route B compatibility disposition". Tom's answer to decision `0bcedf55` v2 defers the production reader change; R-K2 is not re-asked, and no C1/C1-prime/C2 conversion is activated. |
| Process and descriptor binding | A listing or vnode read without identity observed at that operation remains unproven, even when a classified pid was read earlier. |
| Recorded holders | This plan does not parse a structured holder object (pool custody ignores it), so real windows emit an empty array; the row shape and its explicit null kernel start and uid are exercised only by the emitted-shape control. No holder lifetime is proven. |
| Volume device | A volume's numeric device has no inode beside it; the wire's both-or-neither pair rule keeps it in the recorder document. |
| Shared reconciliation in-window | The shared completeness rules are applied by the harness; the engine-side extra/unbound-observation rules are Oxide's X-S2 work and are not implemented here. |
| Collapsed wire identity keys | The wire's key is `site|path|component|parent_device|parent_inode`: it has no fd, pid, start, uid or volume UUID. A real custody scan's descriptor and process families collapse onto a handful of keys, so the shared 64-occurrence cap truncates them (the retained real-host window now reports `occurrences_truncated` as well as the read-row and process caps). The window is insufficient by rule and the recorder document keeps the dropped rows. Making those families reconcilable needs the shared key to carry process identity — an Oxide schema question, not something Barge can change from here. |
| Real capture qualification | The retained host exercise is bounded evidence, not native, parity, producer, or adoption qualification. Its window truncates at the fixed read-row cap and is therefore insufficient by rule. |
