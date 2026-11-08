# Descriptor-bound parent observations for Barge capture

This contract is the design for binding a parent directory's device and inode,
the child component, the actual descriptor identity and the real observed
bytes or result to one attributable operation, at every read site the actual
`barge::release::plan` chain reaches. It exists because the first capture-tape
cut bound parent identity with an untaped, later `symlink_metadata` of the
path parent — a hidden extra read whose result cannot be the parent of the
descriptor the engine actually read through — and because several read sites
resolve paths the kernel walks without any parent observation Barge can cite.

It fixes: what one attributable observation must bind and which provenance
classes a transcript row may honestly carry; the complete inventory of the
release planner's read sites with what each already observes, what is
extractable for free, and what each proposed binding adds; the exact
signatures, files and cfg reachability of the proposed tape additions; the
removal of the tape-side path-stat helpers the checkpoints left behind; the
two declared test-only observations with their cost and error semantics; the
proof of descriptor-relative binding and its precise limits under path and
parent replacement, symlinks, missing ancestors, inode reuse, renamed
parents, handle lifetime, errors and bounded retry; the counterfactuals that
keep the limits honest; the reconciliation obligations; and the finite,
repository-separated implementation slices.

This is a design only. It authorizes no implementation, no production or
public-surface change, no pin change, no feature, no selector change and no
new read in any production build. Separate code implementation requires its
own scoped brief based on this accepted design.

## Scope and authority

### Frozen inputs

- Tom's scope ruling and the authority list in [vision](../vision.md); the
  invariants in [architecture](../architecture.md#invariants).
- Tom decision `d2c34987` (answered 2026-11-02): complete parent-descriptor
  identity observations are a separate, separately reviewed follow-up; B-S1
  keeps affected rows unproven meanwhile and does not wait for this design.
- The approved O1→O2→O3 tranche `bb5bc0cf` and the accepted shared capture
  contract: Oxide's
  `docs/knowledge/contracts/barge-adoption.md`, sections "O2 capture
  boundary", "Fixture capture and comparison seam", "Leaf-observation tape:
  source feasibility audit", "Shared capture contract and implementation
  slices" (the schema names, caps and reconciliation oracle are that
  contract's, and this design extends them; where they differ, that contract
  wins and this file is corrected).
- The B-S1 slice as scoped in that contract: a `cfg(test)`-only leaf tape and
  in-crate harness, with the production build compiling zero new code and
  every preexisting production statement byte-for-byte unchanged.
- Oxide X-S1/PR230's proposed machine-readable manifest schema is
  **not canonical**: neither its unreviewed fields nor B-S1's provisional
  consumption surface is a wire contract. This design states binding
  semantics and required row members, and coordinates; it does not fix JSON
  layout.
- The primary production baseline is the O1 pin
  `4af9360bd09629e970c143080ca159de8eef2562`; this worktree's `crates/` has
  no diff from it, so every source claim below reads identically at the
  planning base `39064ba`.
- B-S1's PR41 checkpoints `226da715` and `227a76da` are immutable, unaccepted
  source evidence, inspected at the exact git objects for the inventory; they
  are not a reviewed API to assume, and nothing here adopts them wholesale.

### In scope

The parent/component/descriptor binding at the actual read sites reached by
`release::plan` — direct path readers, lock opens, enumerations, handle-child
reads and handle self-facts — including lock/re-read/error/absent paths; the
honest provenance classes a row may carry; the removal of hidden tape-side
reads; the descriptor-relative binding theorem and its limits; the
counterfactuals; the reconciliation obligations; the compile-away and
provenance rules; the slices.

### Out of scope

Any implementation, any production behaviour or API change, any change to
Oxide (its tape X-S1 and orchestrator X-S2 are Oxide slices), any pin change,
any native proof run, any live pool/registry/account read, the process-table
and per-descriptor custody leaves' process identity (already scoped in the
shared contract's B-S1 leaf table; this design only touches where those rows
name files and directories), and the parity question itself (O2 evidence is
X-S2's, separately dispatched).

## The binding problem

### The discovered defect

The first B-S1 tape commit `f54fd37` carried, in `crates/barge/src/tape.rs`:

```rust
fn parent_identity(path: &Path) -> (Option<u64>, Option<u64>) {
    match path.parent().map(std::fs::symlink_metadata) { ... }
}
```

called from the `Identity` constructors at row-record time. Three things make
it unusable, and they generalize:

1. **It is a hidden extra read.** The tape performed a real
   `symlink_metadata` that the engine's operation did not, produced no row of
   its own, and charged no cap — exactly the escaped untaped read the shared
   contract's mandatory counterfactual 1 forbids.
2. **It is later.** The lookup ran after the read it claims to describe. A
   parent replaced or renamed between the read and the record yields an
   identity the operation never saw.
3. **It is path-relative.** A lexical `path.parent()` resolves whatever that
   path names *now*; the descriptor the engine read through has no path. The
   result therefore cannot bind the parent of the descriptor actually read.

`226da715` removed the helper (the tape core's own test
`recorder_does_not_reopen_or_stat_a_parent` pins that the recorder never
reopens or stats a parent), marked every read row `observation_proven: false`
and every window `observation_complete: false`, and `227a76da` bound clock
rows to their caller sites and platform rows to their classified uid.

### What the checkpoints left

The same defect pattern survives at `227a76da` in two cfg(test) helpers
outside `tape.rs`, and several sites still carry no honest parent binding:

- `crates/barge/src/state.rs` `dir_facts(path)` and
  `crates/barge/src/area.rs` `dir_facts(path)` — each a cfg(test)
  `std::fs::symlink_metadata` executed **per enumerated entry**, at record
  time, to decorate the `dir-entry` rows of
  `state::read_intents_with_deadline` and `area::read_directory`. Untaped,
  hidden, per-entry cost, temporally after the enumeration opened, and
  path-resolved: the same three defects.
- `crates/barge/src/state.rs` `lock_file_facts(file)` — a cfg(test)
  `file.metadata()` on the already-opened lock file. This one is
  descriptor-relative (it cannot alias), but it is a **new** syscall the
  production run does not make, charged to no row of its own. It must be
  declared, not smuggled.
- Direct path reads carry no parent at all (`state::read_bounded_inner`,
  `StateLock::shared_if_present`, `reference::read_receipts_with`'s
  enumeration, `area::read_directory`, `reference::pools_holding`,
  `state::is_initialized`, the hold-file stats): the production operations
  observe no parent, and none may be invented.
- Refusal rows lose or omit the binding: `MacOs::child_facts` and
  `reference::read_oxide_marker` refusals record a bare component name with
  no parent; `MacOs::open_child_dir` and `MacOs::duplicate_dir` refusals
  record no row at all.
- Handle self-facts rows (`MacOs::dir_facts`, `MacOs::recheck_dir` via
  `tape::handle_observation`) carry device and inode but no path, no
  provenance and no link to the open-time observation of the same handle, so
  a re-read cannot be reconciled as the second occurrence of one handle's
  lifetime.

B-S1's own predecessor record (`docs/knowledge/contracts/o2-tape.md` on that
branch) states the limit correctly: these sites cannot establish the required
binding by a later stat or a lexical path, and a reviewed change would need
to carry an already-opened parent descriptor through the operations without a
second lookup. This design is that reviewed answer, or its precise no-go per
site.

## Sources and version identity

| Source | Identity | Used for |
| --- | --- | --- |
| `inflatable-cookie/barge` O1 pin | `4af9360bd09629e970c143080ca159de8eef2562` | every production source claim; this worktree at `39064ba` has no `crates/` diff from it |
| B-S1 PR41 checkpoint 1 | `226da715f6b9dc8b7c7f93eb584aa6a6b083495a` | the helper removal, `observation_proven`/`observation_complete`, the recorder no-reopen test |
| B-S1 PR41 checkpoint 2 | `227a76da71366cb1f132dba279397eba40fef4ff` | the clock sites, platform uid, the surviving `dir_facts`/`lock_file_facts` helpers, the current row shapes |
| B-S1 first tape commit | `f54fd37` (ancestor of `19cafe2`) | the `parent_identity` defect itself |
| `inflatable-cookie/oxide` | sibling pin `9339a01` | `docs/knowledge/contracts/barge-adoption.md`: the accepted shared capture contract, schema names, caps, reconciliation oracle, X-S1/X-S2 slice boundaries |

Oxide's PR230 (#240) manifest schema proposal is cited as pending review, not
as content. Anything uncertain about these sources is a named open issue,
never a qualification claim.

## What one attributable observation binds

A transcript read row for a file or directory observation is attributable
when one operation — one call at one named source site — carries all of
these, each labelled with where it came from:

- **Site and kind.** The source symbol that performed it and the observation
  kind (file read, stat, enumeration entry, lock, handle self-fact).
- **Parent identity.** The observed parent directory's `(st_dev, st_ino)`,
  with a provenance class (below). A parent field without provenance is not
  a fact and must not serialize.
- **Child component.** The exact name resolved within that parent. A
  component path is bound to its observed parent and exact entry, never to a
  string alone.
- **Descriptor identity.** The `(st_dev, st_ino)` of the object the operation
  actually opened or stated, when the operation read it.
- **Result.** The bytes digest with the cap that bound it, an explicit
  absent, a listed count with truncation, a classified answer, or a refusal
  errno class — never a defaulted verdict.
- **Epoch and occurrence.** Engine-local, monotonic, assigned at the leaf;
  occurrences counted per identity key, so a root-lock marker reread or a
  custody re-read is a second bound occurrence of the same key, never an
  independently chosen result.

### Provenance classes

A row's parent and identity fields each carry exactly one provenance class:

- **`handle`** — the operation resolved the child *through a directory
  descriptor whose identity the engine observed when it opened that
  descriptor* (`DirHandle::identity()`, captured at open by
  `read_dir_identity`). This is the only class that binds the parent object of the
  actual read directly, subject to the numeric identity premise below; the kernel resolves
  `openat(fd, name)` inside the directory object the fd names. Can discharge a
  parent obligation only after the numeric identity premise is qualified.
- **`extracted`** — read from a fact the same operation, or its production
  prefix in the same function body, already fetched: `MetadataExt::dev()`/
  `ino()` on a `Metadata` the site already holds. Zero additional syscalls.
  The parent variant (an enumeration's parent taken from that site's own
  preceding production stat) is the observation of an *earlier* resolution,
  not of the operation's own; it provides only contextual **derived** evidence (below).
- **`context`** — carried from an earlier observation of the same object in
  the same window (for example, the resolved area handle beside a
  `open_path` call whose absolute path names a child of that area).
  Provides contextual **derived** evidence only.
- **`derived`** (reconciliation-side label, not a row field) — a parent
  binding whose row provenance is `extracted` or `context`, a contextual inference under declared STATIC custody and manifest agreement.
  It is not the observed parent required by the shared contract and leaves
  that obligation **unproven**.
- **`none`** — the field is absent. A parent obligation flagged mandatory
  cannot be discharged by a `none` row.

An own-descriptor identity field is `handle`-class when the operation read it
through the descriptor it used (fstat of the opened file or handle) and
`extracted` when read from an already-fetched `Metadata`. Both are the
operation's own observations of the object it acted on; neither is a new
syscall.

### Refusals and absences

A refusal binds whatever the failed operation actually observed: an
`openat`/`open` errno class with the parent binding the call carried (for a
handle-child call, the handle identity is `handle`-class even when the child
answer is `ENOENT`); a path-open refusal binds no parent (none was resolved)
and must not claim the context handle as if it were. A positive absence
(`Ok(None)`, `ReadOutcome::Absent`) binds the parent when the look was
handle-relative or has an `extracted` prefix stat, because "the name is not
there in *this* directory" is exactly what those operations observed.

## Site inventory: the actual `release::plan` chain

Every site below is at the O1 pin. "Observed" is what the production
operation already reads; "free" is what is extractable from observed facts
with zero added syscalls; "selection" is this design's decision, with the
provenance the row may carry. The chain is `plan_observed` (release.rs
`plan_observed`) → setup → the per-root `decide_root` loop, plus the effect
path's `reprove_row` re-reads where they reuse the same sites (they are second
occurrences of the same keys, and this inventory covers them identically).

### Setup reads

| # | Site (file, symbol) | Kind | Observed by the operation | Free extractions | Parent availability | Selection |
| --- | --- | --- | --- | --- | --- | --- |
| S1 | `release.rs` `read_registry` → `state::LockSet::observe_registry` → `StateLock::shared_if_present` | lock open + flock | open result (`ok`/`NotFound`/errno), flock outcome | opened fd fstat only as a **declared new observation** (L1 below) | none | lock rows carry own identity under L1; parent `none`; absent open binds absence only |
| S2 | `registry.rs` `read` (via `state::read_bounded`) | bounded file read (K2) | precheck `symlink_metadata`; opened fd fstat (`file.metadata()`); bytes | own dev/ino on both facts | none | K2 binding (below): stat row for the precheck identity, read row for the opened identity + digest |
| S3 | `area.rs` `show` → `read_declarations` → `area::read_directory` ×2 | dir stat + path enumeration (K4) | `symlink_metadata(path)` precheck; `read_dir` names | own dev/ino of the precheck; parent = the precheck's identity as `extracted` | `extracted` from the site's own precheck | keep per-entry name rows parent-bound to the `extracted` precheck identity; **remove the tape-side `dir_facts` helper** |
| S4 | `area.rs` declaration file `state::read_bounded` | K2 | as S2 | as S2 | none | K2 |
| S5 | `area::VolumeRoot::open_with` (`MountLookup::ByUuid`) → `MacOs` mount census, volume identity via fd, mount-root proof, dir open | volume/handle | census rows, UUID, mount, opened handle identity (`read_dir_identity`) | — | none for the opened volume root | unchanged from the checkpoint's volume rows; the opened root handle's identity is the `handle` provenance for everything below it |
| S6 | `state::LockSet::observe_volume` → `shared_if_present` | lock | as S1 | as S1 | none | L1 |
| S7 | `area::resolve_by` | handle walk | `open_path` volume root, `open_child_dir`/`child_facts` components | — | `handle` at every child step | complete refusal-row coverage so each component step binds parent + component + errno |
| S8 | `area::list_names` → `dirs.duplicate_dir` + `dirs.list_names` | handle enumeration | duplicate fd; `getdirentries` names through the handle | — | `handle` (the area handle) | one `handle_observation`-class row bound to the area handle's identity; add the duplicate's link to its source handle's open epoch as `context` |
| S9 | `reference::BargeReferences::read` → `cache::read_placements` | state-dir census + K2 reads | dir listing + bounded reads | as S2/S3 | per site | placements census rows as S3; each placement document row as S2 |
| S10 | `reference::OxideSnapshot::read_within` → `control_directory_with` → `inspect::observe_oxide_at` (`read_pools`, `read_registration` per candidate) | K2 per candidate | as S2 | as S2 | none | K2; the candidate's `inspect(&outputs)` metadata row (already an `io::Result<Metadata>` at the site) carries own dev/ino `extracted` |
| S11 | `reference::read_receipts_with` | K3 + K4 + K2 per entry | `std::fs::metadata(outputs)` precheck; `read_dir(outputs)`; per-entry `state::read_bounded` | outputs' own dev/ino from the precheck; parent of entries = precheck identity `extracted` | `extracted` | entries parent-bound to the precheck identity; per-entry receipt rows as S2; **no parent may be claimed when the precheck refused** |
| S12 | `reference::pools_holding` | K3 stat | `symlink_metadata(removable)` | own dev/ino | none | stat row with own identity `extracted` |
| S13 | `pool_custody::read_document` (per pool) + pre/post stats | K2 + K3 | pre-stat, `state::read_bounded`, post-stat | own dev/ino on all three | none | K2; the pre/post stat pair is two occurrences of one key whose outcomes must agree under STATIC (they are the checkpoint's existing rows) |
| S14 | `pending_intents` → `state::read_intents_with_deadline` | K4 + K2 per intent | `read_dir(directory)` only — **no production precheck stat** | none | none honest | entries parent `none`; **remove the `dir_facts` helper**; if a future case flags this enumeration's parent mandatory, the declared one-row pre-observation (K4b) is the only route |
| S15 | `observer.effective_uid()` | process fact | uid answer | — | n/a | unchanged (process rows are the shared contract's leaf table) |

### Per-root reads (`decide_root`)

| # | Site | Kind | Observed | Free | Parent availability | Selection |
| --- | --- | --- | --- | --- | --- | --- |
| R1 | `locks.observe_unit` / `observe_root` → `shared_if_present` | lock | as S1 | as S1 | none | L1; each root's lock is its own key |
| R2 | `dirs.open_path(&root.path)` → `MacOs::open_path` | path open + fstat | opened handle identity (`read_dir_identity`) | — | none for the opened dir itself; **`context`** available: the caller holds the resolved area handle (`ResolvedArea::dirs`) whose child the listed name is | opened-identity row as today, plus an optional `context` parent field naming the area handle's identity and its open epoch; provenance `context`, mandatory parent unproven |
| R3 | `area::read_marker_with_digest` → `read_marker_bytes` → `dirs.child_facts` + `dirs.read_child_file` | handle-child reads (K1) | child facts (twice), opened fd fstat ×2, bytes, final child re-check | — | `handle` (the root handle) at every step | already bound at the checkpoint for present rows; **repair refusals** to carry the same handle parent; the internal before/after fstat pairs are distinct occurrences of the child key whose outcomes must agree under STATIC |
| R4 | second `read_marker_with_digest` after `locks.observe_root(id)` | K1 reread | as R3 | — | `handle` | second occurrence of the same key, plus the identity reread comparison the engine itself performs (locked_by vs now) recorded as its verdict row |
| R5 | `area::read_holds` → `read_hold_directory` | K4 (`area::read_directory`) + K3 per hold + K2 per hold | dir precheck stat + `read_dir`; per-hold `symlink_metadata`; per-hold `state::read_bounded`; per-hold process classification | as S3/S2 | dir entries `extracted` (precheck); hold files: none (the hold stat is of the file, not its parent) | as S3 for the listing; hold-file rows as S2/K3 |
| R6 | `read_record` → `identity::read` → `state::is_initialized` + `identity_directory` + `child_facts`/`read_child_file`/`recheck_dir` on `identity.json` | K3 + K1 + handle self-facts | `std::fs::metadata(state root)`; handle walk `open_path`→`private_directory`×2; child facts ×2; opened read; `recheck_dir` | metadata own dev/ino | `handle` from `identity.json`'s parent handle; the state root's own parent: none | `is_initialized` stat row with `extracted` identity; walk rows with `handle` parents; `recheck_dir` rows carry the handle identity and are reconciled as the second occurrence of the handle's key (CF-7) |
| R7 | `area::scan_activity` → `dirs.dir_facts` + `walk_activity` | handle self-fact + K1 enumeration | dir facts through the handle; `list_children`/`child_facts`/`dir_facts` per entry, bounded | — | `handle` at every step | unchanged from the checkpoint's rows plus refusal coverage; the bounded activity walk's caps are the site's own, recorded as today |
| R8 | `dirs.dir_facts(&handle)` (root facts) | handle self-fact | fstat through the descriptor | — | the handle itself | `handle_observation` row gains provenance and links to the handle's open row (`context`), so it reconciles as a later occurrence of the same object key |
| R9 | `identity_evidence` → `dirs.open_path(&identity_area.path)` | path open + fstat | opened folder identity (device comparison only) | — | none (`context` n/a — the folder is the area itself) | opened-identity row; the device comparison's two sides are both observed identities of held handles, recorded as the verdict row's inputs |
| R10 | `custody::take_custody_by` → `custody::decide`: `read_unit_holds` (again), `registry::read` (again), `grant_still_recorded`, `area::open_folder_by`, unit open, marker verify, custody scan | K4/K2/K1 rereads + process scan | as the earlier sites, again | as earlier | as earlier | every re-read is a distinct occurrence of its key; **outcome divergence between the plan read and the custody re-read is the engine's own keep decision, recorded honestly — the reconciler compares each occurrence against the manifest, never against the other engine's order** |

### Site classes referenced above

- **K1 (handle-child):** every `DirOps` child operation on a held
  `DirHandle`. Parent = `handle`. Descriptor-relative binding; numeric identity lifetime remains subject to the K1 premise below.
- **K2 (bounded path read):** `state::read_bounded_inner`'s precheck stat +
  open + fstat + read. Two-to-three occurrences of one key; own identities
  `extracted` (free); parent `none`; mandatory parent remains unproven even under custody.
- **K3 (path stat):** `symlink_metadata`/`metadata` prechecks. One
  occurrence; own identity `extracted`; parent `none`.
- **K4 (path enumeration):** `std::fs::read_dir`. Entry rows bind the
  parent from the site's own production precheck stat as `extracted` where
  one exists (S3, S11, R5); otherwise `none` (S14). **K4b** (only if a case
  ever flags it mandatory): one taped cfg(test) `parent-candidate`
  pre-observation before the `read_dir`, its own row, cost one
  `fstatat`, failure ⇒ no parent field; remain insufficient for mandatory parent coverage. It cannot satisfy the shared mandatory parent requirement, so nothing adds it today.

## Selected design

### The rule

**Bind what the operation itself observed; extract, never re-look; declare
every added observation; derive parent bindings only under declared custody;
and remove every hidden tape-side read.** No production statement changes;
every addition is a separate `#[cfg(test)]`-attributed statement or lives in
the `cfg(test)` tape module; the production build compiles none of it.

### Tape surface (exact signatures, `crates/barge/src/tape.rs`, cfg(test))

- `Identity` gains one field, `parent_source: ParentSource`, serialized only
  when not `none`, where `enum ParentSource { Handle, Extracted, Context }`.
  Existing constructors keep their shapes; `Identity::file` and
  `Identity::component` gain parent-source-carrying variants:
  - `Identity::file_bound(path, opened: (u64, u64), source: OwnSource)` —
    the opened/extracted own-identity row;
  - `Identity::component_source(parent: (u64, u64), name: &str, source:
    ParentSource)` — component rows whose parent provenance is explicit;
  - `Identity::context_parent(path, parent: (u64, u64), opened_epoch: u64)` —
    the R2/R8 context link.
- `read_component_refused(site, parent: (u64, u64), name: &str, class: &str)`
  — the refusal counterpart of `read_present_component`; replaces the
  bare-name `listing_refused` rows at `MacOs::child_facts` and
  `reference::read_oxide_marker` refusals.
- `component_refused_unbound(site, name: &str, class: &str)` exists only if a
  refusal genuinely observed no parent; the default is the bound form, and
  every child-site refusal path must be walked and covered.
- `handle_observation` gains `path: Option<&Path>` and `source` arguments so
  `MacOs::dir_facts`/`recheck_dir`/`duplicate_dir` rows name the object and
  their provenance, and a recheck reconciles as the second occurrence of the
  handle's key.
- `lock_rows`: the existing `tape::lock` rows stay; `LockOutcome::Held`'s
  identity is documented as sourced from the L1 declared fstat.
- **Removals:** both cfg(test) `dir_facts(path)` helpers
  (`state.rs`, `area.rs`) and their per-entry calls; the
  `reference::read_receipts_with` entry rows switch from `parent: None` to
  the `extracted` precheck identity; the `state::read_intents_with_deadline`
  entry rows switch to parent `none`.

#### Implemented test-only observations (Barge047)

The `/2` adoption completed the finite observation deviations that current
source supplies the fact for. Implemented names and limits:

- `crate::tape::extracted_dir_entry(site, parent, name)` records one
  enumeration row whose parent is the identity the site's own production
  precheck already observed (`extracted`). `area::read_directory` uses it, so an
  entry row is parent-bound without a second lookup and without a handle.
- `crate::tape::context_link(site, path, own, context)` records the R2 link:
  the opened root's own observed identity with the identity of the area handle
  the resolution already holds as its `context` parent. `release::decide_root`
  calls it immediately after the original `dirs.open_path(&root.path)`;
  `ResolvedArea::held_identity` is a `#[cfg(test)]` accessor over the handle the
  resolution already holds.
- `crate::tape::linked_handle_observation(site, kind, descriptor, own, context,
  outcome)` records the S8/R8 rows: the descriptor's own `handle` identity and
  the source handle's identity as their `context` parent. `MacOs::duplicate_dir`
  passes the handle's identity for both; `MacOs::dir_facts` and
  `MacOs::recheck_dir` pass the identity the descriptor actually returned, so a
  divergence from the handle's open identity is visible rather than smoothed.
- PCF-4 and PCF-5 are pure controls: `laundered_parent_rows` names a row that
  claims a `handle` parent at a direct-read site (`DIRECT_READ_SITES`), and
  `incoherent_handle_rereads` names a handle self-observation (`HANDLE_SELF_SITES`)
  whose own identity differs from its context identity. Each is tested with a
  real recorder row and a doctored row.

Limits. The reviewed transcript schema has no epoch-reference field, so a
context link is the observed identity plus `context` provenance, not an epoch
cross-reference; the design's "links to its source handle's open epoch" is not
expressible and is not claimed. None of these rows discharges a
`parent_required` obligation: K1 stays unqualified, so completeness reports it
as an issue. The rows add no syscall, no path lookup and no production
statement.

### Declared new observations (cost and error semantics)

| Observation | Where | Cost | Error semantics |
| --- | --- | --- | --- |
| **L1** lock-file fstat: `file.metadata()` on the already-opened lock file, per held lock | `StateLock::shared_if_present` / `StateLock::exclusive` (cfg(test) statement after the flock succeeds) | one `fstat` per held lock; no path resolution, cannot alias | fstat failure ⇒ `Held` with no identity: the lock outcome obligation stays proven, the identity obligation stays unproven; never a refusal |
| **K4b** parent-candidate pre-stat (conditional, unused today) | before a K4 `read_dir` whose parent a case flags mandatory | one `fstatat` per enumeration | failure ⇒ no parent field; the row is its own taped occurrence; never claimed as the operation's resolution parent |

Nothing else is added. In particular the design does **not** re-stat any
parent at record time, does not open any directory the engine did not, and
does not read any file the engine did not.

### Repair list (coverage completeness)

Every site's refusal and absent paths must produce the same bound fields as
its success path where the operation observed them. The confirmed gaps the
implementation closes: `MacOs::open_child_dir` refusal (no row today),
`MacOs::duplicate_dir` refusal (no row today), `MacOs::child_facts` refusal
(bare name today), `reference::read_oxide_marker` refusal (bare name today),
`state::read_bounded_inner` precheck-success stat row (the precheck identity
is dropped on the happy path today — one free `extracted` row restores it),
and `read_child_file`'s internal mid-read refusals (each must name the
parent handle and the component). A site whose refusal observed nothing
binds nothing; that is coverage honestly bounded, not a default.

### Transcript requirements (coordination, not wire format)

Required semantics for the reviewed manifest/transcript schema (Oxide PR230
coordination; B-S1's provisional consumption surface stays a consumer, not a
definition):

- every parent and own-identity field carries its provenance class;
- every row carries site, kind, epoch, occurrence and the cap that bound it;
- a case's `mandatory_observations` entries gain a `parent_required` flag:
  `handle`-provenance discharges it only with the qualified K1 premise; `extracted`/`context` are contextual evidence only and do not discharge
  the shared contract’s mandatory parent identity; `none` never discharges it;
- effects, clocks, process leaves and the caps block are unchanged from the
  shared contract.

### Reconciliation additions

The shared contract's oracle is unchanged; parent binding adds to step (3):

- a `handle` parent binds the directory object; its numeric identity satisfies the
  obligation only when the K1 identity premise is independently qualified;
- `extracted`/`context` parents are compared against the manifest entry's
  `parent_dev`/`parent_ino`: agreement under declared STATIC custody records
  context only, never satisfaction of a mandatory parent obligation; disagreement
  or missing binding is **insufficient** with the row named;
- an own-descriptor identity that disagrees with the manifest entry under
  the same bytes is CF-3's replaced-object detection and is insufficient;
- two occurrences of one key within one engine's window whose outcomes
  differ are insufficient unless the case declares the divergence dynamic
  (a declared remount, a declared writer lifecycle) — this is CF-7 applied
  to the handle self-facts and the marker reread.

## Test-only context carrying versus private handle/type instrumentation

Two routes can bind parents at the sites that today observe none.

**Route A — test-only context carrying already-observed facts (selected).**
The recorder receives what the production operation and its in-function
prefix already observed: `DirHandle::identity()` for handle-child rows,
`MetadataExt` extraction for stat/opened rows, the caller-held area handle
for `open_path` context, the site's own precheck stat for enumeration
parents. Zero or near-zero added syscalls (L1 only), zero production shape
change, refusal and error semantics untouched because the code under test is
untouched. The cost is honesty about provenance: parent bindings outside K1 are at best `extracted`/`context` evidence;
they do not satisfy the shared contract’s mandatory direct-state parent identity.
Those rows remain unproven even under STATIC custody.

**Route B — private handle/type instrumentation (rejected).** Carry an
already-opened parent descriptor through the direct reads: open the state
root once, resolve children by `openat`, or pass parent handles into
`read_bounded_inner` and the enumerators. This would make every parent
`handle`-proven, but it changes the code under test: path resolution,
intermediate-symlink handling and errno classes differ between
`File::open(path)` and a handle-relative open chain, so the reader would
either be a duplicated policy (forbidden — the audit's "no policy copies")
or a production change to the actual readers. It may be necessary to satisfy mandatory direct-state parent obligations,
but its behavioral equivalence has not been established. **Not authorized by this design.** If a
future proof ever needs *proven* (premise-free) parent identity for the
boot-volume state files themselves, the minimal source-backed option is a
descriptor-relative state-reader seam in the production readers — a
production semantics change (intermediate-component resolution and refusal
behaviour), which is an operator scope question for Tom with the file, symbol
and behaviour named, never a silent inference. The shared contract expressly requires parent identity at direct state reads
(`barge-adoption.md`, Direct state reads and identity definition). Therefore
this design cannot establish complete coverage there; a future scoped brief
must evaluate that seam and seek Tom’s ruling if semantics or public API change.

## Limits of descriptor-relative binding

**The theorem (K1).** For an operation `openat(parent_fd, name)` with
`O_NOFOLLOW` at the leaf, where `parent_fd`'s identity `(dev_p, ino_p)` was
read by `fstat` when the descriptor was opened: the child object resolved is
a child of the directory object `parent_fd` names, at call time. The descriptor identifies the directory object at the operation, including
after a rename. However, the cited sources do not establish APFS inode
non-reuse or `(dev, ino)` stability over a live descriptor lifetime. Treat
those properties as unverified premises, not a theorem. Subject to their
independent qualification, `(dev_p, ino_p, name, fstat(child), bytes)` is one
attributable operation whose parent is the actual parent of the read, for the
descriptor's whole lifetime — **regardless of renames, sibling replacement,
or path replacement elsewhere in the tree**. The recorded path string is
valid only at open time and is a label, never an identity: the reconciler
binds identities and treats paths as capture-time labels.

**Limits that remain, each with its counterfactual:**

1. **Path-opened leaves (K2/K3/K4).** The kernel resolves intermediate
   components unobserved. No userspace fact proves which parent the
   resolution traversed; the precheck stat, the open and the read are
   separate resolutions. The bytes are bound to the opened descriptor's
   identity (occurrence 2), which is the object actually read — that much is
   descriptor fact. No mandatory parent is established, including under custody.
2. **The fstat pair is not a stable namespace interval.** The precheck→open
   pair does not establish that the namespace was stable between them; it
   establishes that the bytes came from the object occurrence 2 names.
   Production already mitigates type and size divergence between the pair;
   the tape's free dev/ino extraction adds detection of a replacement that
   changed identity but not size or type. It cannot detect an
   unlink-and-recreate whose inode number is reused inside the gap with
   identical size and bytes: that is invisible at this site, excluded only by
   custody, and the design claims exactly that much and no more.
3. **Symlinked intermediates.** Path opens follow intermediates (leaf-only
   `O_NOFOLLOW`); a link planted in the fixture pointing outside would make
   a derived parent wrong while pre/post tree checks could still match if the
   link were removed again. Under declared custody with all actors declared,
   this requires an undeclared actor — which is precisely the failure the
   custody premise must catch; the manifest records symlink targets, and the
   counterfactual set must include a declared-actor variant so the
   derivation's reliance on custody is exercised, not assumed.
4. **Missing ancestors.** An `ENOENT` ancestor yields an absence or refusal
   with no parent fact: nothing resolved. The obligation binds to the
   manifest's declared-absent entry; parent fields stay `none`, and only the
   absence itself is dischargeable. A handle-child `ENOENT` is different: it
   proves the name absent in that specific parent (`handle`-class).
5. **Device and inode reuse.** Within one STATIC window under custody there
   is no deletion actor, so no reuse. Across remounts the device changes and
   is a declared DYNAMIC fact; a same-UUID remount must reconcile as the
   declared remount, never as a fresh identity. Reuse with identical observed outcomes may be invisible; it cannot be
   assumed to surface as a conflict. The affected identity remains insufficient.
6. **Renamed parents.** Identity survives; the path label does not. A row
   whose path string no longer resolves is not wrong; a row whose identity
   no longer matches any manifest entry is.
7. **Live handle lifetime.** Between open and use, entries may change:
   visible as occurrence outcome changes, insufficient unless declared
   DYNAMIC. A force-unmounted volume makes descriptor reads fail with real
   errnos; the caller classifies them today and the tape records the refusal
   classes; the reconciler compares them against the declared volume epoch.
8. **Bounded retry.** Custody and dead-handle retries re-observe: each
   attempt is a new epoch occurrence bound to the same key. Under STATIC they
   must agree; the first agreeing pair rule (Q-010's discipline) is the
   engine's own and is recorded as answers, never merged into one invented
   result.
9. **Locks.** Lock outcomes are dynamic facts (contention, would-block,
   creation). Lock rows are per-engine records, never cross-engine parity
   rows; the shared contract already scopes them this way.

## Mandatory counterfactuals for parent binding

Each must end insufficient or as an honest declared effect — never parity.
They extend the shared contract's seven:

- **PCF-1 (escaped parent look).** Pure: the tape source guard rejects any
  cfg(test) filesystem read outside the declared observation statements —
  mechanically, the removal of both `dir_facts` helpers is asserted by their
  absence, and the guard rejects `symlink_metadata`/`read_dir`/`File::open`
  in cfg(test) helper position outside `tape.rs`'s declared rows. Native
  (X-S2, later): CF-1 unchanged.
- **PCF-2 (same bytes, changed parent).** Replace a fixture file with
  identical bytes under a different parent (new directory, new inode): K2/K4
  K2 has no parent and K4 may have an earlier `extracted` parent;
  both remain insufficient for mandatory parent coverage, named; K1 rows are unaffected (their parent is the handle),
  which is the demonstration that handle binding survives parent
  replacement — the case must run both site classes.
- **PCF-3 (renamed parent, handle held).** Rename a fixture directory while
  a case's handle rows are being produced (declared DYNAMIC actor): K1 rows
  keep binding the same `(dev, ino)` parent; the derivation for path rows
  breaks as declared. Parity claimed on the K1 rows only.
- **PCF-4 (provenance laundering).** Pure: a transcript row claiming
  `handle` provenance for a site whose inventory class is K2/K3/K4 is
  rejected by the obligation table — the reconciler checks provenance class
  against the site inventory of this contract.
- **PCF-5 (incoherent reread).** A handle's `recheck_dir` occurrence whose
  identity differs from the handle's open row within one STATIC window ⇒
  insufficient (CF-7 concretized for handle self-facts).
- **PCF-6 (undeclared parent-candidate).** A K4b row appearing where the
  case declared no pre-observation is an extra unmodelled observation ⇒
  insufficient; likewise a mandatory-parent flag discharged by a `none` row.
- **PCF-7 (lock identity without its declared observation).** A lock row
  carrying an identity where the L1 fstat cannot have run (its site absent
  from the source inventory) ⇒ apparatus error, insufficient.

## Caps, epochs and cost

The shared contract's caps are unchanged: 8192 read rows, 8 MiB document,
64 occurrences per identity key, 512 process rows. The additions' cost is
bounded and declared: provenance fields add a short enum per row (well inside
the document cap); L1 adds at most one fstat per held lock (locks per plan
are single-digit); K4b is unused. The predecessor record's measured risk
stands unchanged and is inherited by X-S2: a real host's custody scan
approaches or passes the row and process caps, and a window that truncates
is insufficient, never quietly complete. Epochs stay engine-local and
leaf-assigned; occurrences stay per-key; nothing in this design extends any
real deadline or merges any occurrence.

## cfg reachability, compile-away and provenance

- `crates/barge/src/tape.rs` and `crates/barge/src/tape_harness.rs` stay
  `pub(crate)` modules under `#[cfg(test)]` in `lib.rs`; nothing becomes
  `pub`; the testkit stays crate-private. Every statement added to a
  production function body is separately `#[cfg(test)]`-attributed, so the
  production compile is statement-identical.
- Compile-away is proven by a build with `cfg(test)` inactive over the
  workspace (the implementation slice names its covering Effigy selector and
  runs it once), together with the per-file diff discipline the shared
  contract fixes for B-S1: the non-cfg(test) diff against the pin is empty,
  shown by a per-file digest map in the PR.
- Source binding: the manifest names the Barge pin, the slice commit, the
  per-file `crates/barge/src` digests, the staged lockfile digest, both
  toolchain versions and the exported harness executable digest. Binary
  provenance: production binaries are untouched by this design; the only new
  binary is the `cfg(test)` harness executable, whose digest the
  orchestrator verifies before any window.
- The design adds no source file to `crates/`; the tape additions live in
  the existing cfg(test) modules, and the two helper removals shrink
  `state.rs` and `area.rs`.

## What this design does not authorize

No production behaviour, API or public-surface change; no feature; no pin
change; no Barge proof feature in any production dependency; no Oxide
repository change; no selector change; no fixture, pool, registry, account,
volume or host state read or mutation; no native proof; no parity claim; no
qualification claim; no clock or evidence reset; no manifest JSON layout
(the reviewed schema owns it). The B-S1 checkpoints remain unaccepted
evidence; this design's acceptance makes nothing about them accepted — a
future implementation brief adopts, amends or redoes them slice by slice,
with this contract as the binding standard for parent identity.

## Implementation slices

Finite, repository-separated, in dependency order. Each lands with its own
tests, doc updates and its own scoped brief; none authorizes a real run.

- **B-S1b (Barge repository): parent-provenance tape repair.** Scope: the
  tape surface additions and removals above — the two `dir_facts` helper
  removals; the `reference::read_receipts_with` and
  `state::read_intents_with_deadline` entry-row changes; the
  `state::read_bounded_inner` precheck row restoration; the refusal-row
  repairs (`MacOs::open_child_dir`, `MacOs::duplicate_dir`,
  `MacOs::child_facts`, `reference::read_oxide_marker`,
  `read_child_file` mid-read refusals); the L1 declaration and its tape
  documentation; the `handle_observation` provenance and context links; the
  `ParentSource` provenance field. Tests: the tape module's pure self-tests
  extended with PCF-1's source guard, PCF-4's provenance-vs-inventory check,
  PCF-6/PCF-7 self-tests, the provenance serialization, refusal-row binding,
  and the caps; the harness's embedded exercise extended to assert parent
  provenance on the marker, enumeration and lock rows it already produces.
  Validation: `effigy rust:compile`, `effigy rust:format`,
  `effigy rust:lint`, `effigy rust:test`, and `effigy docs:check`.
  The native capture proof belongs to the Oxide X-S2 slice below, after this
  instrumentation is available. No native run is authorized here.
  Source binding: the O1 pin; the diff discipline of the shared contract.
  Stopping criteria: all green, production statements byte-unchanged, no
  hidden tape-side read remains.
- **X-S2 obligation extension (Oxide repository, Oxide's own slice):** the
  reconciler consumes the provenance classes and the `parent_required`
  obligation flags; PCF-2's fixture case and the K1-vs-K4 demonstration live
  there. Its separately activated native capture proof exercises actual
  descriptor and namespace behavior under the shared contract’s qualified
  supervisor and default Builds routing: 300 s, 512 MiB, at most one owned
  128 MiB image and one ordinary detach, with separate real image custody.
  Time and disk fit remain unmeasured; the implementation brief must specify
  staging, fixture and activation and assess fit before a counted run. These
  bounds grant no execution or cleanup authority. Coordination point only: the required members and semantics are
  this contract's sections above; the wire shape is PR230's reviewed schema.
  Nothing in Barge changes for it beyond B-S1b.
- **Schema coordination (both, no code):** the provenance enum and the
  `parent_required` flag are proposed to the reviewed manifest/transcript
  schema through Oxide's PR230 review; until reviewed, B-S1b's transcript
  carries the semantics in its provisional surface and stays a consumer.

## Validation of this design

Design-only: `effigy docs:check` once for this changed knowledge input (its
actual exit code is recorded in the delivery). No Rust, native, image,
fixture, process or live-state action of any kind belongs to this task; the
proposed validation above belongs to the implementation slices.

## Open design issues

Mandatory parent coverage for direct state reads remains unresolved. The only
candidate that could supply it, a per-call parent open (C1), is disposed of
below. Source does not support activation.

R-K2 is ruled. Tom's authentic board answer to decision
`0bcedf55-c0a2-42fe-86eb-1f3c384996fa` version 2 is: **"Defer the production
reader change (recommended)"** (Nov8). Task 046/PR46 merged
`bcb1f58ef1d164c31ba42ec8dcce917460b2bd06`; its per-site disposition found no
mechanical parent-handle reuse at the 19 read call sites in 18 functions and
the 2 lock sites. Therefore:

- No production C1, C1-prime or C2 conversion is activated.
- Existing read, lock, error and object-selection behavior stays exactly as it
  is; this contract changes no production statement.
- Direct-read parent, K1 lifetime, process and enumeration gaps remain
  unproven, and full capture comparison remains held.
- The deferral is not completion of the mandatory parent prerequisite, and R-K2
  is not re-asked. A future activation would need its own reviewed scope.

### Route B compatibility disposition: the parent-opening question

Status: source-backed design disposition. Nothing here is executed. No test,
fixture, native run, build or process probe backs it. Source identity: Barge
`main` at `b613506` (after the 045 merge `e39fce7` and the 044 merge
`b613506`). Line numbers below are at that head.

This section replaces "Candidate private read seam for review". That section
framed the candidate for activation ("before activating the candidate",
"before planner activation") and left the parent-opening question open.
Source does not support activation. This section answers the question, and
the candidate is not activation-ready. Tom's Nov8 answer to decision
`0bcedf55` v2 (recorded under "Open design issues") defers the production
reader change outright, so nothing in this disposition is activated.

#### 1. Does a caller already own a usable parent descriptor?

No. Source facts:

- The 19 production call sites of `state::read_bounded` sit in 18 functions.
  `registry::read_with_deadline` (`registry.rs:559`, `:565`) has two cfg
  variants and counts twice. Every site reaches the reader through
  `read_bounded` (`state.rs:470`), the sole entry to `read_bounded_inner`
  (`state.rs:477`). No site receives a `DirHandle` or `File` for the leaf's
  parent.
- `StatePaths` holds a `PathBuf` root (`state.rs:103–106`). `LockSet` holds
  `StatePaths` only (`state.rs:1246–1250`). `StateLock` holds the lock file's
  own descriptor, not its parent's (`state.rs:975–981`).
- The one state-root descriptor in production is `identity::identity_directory`
  (`identity.rs:168`, `dirs.open_path(state.root())`). It is local to that
  function and feeds only the K1 child reads of `identity.json`. No
  `read_bounded` caller is in its scope.

So each read must open its parent newly. Reuse would need either a handle
threaded from a caller that holds none (Route B, rejected above) or a
`StatePaths`-owned root handle (C2 below).

#### 2. Direct lookup versus directory open

Current `read_bounded_inner` (`state.rs:477`): `symlink_metadata(path)`, then
refuse a link, a non-file or an over-cap file, then `File::open(path)`, then
`file.metadata()`, then the read loop. That is two path resolutions (lstat and
open) and one descriptor, held from open to drop. A precheck `NotFound` maps to
`Absent` at `state.rs:484–493`, whatever the cause.

Candidate C1 (section 3): `open(parent, O_RDONLY|O_DIRECTORY|O_CLOEXEC)`, with
no `O_NOFOLLOW` on the parent so that links along the path are followed as
today. Then `fstatat(name, AT_SYMLINK_NOFOLLOW)` with the same checks, then
`openat(name, O_RDONLY|O_CLOEXEC)` (leaf-follow kept, as `File::open` does),
then `file.metadata()`, then the same read loop. The parent descriptor is
dropped on every return. Rust's `OpenOptions` already sets `O_CLOEXEC` on Unix
(local std source, `library/std/src/sys/fs/unix.rs`).

Counterfactuals. Each is a source-backed prediction, not an executed result.

**Links and missing components**

| Condition | Current | C1 | Result |
| --- | --- | --- | --- |
| Link in an intermediate component (for example, state root is a symlink) | followed by lstat and open | followed by the parent open | same |
| Leaf is a symlink | refused at precheck | refused at `fstatat(NOFOLLOW)` | same |
| Leaf missing | `NotFound`, mapped to `Absent` | `ENOENT` at `fstatat`, mapped to `Absent` | same |
| Parent missing | `NotFound`, mapped to `Absent` | `ENOENT` at parent open | same only if the parent `ENOENT` maps to `Absent`. Today missing child and missing ancestor are both `Absent`, so the contract's "distinct" requirement is new, not preserved |
| Parent is a file | `ENOTDIR`, refused | `ENOTDIR` at parent open, refused | same class; error detail differs |
| Parent has no read bit (search-only, `wx`-class) | lstat and open succeed | `open(O_RDONLY\|O_DIRECTORY)` needs read: `EACCES`, refused | **new refusal**; the read succeeded before |
| Only one fd free | one fd needed; succeeds | two fds held together; parent open `EMFILE`, refused | **new refusal** under fd exhaustion |
| File replaced after open | bytes from the opened descriptor | same | same for bytes; the recorded parent is the object opened, not the path's current occupant |

**Root, relative, empty and trailing components**

| Condition | Current | C1 | Result |
| --- | --- | --- | --- |
| Single relative component (`plan.json`) | `Path::parent()` is `Some("")`; lstat and open resolve against the cwd; an existing file is read | open of `""` returns `ENOENT`. Mapped to `Absent` as today, the existing file reads as `Absent` | **false `Absent`** unless the empty parent is opened as `"."` |
| Root path (`/`), or a leaf with no normal name (`..`, `x/..`) | `lstat` finds a directory; refused as not a regular file | no file name to split off; C1 must refuse first, with the same class | same only if the explicit refusal is added |
| Trailing slash on a regular file (`dir/file.json/`) | `lstat` returns `ENOTDIR`; refused | parent `dir`, name `file.json`; opens and reads the file | **new read** where the current read refuses |
| Trailing slash on a symlink (`dir/link/`) | `lstat` follows the link; refused as not-regular or `ENOTDIR`, not as `symlink` | `fstatat(NOFOLLOW)` sees the link; refused as `symlink` | same refusal, different class and detail |
| Interior `.` (`dir/./file.json`) | kernel normalizes | Rust `Path` drops the interior `.`; same parent and name | none |
| Relative multi-component path (`dir/file.json`) | cwd-relative | parent open is cwd-relative too | none for the cwd; relative input is a caller fact (section 2, path form) |

**Acquisition and cleanup failures**

| Failure | Current | C1 | Result |
| --- | --- | --- | --- |
| Parent open fails `EACCES` or `EMFILE` | the lstat or open of the leaf may still succeed | refused at parent open; no fd leaked if the parent is never acquired | **new refusal** |
| `fstatat` fails after the parent is acquired | refused as `stat_refused` (lstat) | refused; the parent must close on this return | same class if mapped; cleanup is a test obligation |
| `openat` fails after a successful precheck (race) | `File::open` error, refused | refused; parent closes on return | same class |
| `file.metadata()` or read fails | refused | refused; parent and file close on return | same class |
| Cleanup on each return | `File` drops | the parent `File` must drop on every `?` path | source shows the design, not the behaviour; test required (section 7) |

**Path form per caller (absoluteness).** The reviewer asked for this evidence.
Every ordinary-build state and Oxide root is built from the password database
home, which source does not validate as absolute:

- Barge state root (ordinary build): `password_database_home()` joined with
  `STATE_ROOT_RELATIVE` (`os/macos.rs:809`). `password_database_home` returns
  `pw_dir` unchecked (`os/macos.rs:5017–5056`). A relative override
  `BARGE_STATE_DIR` is refused (`state.rs:117–124`).
- Proof-feature account home: a relative path is refused (`os/macos.rs:4967–4972`).
- Oxide state and cache directories: `tool_account_home` returns
  `password_database_home()` in ordinary builds (`os/macos.rs:831–833`).
- Oxide pool paths: checked absolute when the pools record is parsed
  (`inspect.rs:644`). Registration and registry paths derive from those.
- CLI `--plan-file`: `PathBuf::from` with no check (`main.rs:518`). A relative
  name is read against the cwd, and an absent file is a usage error
  (`main.rs:532`). This is the one inventory caller where user input reaches
  the reader without an absoluteness check.

The premise "`pw_dir` is absolute" is conventional, not checked in source. It
is named here as a premise, not as evidence.

**Pinning does not merge observations.** Each read opens its own parent. That
gives one occurrence per call, keyed to that call. A second read of the same
file is a second occurrence. No parent descriptor is reused across calls, so
one call's parent never speaks for another's.

**Rejected: observe-only parent open in cfg(test).** A parent open that the
read does not use is a second resolution. It is the defect `226da715` removed,
and it binds nothing the read used.

**Why C1 still does not discharge the parent obligation.** The handle-class
parent C1 gains holds for one call only. Discharging the shared contract's
mandatory parent obligation needs the K1 lifetime premise, and source does not
establish it (K1 theorem, above: APFS `(dev, ino)` non-reuse is unverified).
C1 buys per-call parent consistency. It costs the refusals in the tables. It
does not satisfy the obligation by itself.

#### 3. Options

- **C1, smallest candidate.** Change `read_bounded_inner` and
  `StateLock::shared_if_present`. Because `read_bounded_inner` is shared,
  every one of the 19 call sites changes, including the CLI plan-file read.
  No caller signature changes. The parent is a local `File`. In
  `shared_if_present` the parent is dropped after `openat` and before
  `wait_for_lock`, so no parent descriptor is held during the lock wait
  (`LOCK_WAIT`, 5 s). Also required: an explicit `"."` for an empty parent, and
  an explicit refusal for a root or a leaf with no normal name, both with the
  same class as today.
- **C1′, narrowed.** The same design behind a new state-scoped entry point that
  only the inventory callers use. The CLI plan-file read stays on the path
  reader. This is a distinct behaviour surface and is part of R-K2 option B′.
- **C2, larger.** A `StatePaths`-owned root handle, opened once per command and
  threaded to every reader. This changes ownership and lifetime across a
  command. A root replaced mid-command changes selection for every later read.
  Multi-level sites still need intermediate opens. Not recommended.
- **C0, no change.** Direct-read parents stay `none` (unproven), as today
  (Route A, `o2-tape.md` "Direct-reader parents").

**Recommendation: C0 now.** C1 or C1′ becomes the candidate only when both
hold: (a) the owner accepts the tables' refusals and selection differences as
real semantic changes; and (b) the K1 lifetime premise is qualified, so a
handle-class parent can discharge the mandatory obligation. Without (b), C1
adds refusals and discharges nothing.

#### 4. Per-caller table

Production call sites: 19, in 18 functions (`registry::read_with_deadline`
counted for both cfg variants). Plus the two production shared-lock callers
(L1, L2).

Chain column: "inv." means the site is named in the shared contract's Direct
state reads or Locks rows (Oxide `docs/knowledge/contracts/barge-adoption.md`,
lines 862–863) or in this contract's S/R inventory. "other" means it is not.

Verdicts:

- **A**: mechanical under an existing proved invariant. None.
- **B**: needs the product boundary ruling R-K2 (section 5).
- **B-E**: B, and the site feeds a custody, release or deletion decision. Any
  refusal change needs the Elevated evidence the shared contract requires,
  before the change.
- **N**: not affected by C1. None: C1 changes the shared reader, so every site
  is affected.

| # | Site (file:symbol) | Chain | Ordered ops today | Parent handle today | Budget and mapping | Caller invariant | Verdict |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | `barge-cli` `volume` (`main.rs:518`, `read_bounded` at `:526`), user-named plan file | other | `read_bounded(file)` | none | registration cap, 1 row; `Absent` → usage error; refused → error | CLI apply; user input with no absoluteness check; relative names resolve against the cwd (section 2, path form) | B. C1 changes it through the shared reader; C1′ leaves it unchanged |
| 2 | `cache::read_placements` (`cache.rs:699`) | inv. S9 | `area::read_directory(placements)` (path precheck, `area.rs:2958`), then per-id `read_bounded` | none (K4 listing + K2) | placement cap, rows; `Absent` → skip; refused → error | no lock traced | B |
| 3 | `cache::read_adopt_intent` (`cache.rs:928`) | other | `read_bounded(root/cache-adopt-<kind>.json)` | none | intent cap, 1 row; `Absent` → `None` | cache command gates | B |
| 4 | `cache::finish_record` readback (`cache.rs:2597`) | other | publish, then `read_bounded(placement)` | none | placement cap, 1 row | caller has just published the file | B |
| 5 | `cache/undo_capture::require_no_pending` (`undo_capture.rs:59`) | other | `read_bounded(root/cache-undo-<kind>.json)` | none | 2 rows; only `Absent` passes | pending-journal gate | B |
| 6 | `registry::read_with_deadline` (`registry.rs:559`, `:565`) | inv. S2 | `read_bounded(registry_path)` | none | registry cap, volume rows, caller deadline; `Absent` → `Registry::Absent` | no uniform invariant. Read under a registry lock at `describe.rs:681`, `release.rs:749`, `volume.rs:94` and `:306–308`, `space.rs:241`, `prune.rs:821`. Read before or outside any registry lock at `area.rs:3101` (pre-lock read; lock taken at `:3104`), `area.rs:3514`, `area.rs:3664`, `area.rs:4953`, `delete.rs:2958`. The `registry::read` doc says the caller locks first; the call sites do not all do | B-E |
| 7 | `area::read_declarations` (`area.rs:2895`) | inv. S4 | `read_directory(areas)` (path precheck `symlink_metadata`, `area.rs:2958`), `read_directory(ns)`, per-file `read_bounded` | none | declaration cap, `AREA_ROWS`; `Absent` → skipped | no lock traced. Pre-existing: a listed-then-vanished file is skipped, not refused | B |
| 8 | `area::release_with` hold read (`area.rs:3672`) | effect | `registry::read` (`:3664`), `lock_root` (exclusive, `:3667`), `read_bounded(holds/<root>/<holder>.json)` | none | hold cap; `Absent` → `released: false` | exclusive root lock held at the read | B-E |
| 9 | `area::activity_record_bytes` (`area.rs:3796`) | effect | `read_bounded(activity_path)` | none | activity cap, 1 row | root lock held in `release_with` (`area.rs:3715`); second caller `idle_baseline_with` (`area.rs:5167`) lock not traced | B-E |
| 10 | `area::read_hold_directory` (`area.rs:3936`) | inv. R5 | `symlink_metadata(hold)` (`area.rs:3889`), uid and mode checks, `read_bounded` | none; directory listing gives `extracted` only | hold cap; refused → `unknown_hold` (non-fatal) | `read_holds` observers | B-E |
| 11 | `area::read_activity` (`area.rs:4287`) | other | `symlink_metadata` (`area.rs:4253`), uid and mode checks, `read_bounded` | none | activity cap, 1 row; `Absent` → missing; refused → unverified | observer `effective_uid`; callers in `release_with` (`area.rs:3680`, `:3714`), `idle_baseline_with`, `list_roots_with` | B-E |
| 12 | `census::read_registry` (`census.rs:349`) from `observe_oxide_registry` (`census.rs:312`) | other | `read_bounded(pool/outputs/registry)` for each pool in the pools record | none | Oxide registry cap; refused → `Unknown` | Oxide-owned path; pool paths checked absolute at `inspect.rs:644` | B-E |
| 13 | `inspect::read_pools` (`inspect.rs:622`) | inv. S10 | `read_bounded(state_dir/pools)` | none | pools cap, pool rows; refused → observation error | Oxide state dir | B-E |
| 14 | `inspect::read_registration` (`inspect.rs:447`) | inv. S10 | `read_bounded(candidate registration)` | none | 1 row; refused → observation error | per candidate | B-E |
| 15 | `pool_custody::read_document` (`pool_custody.rs:402`) | inv. S13 | `identity(path)` pre-stat, `read_bounded`, post-stat | none; pre and post stats are path-based | document cap, `MAX_RECORDS`; refused → `Err`, keeps the pool | custody decision | B-E |
| 16 | `reference::read_registry` (`reference.rs:1180`) from `read_within` (`reference.rs:636`) | inv. S10 | `read_bounded(outputs/<registry>)` | none | Oxide registry cap; refused → `Unknown` | Oxide-owned path | B-E |
| 17 | `reference::read_receipt` (`reference.rs:1811`) | inv. S11 | per listed entry: precheck `metadata(outputs)` (follows links), `read_dir(outputs)`, `read_bounded(entry)` | none; precheck is `extracted` | receipt cap, 1 row; `Absent` → `Ok(None)`; refused → `Err` | S11: no parent may be claimed when the precheck refused | B-E |
| 18 | `state::read_intent_with_deadline` (`state.rs:2413`) | inv. S14 | `read_dir(intents)` (K4, no precheck), then per-stem `read_intent_with_deadline` (`state.rs:2587`) → `read_bounded(intents/<id>.json)` | none; no precheck | intent cap, 1 row, caller deadline; `Absent` → `UnknownState` | journal enumeration in `read_intents_with_deadline`, used by guard and effect paths | B-E |
| L1 | `LockSet::acquire_shared` → `StateLock::shared_if_present` (`state.rs:1038`, `:1362`) | inv. S1, S6, R1 (`observe_*`) | `open(path, RDONLY)` (follows leaf, no create), `wait_for_lock` (poll 20 ms, to `LOCK_WAIT`), L1 test fstat | none | `NotFound` → `None`; other → error | rank check in `LockSet`; the held lock owns its fd until drop | B. C1 must close the parent before the wait |
| L2 | `space.rs:246` volume lock | other | `shared_if_present` | none | `READ_DEADLINE` | lock dropped per loop iteration | B |

Tally: A 0; B 8 (rows 1, 2, 3, 4, 5, 7, L1, L2); B-E 12 (rows 6, 8–18); N 0.

#### 5. Product boundary ruling R-K2 (answered: defer)

Question put to Tom: whether the state and Oxide-artifact readers may change
refusal classes and object selection in the cases in section 2 (search-only
parent `EACCES`; parent-open `EMFILE`; rename between parent open and child
operations; relative single-component input reading as `Absent`; trailing-slash
reads), in exchange for a per-call parent that cannot discharge the mandatory
parent obligation until the K1 lifetime premise is qualified.

**Answer.** Tom's authentic board answer to decision `0bcedf55` version 2 is
"Defer the production reader change (recommended)" (Nov8). Choice A below is
the ruling: no production change, direct-read parents stay unproven, and the
question is not re-asked.

- **A. Defer (recommended and chosen).** No production change. Direct-read
  parents stay unproven, as `o2-tape.md` records. Revisit when K1 lifetime is
  qualified.
- **B. Accept C1 for all 19 sites.** Change the shared reader and lock seam
  under section 7. The new refusals, including on the CLI plan-file read,
  become declared behaviour.
- **B′. Accept C1′ for inventory sites only.** The same design behind a
  state-scoped entry point. The CLI plan-file read stays on the path reader.
  Still changes refusal and selection for the inventory sites.
- **C. Thread a `StatePaths` root handle (C2).** Larger ownership change. Still
  no parent for multi-level sites. Not recommended.

Recommendation: A. B and B′ add refusal and selection changes and still leave
the mandatory obligation undischarged. The cost is real. The benefit is not yet
attainable. Tom chose A.

#### 6. Contract conflict, and what this does not solve

**Conflict.** Oxide's shared contract (`barge-adoption.md`, "Direct state
reads", line 862) requires parent identity for its listed direct readers. No
source-backed candidate meets that today. A handle-class parent needs a new
per-call open (C1 or C1′), which changes refusal and selection. Its lifetime
premise (K1) is one that source does not establish. This conflict goes to the
planner with R-K2. Barge does not edit `barge-adoption.md` from this
repository.

**Not solved by this disposition:**

- K1 lifetime, and APFS `(dev, ino)` non-reuse over a descriptor's life.
- Operation-time process and fd identity: who holds the fd, and whether the
  same fd number names the same object at a later observation.
- Enumeration parents (K4): `read_dir` entries at rows 2, 7, 17 and 18 stay
  `none` or `extracted` under C1. If enumeration is ever taken, the withdrawn
  candidate's rules stand: one duplicated descriptor per stream, closed on every
  return, and no silent omission of an unreadable or non-UTF-8 entry.
- Multi-level parents beyond the immediate parent.
- Native behaviour of `EACCES`, `EMFILE`, rename windows and trailing-slash
  forms on APFS. Every section 2 row is a counterfactual.
- Parents of the Oxide-owned readers (rows 12–17) beyond the same K1 premise.
- The `pw_dir` absoluteness premise (section 2, path form).
- Custody and deletion decisions. This disposition changes none of them. Any
  change on a B-E site needs Elevated evidence first.

#### 7. Regression and verification requirements (only if R-K2 is ruled B or B′)

Nothing here is run by this disposition.

- **Fault injection (cfg(test), declared).** Inject failure at parent open,
  `fstatat`, child open, `fstat` and read. Each path must close every acquired
  descriptor on every return. Compare the fd count before and after.
- **Parity controls.** Intermediate symlink (followed); leaf symlink (refused);
  missing leaf (`Absent`); missing ancestor (`Absent`); parent that is a file
  (`ENOTDIR`, refused); single relative component with an existing file
  (present, not `Absent`); root path and `..` leaf (refused as not a regular
  file); file replaced after open (bytes from the opened descriptor); lock
  absent, held, and refused by deadline. Each must match the current result.
- **Declared-difference controls.** Search-only parent (`EACCES`); fd
  exhaustion at parent open (`EMFILE`); injected rename between parent open and
  child operations; trailing slash on a regular file (must be refused as today,
  and the C1 difference recorded if it is not); trailing slash on a symlink
  (refused as today, class change recorded). Each must show the outcome and be
  recorded as a difference, not as parity.
- **Lock.** The parent descriptor is closed before `wait_for_lock`. The fd
  count during the wait is unchanged.
- **Verification tier.** `effigy rust:compile`, `rust:compile:release`
  (cfg(test) compile-away), `rust:format`, `rust:lint`, `rust:test` (with the
  scratch variables from `AGENTS.md`), `rust:test:tape` and
  `rust:test:tape-eval`, `docs:check`. Native: a separately activated run under
  Oxide's qualified supervisor, diagnostic first, in the X-S2 pattern.
  Independent exact-head review. B-E sites also need Elevated evidence before
  any change.

#### 8. Recommended next scoped action

1. R-K2 (section 5) is answered: option A, defer. The contract conflict in
   section 6 is recorded, not re-asked.
2. Under A, section 4 is the complete inventory. The `o2-tape.md` rows stay
   unproven and no implementation brief follows. The test-only observation work
   that needed no production seam is implemented under "Implemented test-only
   observations (Barge047)".
3. A future B or B′ would need its own implementation brief carrying section 7,
   scoped to `read_bounded_inner` and `StateLock::shared_if_present` (B), or to
   a new state-scoped entry point (B′).
