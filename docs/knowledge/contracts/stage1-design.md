# Stage 1 design: library, CLI, volumes, areas, scratch and caches

This contract is the reviewed design for Barge's first stage. It fixes the
crate and CLI shape, where Barge keeps authoritative state, how a volume's
locations and consumer areas are declared and recorded, how project scratch and
package caches are placed, how writer custody and safe deletion work, and how a
scheduled host job drives expiry and pruning without a daemon. It also fixes the
on-disk formats a later implementation must read and write, the compatibility
boundary with Oxide's and Effigy's existing state, the implementation slices in
dependency order, and the tests those slices must carry.

This contract is the frozen design; the implementation lands slice by slice, and
[implementation status](#implementation-status) records what has landed in it.
No part of this contract authorizes a native run, a host or settings change, a
data move, a consumer edit, or an Oxide source change.
[Architecture](../architecture.md) owns the concepts and the boundary with
Oxide; this file owns the concrete stage 1 formats and interfaces. Where the two
differ, architecture wins and this file is corrected.

## Scope and authority

Frozen inputs, recorded before this design and unchanged by it:

- Tom's scope ruling and the authority list in [vision](../vision.md).
- Concepts, locations, volume modes, placement rules, platforms and invariants
  in [architecture](../architecture.md); the stage 1 scope in
  [questions](../questions.md) Q-001 to Q-005, all answered.
- Decisions `0a33cd0a-bad6-42ee-a495-236497d12338` (`root_dir` plus feature
  folders), `c80e2034-10bf-408f-be3f-177931e2016c` (no `kept/` group) and
  `e5e7bffa-727b-4327-8540-0d41470869a6` (consumer areas instead of
  Oxide-specific folders). The third refines the first two: only `scratch/` and
  `caches/` are Barge's own folders, and every other managed folder is an area a
  consumer declares. Barge holds no knowledge of Oxide or any other consumer.
- macOS on APFS only (Q-005). The Linux layer is designed for, not implemented.

In scope: one Rust library crate and a thin CLI; versioned JSON; account-wide
state and registration records; read-only import and inspection of Oxide's and
Effigy's existing on-disk state; per-volume `root_dir` and folder overrides; the
area declaration API and its record; project scratch allocation, conclusion and
expiry; npm, bun and Cargo registry/git cache placements with in-place adoption;
writer custody and safe whole-root deletion; the scheduled-job interface; the
failure and race matrix; finite implementation slices with dependencies; the
proposed test matrix.

Out of scope for stage 1 (each needs its own brief and gates): volume
selection and registration writes, reserve and floor pruning, Oxide adopting its
areas, killing the host reaper or changing any host job, any live placement
operation, and any change to Oxide or Effigy. [Architecture](../architecture.md#boundary-with-oxide)
owns the staged handover order.

## Sources and version identity

Read at these exact versions; a later version is a new input, not an
inherited qualification.

| Source | Identity | Used for |
| --- | --- | --- |
| `inflatable-cookie/barge` `main` | `f08c1550a848fa85b801befe839c9be0d07ccdce` (2026-10-26T08:07:03Z) | vision, architecture, questions, retired concepts, this design |
| `inflatable-cookie/oxide` `main` | `250c24e8ed7305e2f0d750ce29d149a2a6637809` (2026-10-26T07:40:57Z) | formats and rules Barge must read in place, as reference only |
| `inflatable-cookie/effigy` `main` | `b94b1061a7f0a245974a9f374681e58066ccdf89` (2026-10-26T08:13:12Z) | the scratch marker Barge must not break |
| Decisions | `0a33cd0a…`, `c80e2034…`, `e5e7bffa…` (answered 2026-10-26) | location rulings frozen above |

Oxide references below point at that commit:

- `docs/knowledge/contracts/workflow.md` — Removable build volume
  (`apfs-volume-v1`, registration, selection, flush), Final outputs on the
  permanent drive (`oxide#117`), Finder metadata in pools (`oxide#105`),
  Temporary scratch placement (`oxide#151`, `oxide#145`), Pruning owned output
  roots (`oxide#129`), and the shared proof supervisor.
- `src/bin/oxide/removable.rs` (`REGISTRATION`, `POOL_DIR`, `PAYLOAD_DIR`,
  `Registration`, `DriveMode`), `src/bin/oxide/config.rs` (`pools.json`,
  `known_pools`), `src/bin/oxide/outputs.rs` (`MARKER`,
  `oxide.outputs.root/2`), `src/volume.rs` (`Binding`, `dir_at`,
  `observe_root`, `refuse_unclosed_root`).
- `scripts/bounded-test.py` — the `.oxide-scratch.json` marker schema, its
  conclusion states and the seven-day self-expiry.
- `~/.local/share/scratch-reaper/reap.py` and `empty-dead-outputs.py` — the
  current host reaper's behaviours that Barge's scratch expiry must subsume
  (fourteen idle days, `lsof` custody, ENOTEMPTY retry, no symlink following,
  `scratch/oxide` left to Oxide).

Effigy references: `src/runner/scratch.rs` (`MARKER`, `RETENTION`, `EXPIRY`,
`Marker`) and `docs/knowledge/contracts/052-runtime-kernel-contract.md`.

Anything uncertain about these sources is a named [open design
issue](#open-design-issues), never a qualification claim.

## Crate and CLI skeleton

### Packages and module boundaries

One Cargo workspace, two packages, one published library:

```text
Cargo.toml                     # [workspace] members, resolver, shared profile
rust-toolchain.toml            # channel pin
crates/barge/                  # library: all mechanics, no argv
crates/barge-cli/              # thin binary named `barge`: parse, call, print JSON
```

`barge` (library) is what Oxide links. `barge-cli` contains only argument
parsing, the JSON envelope, exit codes and the human summary; every rule lives
in the library so the two cannot diverge. The CLI binary is named `barge`.

Library modules, each with one job:

| Module | Owns |
| --- | --- |
| `os` | Every platform fact behind traits: volume identity and mount facts, account identity, open-descriptor operations, process observation, Finder metadata, flush. macOS/APFS implements it first. |
| `state` | Account state directory, locking, atomic publish, bounded reads, journal records. |
| `registry` | Volume records, placement records, area declarations; the only authority Barge writes. |
| `inspect` | Read-only parsers for foreign state (Oxide `pools.json`, `removable.json`, root and scratch markers; Effigy scratch markers), returning `Observed<T>` values that never become authoritative. |
| `area` | Area declaration, path resolution, root creation, holds, root listing, prune planning. |
| `scratch` | Project scratch allocation, conclusion, expiry sweep. |
| `cache` | Cache kinds, the three placement forms (`setting`, `held-root`, `link`) and undo. |
| `custody` | Writer-custody evidence, verdicts, deletion authorization. |
| `delete` | Whole-root deletion, Finder tolerance, quarantine and replay. |
| `prune` | Area prune policy evaluation, plan/preview, intent journal. |
| `describe` | The versioned documents the library and CLI return. |

The OS layer is a set of narrow traits (`VolumeFacts`, `MountRoot`, `DirOps`,
`ProcessObserver`, `MetadataPolicy`, `Flush`) with one macOS implementation and
a compile-time-absent Linux implementation. No module above `os` may name a
macOS syscall, path convention or errno; that rule is what makes the Linux
follow-on additive.

### Declared toolchain and RustQuality resolution

Proposed for the implementation brief, not created now:

- `rust-version = "1.97.1"` on both packages, `edition = "2021"`, and
  `rust-toolchain.toml` with `channel = "1.97.1"`, `components = ["rustfmt",
  "clippy"]`, `profile = "minimal"`. Justification: the first consumer, Oxide,
  declares `rust-version = "1.97.1"` and pins the same channel
  (`Cargo.toml`, `rust-toolchain.toml` at the commit above). A library whose
  MSRV is newer than its consumer forks the toolchain; one older MSRV buys
  nothing this repository needs. The version is *proposed*: if Oxide's pin moves
  before Barge's implementation starts, the implementation brief re-derives it
  rather than inheriting this line.
- Northstar RustQuality at `profile = "strict"`, activated in the same
  implementation change as the code, following the pack's own setup route:
  `docs/knowledge/contracts/language-quality-activation.json`,
  `rust-quality-profile.json` (`language = "rust"`, `profile = "strict"`,
  `msrv.source = "repository_policy"`, the two manifests, both toolchain policy
  paths, `when_unresolved = "stop"`, empty generated/vendored exclusions) and
  `rust-quality-deviations.json` starting as `{"deviations": []}`.
- Deviation resolution is a decision for the implementation brief, taken against
  the code that exists: the profile starts with no deviations, every finding is
  either repaired or recorded as a concrete accepted deviation with its rule id
  and reason in the same change. This design pre-commits to no deviation, and in
  particular to no `unsafe` allowance beyond the one FFI seam the OS layer
  needs; that seam is the only place `unsafe` may appear, and the strict
  unsafe/FFI findings there are recorded, not silenced.
- Validation selectors are declared in the same change: `effigy` tasks for
  `docs:check`, the library unit suite, and the native design proof named below.
  Building is through `oxide`, never `cargo` (AGENTS.md).

### Versioned JSON and error contract

Every CLI command has `--json`, and stdout is exactly one JSON document. The
document names its own format:

```json
{"schema":"barge.describe/1","command":"describe","ok":true,"data":{…}}
```

Rules, applied identically to the library's public documents:

- The schema string is `barge.<command>/<major>`. A new optional field may be
  added inside a major version; removing a field, changing a type, changing a
  meaning, or making an optional field required is a new major, and the old
  major stays readable for at least one implementation slice.
- A reader refuses an unknown major, and refuses a document whose required
  field set is not exactly what that major defines. Unknown is never read as
  absent, empty or default.
- Failures print `{"schema":"barge.error/1","command":…,"ok":false,"error":{"kind":…,"message":…,"detail":{…}}}`.
  `kind` is a closed set: `usage`, `refused_boundary`, `refused_containment`,
  `volume_absent`, `volume_mismatch`, `unknown_state`, `foreign_owner`,
  `custody_not_proven`, `resource_bound`, `locked`, `io`.
- Exit codes: `0` the command completed and its report says what it did; `2`
  refused before any effect (including an absent volume, which names the drive);
  `3` unknown or unsettled state that needs repair or an operator decision.
  A refusal always changes nothing first. A signal or a timeout is never
  success.
- Bounds are part of the contract: every document has a size cap, every list a
  row cap, and a truncated list is `"truncated": true` with counts, never a
  silently short list.

`barge describe --json` (`barge.describe/1`) reports: the tool and format
versions; the account state directory and whether it is initialized; registered
volumes with availability, durability, `root_dir`, folder overrides and each
record's origin; declared areas; placements with their resolved paths, link
state and absent-volume policy; and every foreign observation with its source
string and version. It performs no mutation and takes only read locks.

### Read-only inspection versus authoritative writes

Two classes, kept apart in code and in the document:

- **Observed** facts come from someone else's bytes: Oxide's `pools.json`,
  `removable.json`, `oxide-outputs` markers and receipts, `oxide-pool` payload
  layout, `.oxide-scratch.json`; Effigy's `.effigy-scratch.json`. They are
  parsed strictly, carry `source` and `version`, and are reported. An
  unreadable, oversized, unknown-version or schema-divergent input becomes an
  `unknown_state` observation naming the file — never a default.
- **Authoritative** facts are Barge's own: its registry, area declarations,
  placement records, root markers, holds, intents and prune journals. Only
  these authorize an effect, and only inside Barge's own folders and declared
  areas.

Stage 1 writes authoritative state only for Barge-created roots and for
placements adopted by explicit command. It never rewrites a foreign marker, a
foreign receipt, or another tool's configuration outside an explicit adopt.

## State and registration

### Account-wide state directory

State is account-wide, not per `HOME` and not per checkout. Resolution:

1. The account home comes from the password database (`getpwuid_r` on the
   effective uid), never from `$HOME`. If `$HOME` differs from the account home,
   Barge reports it in `describe` and still uses the account home for state.
2. State root: `<account home>/Library/Application Support/barge/state` on
   macOS. A test or proof may set `BARGE_STATE_DIR`; that variable is documented
   as fixture-only and never consulted in a normal run, so a stray environment
   cannot move the account's authority.
3. Layout:

```text
state/
  registry.json                 barge.registry/1   (volumes, folder overrides)
  areas/<namespace>/<area>.json barge.area/1
  placements/<id>.json          barge.placement/1
  holds/<root-id>/<holder>.json barge.hold/1
  intents/<intent-id>.json      barge.intent/1
  locks/                        registry.lock, volume-<uuid>.lock, root-<id>.lock
  journal/                      bounded append-only prune/delete observations
```

Every write is atomic: a private `create_new` temporary file in the same
directory, sync, `rename`, then sync the directory. A failed write before the
rename leaves the previous bytes. Reads are bounded (size cap, entry cap,
deadline) and strictly validated; a registry that fails validation makes every
mutating command refuse, and is never rebuilt from markers.

Lock order is fixed and documented so two processes cannot deadlock: registry
lock → volume lock → root lock → intent write. The registry lock is never held
across a deletion or a volume walk; a long delete holds the volume and root
locks and re-reads the registry state it needs before publishing its intent.

### Registry of volumes

`barge.registry/1` holds, per volume:

| Field | Meaning |
| --- | --- |
| `volume_uuid` | APFS volume UUID from the kernel, the identity that matters |
| `mount_hint` | last observed mount point; used to find it, never as identity |
| `availability` | `permanent` or `ephemeral` |
| `durability` | `expendable`, `kept` or `undeclared` |
| `root_dir` | folder relative to the mount; `""` means the drive root |
| `folders.scratch`, `folders.caches` | per-volume override of Barge's own folders, relative to `root_dir` |
| `origin` | `declared` (an operator wrote it) or `observed_oxide` (imported, read-only) |
| `scratch_grants` | recorded deletion authority for non-native scratch: `delegations` and `claims`; empty means keep and report |
| `generation` | increases on every change; used to detect a stale reader |
| `registered_ms` | when this record was written |

Availability and durability are independent and both explicit; a record missing
either is invalid. `durability` has three values and is never defaulted:
`expendable` permits pruning, `kept` forbids it, and `undeclared` is the honest
state of an imported record that does not say. An imported Oxide registration
declares `mode` (availability) but nothing about durability, so the import
writes `undeclared`, and every placement or area use on that volume refuses with
`unknown_state` until a human declares it. `undeclared` is not a synonym for
`expendable` and is never treated as one; expending data is a destructive
permission that is never inferred.

`scratch_grants` is the one place a non-native scratch deletion is authorized:

```json
{"delegations": [{"family": "effigy_scratch", "by": "operator", "declared_ms": 0}],
 "claims": [{"scope": {"kind": "project", "name": "example"},
             "unit": "child", "unmarked_idle_seconds": 1209600,
             "families_excluded": ["oxide_scratch", "effigy_scratch"],
             "declared_by": "operator", "declared_ms": 0}]}
```

A delegation names one marker family (`oxide_scratch`, `effigy_scratch`) and is
written only by that family's owner. A claim names one scope (`project` with a
name, or `top_level_adhoc`) and one unit shape (`child` or `entry`). Both are
read under the volume lock immediately before an effect, never cached across a
run, and both can be withdrawn without touching data. Their exact predicates
are in [scratch compatibility](#compatibility-with-effigy-and-oxide-roots).
Empty or absent means no non-native unit is ever deleted.

Volume identity is verified on every use, not cached: the mount point must open
as a real directory directly under a `/Volumes` that passes the mount-parent
rule, the opened descriptor's device and kernel `ATTR_VOL_UUID` must equal the
registered UUID, the filesystem must be APFS, and it must not be the boot
volume. A different volume at the same mount point is refused
(`volume_mismatch`) and nothing is written or deleted. A stale mount-point
directory is never recreated and never written to.

Stage 1 does not register or change volumes; it reads the record and validates
identity. Registration writes are a separate slice with its own gates.

### Reading Oxide and Effigy state in place

Stage 1 imports nothing into authority. It reads, reports and refuses:

- **Oxide registration** `<internal pool>/oxide-managed/removable.json`
  (`Registration`, version 1 and 2, `deny_unknown_fields`). Version 1 without
  `mode` means `ephemeral`, exactly as Oxide reads it; an unknown version, an
  unknown field or an unparsable file is `unknown_state` naming the file.
  Barge reports mount point, UUIDs, reserve, cap, capacity, quota, mode and
  registration time, and derives *availability* from `mode`. It does not use
  Oxide's reserve or cap as its own.
- **Oxide pools record** `<state dir>/pools.json` (a bounded JSON array of pool
  paths, informational only — Oxide's own comment says it is never ownership or
  reclaim proof). Barge reports it as an observation and never treats a pool
  path as a volume identity.
- **Oxide output roots** `<mount>/<oxide-outputs>/…/.oxide-output.json`
  (`oxide.outputs.root/1` legacy, `oxide.outputs.root/2` current). Barge reads
  the marker to identify a root and reports it; it does not interpret
  `checkout`, `kind`, `generation`, `nonce` or a receipt beyond reporting them,
  and it never adopts, prunes, relinks or deletes an Oxide root.
- **Oxide scratch roots** `<mount>/scratch/oxide/oxide-proof.*` and
  `<mount>/scratch/oxide-validation/` with `.oxide-scratch.json`. See
  [scratch compatibility](#compatibility-with-effigy-and-oxide-roots).
- **Effigy scratch roots** `<scratch root>/<8-char run>[.effigy-scratch.json]`.
  See the same section.
- **Oxide receipts** (`oxide.outputs.transition/1..3`, registry,
  prune-intent and tombstones) are Oxide's alone. Stage 1 does not read them
  beyond `describe` naming their presence and version; prune stays Oxide's until
  stage 4.

Nothing found here is moved, renamed, rewritten, given a Barge marker or
retired. The old formats retire only once both sides agree (Q-004).

## Locations, folders and consumer areas

### Path containment and resolution

A folder is a *relative* path inside the volume: one or more components, none
empty, none `.` or `..`, no absolute prefix, no leading or trailing separator.
Resolution is descriptor-relative from the verified mount root: each component
is opened with `O_DIRECTORY | O_NOFOLLOW` and must be a real directory of this
user on the same device. A symlinked component, a component on another device,
a foreign-owned component, or an entry that changed between `lstat` and `open`
refuses (`refused_containment`). Paths are never normalised with string
arithmetic alone; the descriptor walk is the authority, and the resolved
absolute path recorded in a placement is an observation for humans, not the
thing that decides anything later.

Resolution order for a feature folder (for example scratch):

1. the volume's `folders.scratch` override, else `scratch/`;
2. resolved under the volume's `root_dir` (empty `root_dir` = mount root);
3. recorded in the placement or root record at creation time.

`root_dir` and folder overrides are read when a placement is adopted and
recorded in that placement. Changing `root_dir` later changes where a *new*
placement resolves; it never moves existing data. Relocating existing data is an
explicit, separate command (`barge placement relocate`, a later slice) that
plans, requires custody of both sides and reports; without it the record and the
data stay where they were, and `describe` shows both the recorded path and the
path the current configuration would resolve.

A placement whose recorded path is no longer inside the volume's resolved
`root_dir` (for example after a `root_dir` change) is reported as
`outside_configured_root` and keeps working: recorded beats configured, and the
only way out is the explicit relocate.

### Area declaration API

A consumer declares an area once and gets mechanics back. The library type is
the declaration; the on-disk document is the same shape.

```rust
pub struct AreaDeclaration {
    /// Consumer-chosen and opaque to Barge. Validated for containment only.
    pub namespace: String,
    pub area: String,
    /// Default path under root_dir; overridable per volume.
    pub folder: String,
    /// What a caller sees when the volume is absent.
    pub absent: AbsentPolicy,
    /// How this area's roots may be pruned.
    pub prune: PrunePolicy,
    /// Whether this area's bytes count toward the volume's reserve.
    pub reserve: ReserveParticipation,
}

pub enum AbsentPolicy {
    FallbackLocal { path: PathBuf }, // consumer-owned explicit internal path
    Wait,                            // caller decides; Barge reports unavailable
    Fail { attach: String },         // fail, naming the drive to attach
}
pub enum PrunePolicy {
    None,
    OldestIdleAfter { age: Duration },
}
pub enum ReserveParticipation { Counts, Excluded }
```

The `area` module exposes, and nothing more:

- `resolve(declaration, volume) -> ResolvedArea` — the descriptor-verified
  folder for this volume, with its overrides and policies. Resolution creates
  nothing.
- `create_root(resolved, RootSpec) -> OwnedRoot` — exclusively creates one root
  directory with a fresh 128-bit id, writes its marker before any consumer
  payload, and returns the root's identity (device, inode, marker digest,
  volume UUID).
- `list_roots(resolved, Bounds) -> Vec<RootObservation>` — bounded, whole-root
  observations with marker state and last-activity facts. No consumer content
  is interpreted.
- `hold(owned_root, HoldScope) -> Hold`, `release(Hold)` — cooperative custody.
- `prune(resolved, Plan) -> PruneReport` — evaluates the declared policy over
  observed roots and deletes whole idle roots, in the plan's exact order.
- `space(resolved) -> SpaceObservation` — available and total bytes of the
  filesystem that actually holds the area, with source and time.

Barge interprets nothing inside a root: slot names, ledgers, build outputs,
members and receipts are the consumer's. A root is a directory with a marker and
an identity; that is the whole vocabulary shared with a consumer.

### Area declaration record on disk

For a CLI consumer, `barge area declare --json` writes
`state/areas/<namespace>/<area>.json`:

```json
{
  "schema": "barge.area/1",
  "namespace": "example",
  "area": "pool",
  "folder": "example/pool",
  "overrides": { "<volume-uuid>": "example-pool" },
  "absent": {"kind": "fail", "attach": "the build volume"},
  "prune": {"kind": "oldest_idle_after", "age_seconds": 604800},
  "reserve": "counts",
  "declared_ms": 0,
  "generation": 1
}
```

Validation is strict and bounded: `namespace` and `area` are single path
components matching `[a-z0-9][a-z0-9._-]{0,63}`; `folder` and every override
must pass [containment](#path-containment-and-resolution); the document is at
most 64 KiB with no unknown fields. A declaration whose `folder` or override
collides with another declaration, with `scratch/` or `caches/`, or with a
resolved path of another area is refused — two owners of one folder is not a
state Barge will write.

A library consumer may pass the declaration in memory instead. Authority is the
same either way: the declaration Barge is given is applied, and the effective
declaration is recorded in each root's marker and in the placement record so a
later reader can see which policies produced it. Whether the on-disk document or
the caller's object is authoritative when both exist is [open design
issue O-003](#open-design-issues).

### Owned roots, markers and holds

Every directory Barge creates carries `.barge-root.json` (`barge.root/1`),
written once, atomically, immediately after the directory is exclusively
created and before any payload:

```json
{
  "schema": "barge.root/1",
  "root_id": "<32 hex>",
  "nonce": "<32 hex>",
  "volume_uuid": "<uuid>",
  "owner": "barge",
  "kind": "scratch_run",
  "area": {"namespace": "example", "area": "pool"},
  "resolved_path": "/Volumes/Builds/example-pool/<root>",
  "created_ms": 0,
  "generation": 1,
  "origin": "created",
  "conclusion": null
}
```

`kind` is `scratch_run`, `cache_root` or `area_root`. `origin` is `created` or
`adopted_existing` (see [cache adoption](#adopting-the-existing-hand-made-placements)).
For `scratch_run`, `conclusion` is the conclusion object described under
[scratch allocation](#allocation-and-conclusion); it is `null` while the root
is running, and it is written only by a genuine conclusion.

The marker is validated strictly on every read: exactly the schema's field set,
a known version, a `root_id`/`nonce` of the right shape, the volume UUID equal to
the verified volume, the resolved path equal to the path actually walked, a
regular file owned by this user, private mode, at most 4 KiB, no symlink. An
absent, foreign, symlinked, oversized, corrupt, unknown-version or
schema-divergent marker makes the root *unknown*: never reusable, never
deletable, always reported. A marker is removed only after its root's contents
are gone, and never rewritten to add a conclusion that a different process
could have raced.

Holds are cooperative and recorded: `hold` writes
`state/holds/<root-id>/<holder-id>.json` (`barge.hold/1`) with the holder's
pid, process start identity, boot identity, scope, purpose and time, before the
consumer touches the root. `release` removes it. A hold file whose process is
positively gone is stale and may be removed by the owner of that root under the
root lock; a hold file that cannot be decided (unreadable, foreign uid, unknown
version, pid alive but start identity unreadable) is *unknown*, and unknown
blocks deletion. Holds are advice to other cooperative writers plus evidence to
custody; they are not a kernel lock and this design does not claim they are.

### Kept placements

A kept placement is a path the user chose on a kept volume, outside every
managed folder of that volume (outside the resolved `scratch/` and `caches/`
folders and outside every declared area, including its per-volume overrides).
Barge records the placement and the owned link at the tool's path; it never
marker-marks, prunes, moves, recreates or backs up that data. Being outside the
managed folders is what marks it kept, and the placement record's
`durability: kept` is derived from that check at adopt time and re-verified at
every use.

The containment reading matters because `root_dir` may be empty, which makes the
mount root the folder that holds "everything Barge owns". The design reads the
rulings as: kept data must be outside every folder Barge manages (the feature
folders and the declared areas), not outside `root_dir` itself — otherwise kept
data could not live on a drive whose `root_dir` is empty. This reading and its
alternative are [open design issue O-004](#open-design-issues); the
implementation brief must not start before the owner confirms it.

When a kept volume is absent, the placement is unavailable and the message names
the drive. Nothing is created, nothing falls back, nothing is deleted. A kept
placement whose recorded path now resolves inside a managed folder (for example
because an area was declared over it) refuses at use with `refused_containment`
and changes nothing; the owner resolves it by moving the declaration or the
data.

## Project scratch areas

### Allocation and conclusion

`barge scratch alloc --project <name> --consumer <id> --json` resolves the
volume's scratch folder, verifies the volume, creates `<scratch>/<project>/` if
absent (a plain directory of this user, `0755` at the umask, never a symlink),
then exclusively creates one root `<scratch>/<project>/<root-id>/` with `0700`
and its marker, and returns the path, root id, volume UUID and device. The
caller (Effigy, Oxide's proof supervisor, a script) uses that directory as its
run's temp. Two concurrent allocators can never receive the same root: creation
is exclusive and the marker's id is fresh.

The root's conclusion is recorded once, by its owner, with five facts mirroring
the shape Oxide's supervisor already proved:

```json
{"concluded_ms": 0, "outcome": "closed", "closure": "positive",
 "kept_reason": "evidence", "nested_scope": "exclusive"}
```

- `outcome` ∈ `closed`, `unclosed`, `failed`, `retained`; `closure` ∈
  `positive`, `none`; `kept_reason` ∈ `none`, `evidence`, `unclosed`,
  `exception`, `nested-unclosed`; `nested_scope` ∈ `exclusive`, `hosting`.
- Only `closure: positive` with `nested_scope: exclusive` and an expirable
  `kept_reason` makes a root eligible for expiry. `hosting` means the root may
  contain resources created by another authority, so the owner's own closure is
  never enough; only that authority can conclude it. This is the same rule
  Oxide's supervisor adopted after `oxide#151`, and Barge keeps it because a
  proof scratch that hosts child runs must not be expired by its parent.
- A conclusion is terminal: an existing conclusion is never downgraded, and a
  conclusion write re-verifies the marker identity it read. An abrupt exit
  leaves the initial `null` conclusion, which is never read as "finished".

`barge scratch conclude <root> --json` writes that object; it requires the
caller to be the recorded owner, a live hold on the root, and a positive closure
claim that its own custody evidence supports. A run that cannot claim positive
closure records `closure: none` — an honest non-conclusion, not a failure to
report.

### Expiry

`barge scratch expire --json` (also reached through `maintain`) sweeps one
volume's scratch folder, whole roots only, with fixed bounds:

- Eligible: a `scratch_run` root whose marker records `closure: positive`,
  `nested_scope: exclusive`, `kept_reason: evidence`, and a `concluded_ms`
  strictly older than the policy age (default seven days, per-area or
  per-volume override). `qualify-base` is an Oxide marker reason, not a Barge
  one: Barge's own `kept_reason` enum cannot produce it, so it is never listed
  as eligible here.
- Never eligible: unconcluded, unknown, `retained`, `unclosed`, `failed` with
  `kept_reason` `unclosed`/`exception`/`nested-unclosed`, `hosting`, another
  user's root, another device, a symlink at any level, a foreign marker family
  without a recorded delegation, an unmarked unit without a recorded legacy
  claim, or anything Barge cannot positively identify.
- Bounds: 1 s and 4096 examined entries per sweep, checked between filesystem
  steps, plus a per-root delete deadline (see [bounded
  resources](#bounded-resources)). Exhaustion leaves the unreached work for a
  later run and reports it; there is no background reaper and no retry loop.
- Order: oldest conclusion first. The preview (`--plan`) is the same
  computation with no effect and prints the stable plan plus its SHA-256; a real
  run takes exactly that plan and digest and re-verifies each root's identity,
  marker digest, device, inode, custody and volume before its intent.
- A failed delete leaves a pending intent and exits `3`; a later run replays it
  only from the recorded identity.

### Compatibility with Effigy and Oxide roots

Barge must subsume the host reaper without breaking two live marker families and
without either leaking the trees the reaper used to delete or over-deleting
something it never owned. One positive rule does that: **nothing in a scratch
folder is deleted unless a recorded grant covers the unit, that grant's own
eligibility predicate holds, and custody is `Safe`.** There is no delete path
that skips a grant, and no grant authorizes a different owner's data.

A *grant* is one of three recorded forms:

| Grant | Written by | Covers | Eligibility predicate |
| --- | --- | --- | --- |
| Native root | Barge, at allocation | one `scratch_run` root it created | [the expiry rule](#expiry) |
| Foreign delegation | the family's owner, once per family and volume, in the volume's `scratch_grants` | roots carrying that family's marker | that family's own rule, exactly as pinned below |
| Legacy claim | a human, once per scope, at declaration time | unmarked units in one declared scope | the claim's declared idle age and unit shape |

The three marker families, with the field sets pinned to the sources above:

| Family | Marker | Fields Barge reads | Who concludes | Eligibility Barge applies |
| --- | --- | --- | --- | --- |
| Barge | `.barge-root.json`, `kind: scratch_run` | `barge.root/1`, exactly its field set | Barge | the [expiry](#expiry) rule |
| Oxide scratch | `.oxide-scratch.json` (`version: 1`) | `version`, `run_id`, `root`, `owner_uid`, `device`, `inode`, `created_unix`, `concluded_unix`, `outcome`, `closure`, `kept_reason`, `nested_scope` | Oxide's supervisor only | with a delegation: `closure: positive`, `nested_scope: exclusive`, `kept_reason` in `evidence`/`qualify-base`, concluded strictly more than seven days ago |
| Oxide validation temp | none, plain `oxide-validation/run.*` | none | Oxide | never; not a marked root and never claimed |
| Effigy | `.effigy-scratch.json` (`Marker`) | `run` (required), `task`, `checkout`, `pid`, `pid_start` (all four defaulted by the pinned source, so absent is valid), `created_ms`, optional `keep`, optional `concluded_ms` | Effigy only | with a delegation: `keep: true` with `concluded_ms` older than 24 h, or no conclusion and the recorded `pid`/`pid_start` positively gone, or `created_ms` older than seven days |

Reading a foreign marker is read-only: Barge parses it strictly *against that
family's pinned field set* — an omitted field the family defaults is valid, not
`unknown_state`; an extra field, an unknown version, a wrong type or an
unreadable file is `unknown_state` and the unit is kept. Barge never writes a
foreign marker, never concludes a foreign root, never converts one to a Barge
marker, and never applies its own seven-day rule to one.

**Legacy claims.** The current host job does not skip unmarked scratch: its
`reap.py` deletes each child of every project folder under `scratch/` after
their newest entry is fourteen days old and no process has them open, deletes
top-level entries that are files or whose names contain a dot as single units
the same way, and skips only symlinks, the `oxide` project, Effigy-marked
directories and bare eight-character run names. That behaviour is the thing to
replace, so the design reproduces it as an explicit grant instead of an
implicit name rule. A legacy claim is declared once, per scope, by the owner of
the data:

- The record shape is the volume's `scratch_grants`
  ([registry](#registry-of-volumes)); one claim per scope, with its declared
  idle age, unit shape and excluded families.
- `scope.kind` is `project` (children of `<scratch>/<project>/`) or
  `top_level_adhoc` (entries directly under `<scratch>/` whose name contains a
  dot, or that are not directories, matching the reaper's units). A claim never
  covers the scratch root as a whole and never covers a named project folder.
- A claimed unit is eligible only when it carries no marker of any recognised
  family, is not a symlink at any level, the owning project folder (when the
  scope is a project) has a valid claim, the newest entry in the unit is older
  than the claimed idle age, no hold covers it, custody is `Safe`, and the
  delete is whole-unit with the Finder and `ENOTEMPTY` rules below. A unit that
  becomes marked between plan and effect is kept.
- A claim is reversible and per scope. Removing it returns the scope to
  keep-and-report without touching data.
- A claim is an operator declaration, recorded in the volume's `scratch_grants`
  beside any foreign delegation. It is never inferred from a directory name, a
  marker's absence or an mtime.

Until a family's delegation or a scope's claim exists, `maintain --plan` reports
the units such a grant *would* cover, with counts and reasons, and deletes
nothing there. That is what makes replacing the host job staged per scope: the
plan names every leftover, so a missing grant cannot leak silently, and no
grant means no deletion rather than an over-delete. The same rule is why the
reaper's replacement is a sequence of recorded declarations, each reviewed
against the job's own dry run, not a single switch.

Two layout facts must be preserved rather than tidied:

- Effigy's existing run folders live directly under the scratch root
  (`<mount>/scratch/<8-char>`), beside the project folders, because it was
  configured with `[scratch] root = /Volumes/Builds/scratch`. Barge's own roots
  are one level deeper (`<scratch>/<project>/<root-id>`). The expiry sweep
  therefore recognises both levels and never treats an eight-character project
  folder as a run root, or the reverse; a name never establishes ownership and
  a bare eight-character directory is only ever covered by a `top_level_adhoc`
  claim. Moving Effigy under `<scratch>/effigy/` is a consumer change for a
  later task.
- Oxide's validation temp is `<mount>/scratch/oxide-validation/`, a sibling of
  its `scratch/oxide` subtree, outside the seven-day self-expiry. It is not a
  Barge scratch root, no claim may cover it, and the sweep skips it.

The current host job (`com.tom.scratch-reaper`, daily 04:30) also runs Oxide's
outputs prune. Barge's job replaces only the scratch part, and only after the
scratch slice is qualified and that scope's grants are declared; the prune entry
stays Oxide's until stage 4.

## Package-cache placements

### Kinds and placement forms

Stage 1 designs four kinds:

| Kind | Original path | Form |
| --- | --- | --- |
| `npm-cache` | `<account home>/.npmrc` `cache=` | `setting` |
| `bun-cache` | `<account home>/.bunfig.toml` `[install.cache] dir` | `setting` |
| `cargo-registry` | `<account home>/.cargo/registry` | `held-root` or `link` |
| `cargo-git` | `<account home>/.cargo/git` | `held-root` or `link` |

Original paths resolve from the **account home** (the password-database home
used for state), never from `$HOME`. A `~` in a declaration is expanded once at
declaration and the record holds the absolute path; a later `$HOME` or `~`
change never re-points a placement. Links first (Q-003), and the three placement
forms are distinct records because they need different mechanics:

1. **`setting`** — the tool can only be redirected by its own setting. Barge
   writes that setting once during an explicit adopt and records the file's
   identity (device, inode, size, mtime, digest) and the exact bytes before and
   after. The edit is line-scoped on the named key, with a backup copy written
   first; a file with an unexpected shape (multiple candidate keys, a parse
   error, an unknown format version) refuses rather than editing. No filesystem
   link is involved.
2. **`held-root`** (adopted in place) — the directory already exists where the
   placement wants it, on the registered volume. Adopt writes
   `.barge-root.json` with `origin: adopted_existing` into that directory and
   records its pre-adoption observation (device, inode, entry count, newest
   mtime, allocated bytes). Nothing is renamed, linked, copied or moved. This
   form exists precisely because today's hand-made cache directories are already
   in the right place.
3. **`link`** — the tool's original path should follow the volume. Barge creates
   an owned root under the volume's `caches/` folder and publishes a symlink at
   the original path whose text is the recorded absolute target. A symlink can
   only be published where no directory is in the way, so this form is
   mechanically available in exactly three starting states:
   - the original path is **absent**: create the link under a temporary name in
     the parent and `rename` it into place, so the path is never a missing path
     for a concurrent reader;
   - the original path is **already a symlink**: record it; a link to a foreign
     target is `foreign_owner` unless the explicit adopt names the change;
   - the original path is an **empty real directory**: custody on that directory,
     `rmdir`, then the temporary-name publish. That is the only moment the path
     is briefly absent, which is why the directory must be empty and why custody
     is taken first.

   A **non-empty real directory is never replaced by a link during adopt.** A
   symlink cannot be renamed over a directory, and deleting or moving the
   directory's contents would be exactly the data move the design forbids here.
   Adopt refuses with exit `2` and prints a plan naming the directory, its
   observed entries and the two real options: adopt it in place as a
   `held-root`, or relocate its contents with the separate explicit
   operator-authorized relocation command (below).

**Relocation is its own operation, not part of adopt.** Moving existing cache
contents onto the volume needs explicit operator approval for that action, a
custody proof on both the source and the destination, a durable per-entry
journal, verification after each batch, and its own undo. It is a later slice
with its own gate; nothing in this design moves consumer data.

`barge cache undo --json` reverses exactly the recorded form: a `setting`
restores the recorded bytes only if the file still has the exact after-bytes
Barge wrote (otherwise it refuses and reports the divergence, changing nothing);
a `link` removes the link and restores an empty directory where one was
replaced, or removes the link where the original was absent; a `held-root` undo
removes only the marker it wrote, and refuses if the directory's identity
changed. Undo never reverses a relocation; that operation's own undo does.

Absent volume (Q-001): a cache placement fails clearly, naming the drive to
attach. There is never a silent internal fallback, no re-download to the
internal SSD, and no "create an empty directory so the tool proceeds" — an
absent link target or root is an error, and Barge never recreates the mount
point.

### Adopting the existing hand-made placements

Today `<account home>/.npmrc` and `/.bunfig.toml` point at
`/Volumes/Builds/caches/{npm,bun}` and `/.cargo/{registry,git}` are links into
`/Volumes/Builds/caches/cargo/`. Adoption is in place, with no data move:

- A tool setting that already names a path under a registered volume's
  `caches/` folder is recorded as a `setting` placement and the file is not
  written: no byte, mode or timestamp change. This is the expected case today.
- A symmetric link into a registered volume's `caches/` folder is recorded as a
  `link` placement with no write. Its target directory carries no Barge marker,
  so the same adopt also offers `--adopt-existing-root`: that writes the marker
  as a `held-root` record, additively, and still moves nothing. Without the flag
  adopt reports what it would do and exits `2`, changing nothing.
- A real directory is not linked over. If it already sits on the registered
  volume at the intended path it is adopted as a `held-root` (marker only, with
  `--adopt-existing-root`); if it sits on the internal drive it refuses with the
  plan described above, and the data move stays an explicit operator operation.
- A setting or link that points at a *different* external path, at the internal
  drive, or at a path on an unregistered volume is not adopted silently: adopt
  names the current value and the intended one and requires the explicit adopt.
- The Cargo `cargo-swap.py` host script's behaviour (swapping the registry
  directory) is replaced, not wrapped: a stable link or a `held-root` whose root
  is prunable only by policy with custody needs no swap script once the kind is
  qualified.

## Writer custody and safe deletion

### Capability limits on macOS and APFS

There is no unprivileged macOS primitive that positively proves "no process has
this subtree open" or "this process has exited and left nothing behind". The
design states the limit and builds only on facts it can establish:

| Fact Barge needs | Available evidence | Limit |
| --- | --- | --- |
| A cooperative holder is done | its hold file was released, or its recorded process is positively gone | a process that never took a hold, or escaped its group, is outside this |
| No same-uid process has an open descriptor under the root | enumerating its fds (`proc_pidinfo`/`PROC_PIDLISTFDS`) and matching vnode paths, or an equivalent bounded OS-layer scan | a process of another uid, or one created after the scan, is not observed |
| The root is the inode that was verified | `fstat` device + inode through the opened descriptor before and after each step | a rename by another user between steps stops the operation |
| The volume is still the registered one | descriptor `fstatfs` + `ATTR_VOL_UUID`, re-verified | a vanished volume fails the call, it does not answer "absent and safe" |

`lsof -nP +D` (the host script's current tool) is not used as proof: it is an
external process, unbounded in time and output, and its silence is not a kernel
guarantee. It may appear only as a diagnostic in a report, never as a custody
verdict.

The verdict is therefore explicitly *cooperative plus same-uid observation*.
The residual case — a process of another user, or one that dropped its hold and
kept writing — is named here and in `describe` (`custody.capability`), and is
the reason deletion additionally requires the intent journal and quarantine
rename below, which make a wrong deletion recoverable up to the marker-unlink
step. Barge never claims more than this, and no unknown becomes safe.

### Custody protocol

Custody is a state machine with one authorizing state:

```text
Unknown(reason)     -- cannot decide; never authorizes
Unsafe(reason)      -- a holder or an open descriptor was found
Safe(evidence)      -- authorizes swap or delete
```

`Safe` requires all of: every recorded hold for the root released or positively
stale; the same-uid open-descriptor scan complete within its bound and empty for
this root; the root's device, inode and marker digest equal to the observation
the caller was given; the volume verified; and no pending intent naming another
root. Any unreadable hold, an unreadable process entry, a scan that hits its
entry or time bound, a foreign-uid holder, or a `lsof`-style diagnostic that
errors makes the verdict `Unknown`, which is a refusal. Deletion and swap take a
`Safe` value by type, so a caller cannot pass a boolean.

Custody is taken before a link swap, before a root's quarantine rename, and
before a link removal, and it is re-checked after the intent publish and before
the first destructive step. It is not a lease on the data: a consumer that is
mid-write without a hold is a consumer Barge cannot see, which is exactly the
limit above and the reason adoption is explicit and holds are the documented
protocol.

### Whole-root deletion, Finder metadata and quarantine

Deletion is whole-root only, of owned roots only, descriptor-relative, and
bounded:

1. Verify the root and its marker; take custody; write
   `barge.intent/1` (`intent` phase) recording root id, marker digest, device,
   inode, volume UUID, plan digest and time. The intent is durable before any
   rename.
2. Rename the root inside its area to `.barge-prune-<intent-id>` with
   `renameat`, then record the `renamed` phase. A same-device rename is atomic;
   a cross-device or changed-inode result stops before deletion.
3. Walk and delete through descriptors opened from the area's descriptor:
   `openat` with `O_NOFOLLOW` per directory, `unlinkat` (with `AT_REMOVEDIR`
   for directories) for contents, never following a symlink, with a depth bound
   (12) and per-directory name bound (200 000) and a per-root deadline (120 s).
4. Finder tolerance is exactly Oxide's two forms and nothing broader:
   `.DS_Store`, or `._` followed by a nonempty name; a regular file, no-follow,
   owned by this user, on the directory's device. Every other unexpected entry
   stops the deletion and keeps the root. Finder metadata is deleted as part of
   the whole root; it never establishes ownership, never makes a root reusable
   and never permits a narrower delete.
5. `ENOTEMPTY`/`EEXIST` from `rmdir`/`unlink` retries the enclosing directory at
   most three times with a one-second pause, because Finder can recreate
   `.DS_Store` mid-delete. After the third failure the intent stays pending, the
   quarantine leaf is kept, and the command exits `3`. It does not widen what it
   deletes.
6. The marker is unlinked last. Only after that may the quarantine leaf be
   removed. Then the `deleted` phase is recorded and the intent is settled.
7. On replay, a pending intent is resolved only from its recorded identity: the
   quarantine leaf exists with the recorded device and inode and the marker
   digest matches, or both the leaf and the marker are positively absent (in
   which case the marker's absence plus a completed `marker_last` phase is the
   proof). Anything else stays pending, is reported, and blocks reuse of that
   leaf forever. A symlink inside a quarantine stops the deletion and leaves the
   intent pending; it is never followed and never removed.

Deletion never touches a foreign marker family, a marker-less directory, an
`adopted_existing` root without an explicit plan entry, a root on another
device, or a path outside the area. A whole-root delete of an adopted existing
cache root deletes the consumer's data, so it is allowed only through the area's
declared prune policy or an explicit operator command that names the root.

## Scheduling without a daemon

Barge owns no timer, daemon or hook. A host launchd job calls one command:

```text
/usr/local/bin/barge maintain --json [--plan <file> --plan-sha256 <hex>]
```

- `maintain` performs scratch expiry for every registered, present volume, then
  each area's declared prune policy, in a fixed order, and nothing else. It
  reads the registry, verifies volumes, and writes only the intents and results
  its rules allow.
- Preview then real, like Oxide's prune: `--plan` prints one bounded canonical
  plan document (`barge.maintain.plan/1`) and its SHA-256; a real run requires
  the exact plan bytes and re-verifies every predicate before each effect. The
  host job runs the preview, and runs the real step only when the preview names
  at least one candidate, passing that preview's digest. A non-zero exit is
  recorded and never retried by Barge.
- Exit codes as above: `0` completed (including a partial completion with no
  pending intent), `2` refused before any effect, `3` a pending intent or
  unknown state that needs a human. A timeout is never success.
- Volume absence is not an error for an `ephemeral` volume: `maintain` reports
  the volume absent and skips it. For a `permanent` volume it reports it loudly
  and continues with the other volumes; it never recreates a mount point.
- The job's own file (`~/Library/LaunchAgents/<label>.plist`), its schedule and
  its replacement of `com.tom.scratch-reaper` are host changes with their own
  operator gate. This design defines the command and its contract only.
- Oxide's outputs prune entry stays in the existing host job until stage 4;
  `maintain` never runs it, never reads a receipt, and never deletes an Oxide
  root.

## Bounded resources

Every operation declares its bound; hitting one leaves work for later rather
than growing without limit.

| Bound | Value |
| --- | --- |
| Marker size / registry size / area declaration | 4 KiB / 4 MiB / 64 KiB |
| Registry rows | 64 volumes, 128 areas, 4096 placements |
| Read deadline | 10 s per bounded file, 64 MiB |
| Directory names per listing | 200 000 |
| Deletion depth / per-root deadline | 12 levels / 120 s |
| Expiry sweep | 1 s and 4096 examined entries |
| Maintain run | 600 s wall, 512 MiB owned scratch, stopping 30 s before the deadline |
| Mount walk re-verification | every 512 entries and after each unlink |
| Hold files per root | 64 |

A bound that is reached reports what was not reached, with counts; it never
reports the unfinished part as done.

## Failure, race and cancellation matrix

Every row is a rule the implementation must carry a test for. "Keeps" means the
data, marker and record are left unchanged and reported.

| Trigger | Detection | Action |
| --- | --- | --- |
| Drive pulled before a command | mount root will not open, UUID or device mismatch, `EIO`/`ENOENT` on the volume descriptor | refuse `volume_absent` naming the drive; nothing created, nothing deleted; kept data is unavailable, never "gone" |
| Drive pulled mid-delete | descriptor-relative call fails after the quarantine rename | stop; keep the pending intent and the quarantine leaf; exit `3`; replay only after the volume is reverified and the recorded identity matches |
| Drive pulled mid-adopt | the form's publish step fails | a `setting` adopt leaves either the old or the new bytes and a pending intent that `undo` and replay settle; a `link` adopt publishes through a temporary name, so the original path is never a dangling name in the absent case, and the empty-directory case re-creates the empty directory on replay instead of leaving it missing; a `held-root` adopt either wrote the marker or did not |
| Finder recreates `.DS_Store` | `ENOTEMPTY`/`EEXIST` from `rmdir` | three attempts, 1 s apart; then keep the pending quarantine and exit `3` |
| Finder writes a non-metadata entry | refusal in the walk | stop, keep the root, report the escaped name; never widen the delete |
| Two allocators race | exclusive `mkdir` with the fresh id | the loser gets `EEXIST` and retries with a new id; no shared root, no adoption of the other's marker |
| Two expiries race | root lock plus intent publish | the second sees the pending intent and skips that root; a stale reader's identity check fails and it changes nothing |
| Holder alive but unreadable | process start identity unavailable | custody `Unknown`; refuse; keep |
| Holder pid reused by another process | pid + start identity + boot identity mismatch | treat the hold as stale only when all three positively disagree with the record; otherwise `Unknown` |
| Crash between marker and payload | marker present, root empty | root is a valid empty run root; expiry may take it under the ordinary policy; nothing is inferred as concluded |
| Crash between intent and rename | intent `intent`, root still in place | replay rolls back the intent and re-evaluates the root; not a completion |
| Crash between rename and `renamed` phase | quarantine leaf exists with recorded identity | replay resumes deletion from the recorded identity |
| Crash after `marker_last` before `deleted` | marker absent, quarantine leaf may be present | both-absent proof settles; otherwise pending |
| Conclusion racing expiry | conclusion write requires a live hold; expiry requires `positive` closure | a root with a live hold is never eligible; a concluded root's marker identity is re-verified before its rename |
| `root_dir` changed while a root exists | recorded path vs configured resolution | recorded beats configured; `describe` reports both; no move |
| Same mount, different volume | UUID/device mismatch | refuse `volume_mismatch`; never use, never delete |
| Stale mount-point directory | path exists, but not a mount root of the registered UUID | never write through it; never recreate it |
| Marker-less non-empty directory in an area | walk sees no valid marker | unknown: skip, report, never delete or adopt |
| Unmarked scratch unit with no legacy claim | no recognised marker and no claim covering its scope | keep and report; `maintain --plan` lists what a claim would cover; never infer a claim from a name or an mtime |
| Foreign scratch family with no delegation | an Oxide or Effigy marker whose family has no recorded delegation for that volume | keep and report; never apply Barge's age or conclusion rule to it |
| Unit becomes marked between plan and effect | identity or marker re-verification at the effect step | keep the unit; the effect is abandoned with no partial delete |
| Legacy claim revoked between plan and effect | claim re-read under the volume lock | keep the unit; the claim is never cached across the run |
| Cache original path is a non-empty real directory | adopt sees a real directory with entries | refuse `refused_boundary` with a plan (adopt in place as `held-root`, or the separate relocation operation); nothing is renamed, linked, deleted or copied |
| Cache undo after the file diverged | after-bytes digest mismatch | refuse, report the divergence, change nothing |
| Foreign marker family in a Barge area | marker name/owner differs | skip and report; the owner's rule decides |
| `EACCES` inside a tree | walk error | bounded skip with the reason; never abort the whole sweep into a fallback; the root keeps its marker |
| Clock skew or future-dated marker | conclusion/creation time ahead of now | not eligible; report `unknown_state` |
| Unreadable or oversized marker | size/parse bound | unknown: skip, report |
| Unknown JSON version or field anywhere | strict parse | refuse `unknown_state`, name the file, change nothing |
| Operator cancels (SIGINT/SIGTERM) | signal | stop after the current durable step; leave a pending intent rather than an unrecorded effect; exit non-zero |
| `ENOSPC` on the state volume | write failure | refuse before effect; publish nothing partial |
| Lock held by another process | lock file with a live owner | wait within the command deadline, then refuse `locked`; never break a lock whose owner is unknown |

## Compatibility and staging with Oxide and Effigy

| Format | Read by Barge stage 1 | Written by Barge stage 1 | Retirement |
| --- | --- | --- | --- |
| Oxide `removable.json` v1/v2 | yes, strict, read-only | never | only when Oxide moves registration to Barge (stage 3) and both sides agree |
| Oxide `pools.json` | yes, informational | never | with Oxide's stage 3 adoption |
| Oxide `oxide-outputs` root markers `root/1`, `root/2` | yes, identify and report only | never | with Oxide's stage 4 area adoption |
| Oxide receipts, registry, prune journals, tombstones | presence and version only | never | with Oxide's stage 4 |
| Oxide `oxide-pool` payload and slots | not interpreted | never | stage 3/4 |
| Oxide `.oxide-scratch.json` and validation temp | yes, strict; with a recorded delegation, its own expiry rule, otherwise keep and report | never | with Oxide's scratch adoption |
| Effigy `.effigy-scratch.json` | yes, strict against the pinned field set; with a recorded delegation, its own expiry rule, otherwise keep and report | never | with Effigy's own adoption |
| Barge registry, areas, placements, roots, holds, intents | — | yes, authoritative | — |

Rules that keep this safe:

- Barge never rewrites, moves, renames, marker-marks or deletes a foreign owner's
  data. Compatibility is read-only until the owner's own adoption slice grants
  it; a foreign marker is read, reported and kept until a delegation or a legacy
  claim covers it.
- Every scratch deletion needs a recorded grant (native root, foreign family
  delegation, or legacy claim). A missing grant means keep and report, never a
  name-based or age-based delete.
- A foreign format's unknown field, unknown version or unreadable bytes make the
  fact unknown. Unknown is never treated as absent, idle, empty or safe.
- Paths are recorded at adopt and never silently moved. `root_dir` and folder
  overrides affect new resolutions only.
- The versioned JSON contract is the CLI's compatibility surface; a consumer
  written against `barge.describe/1` keeps working when optional fields are
  added and refuses a document it does not understand rather than guessing.
- No stage boundary is closed by this design: registration writes, reserve
  pruning, Oxide's area adoption, and the host-job replacement each need their
  own brief, review and (where they touch a real drive) operator gate.

## Alternatives considered and rejected

| Alternative | Why rejected |
| --- | --- |
| Per-`HOME` state | Two shells, a fixture `HOME` or a service would produce two authorities for one account. State is keyed by the account, not the environment. |
| One `barge.toml` per volume as the only truth | A config file on a drive cannot be authoritative while the drive is absent, and an absent drive would read as "no configuration". Identity and policy live in account state; markers live with the data. |
| A daemon or `launchd` watcher | The vision excludes a daemon in the first version; a one-shot command with preview/plan and exit codes is enough, testable and non-blocking. |
| `kept/` group folder | Retired by Tom's ruling (`c80e2034`): kept data lives where the user organises it, outside managed folders, and is never owned. |
| Oxide-specific `outputs/` and `pools/` folders in Barge | Refined by `e5e7bffa`: Barge has no consumer knowledge. Broad names would either grow per-consumer code or mis-file data. |
| One `delete` primitive without a quarantine rename | A crash mid-walk leaves a partially deleted root with a valid marker, which the next run could read as a complete root. Rename-then-delete with a journal keeps the identity recoverable. |
| Deleting unmarked scratch by name or age alone | A name is not ownership and mtime is not a conclusion, and the live reaper's name rules are exactly what must not survive the handover. A legacy claim records the scope, unit shape, idle age and excluded families once, so the replacement deletes what the operator meant and reports everything else. |
| Deleting a foreign marker family's roots under Barge's own age rule | It would silently outlive or contradict the owner's conclusion contract. A family's own rule applies only after that owner's explicit delegation. |
| Replacing a real cache directory with a link during adopt | A symlink cannot be renamed over a directory; moving or deleting its contents would be the data move this design forbids. Adopt does the in-place `held-root` form or refuses with a plan, and relocation stays a separate authorized operation. |
| `lsof` as the custody proof | External, unbounded, and its silence is not a kernel guarantee. It is a diagnostic at most. |
| Treating a same-device link as "no data move" without a record | Without the recorded link text and root identity, an undo cannot prove what it is restoring. Every link is recorded. |
| Hard links or copies for cache placement | A copy doubles writes (the opposite of the product's purpose) and a hard link cannot cross volumes; the link is a symlink with a recorded target. |
| Reserve and floor pruning in stage 1 | It needs qualified space measurement, volume policy and a real drive; out of stage 1 scope. |
| Linux plumbing now | Q-005 fixes macOS/APFS only. The OS layer is the seam; inventing a Linux backend now would be untested design spread over two platforms. |
| A "force delete" escape hatch | Any force path would let an unknown become safe. Repair is an explicit operator command with an explicit plan, not a bypass. |

## Implementation slices and dependencies

Finite, in order; each slice lands with its tests and its documentation update.
No slice authorizes a native run, a host change or a data move on its own.

| Slice | Content | Depends on |
| --- | --- | --- |
| S1 | Workspace, both crates, toolchain pin, RustQuality activation, `os` traits with the macOS implementation, volume identity and mount verification, `barge describe --json` skeleton, JSON/error contract, docs check wiring | this design frozen |
| S2 | Account state directory, atomic publish, locks, bounded reads, `barge.registry/1` with read-only import of Oxide's `removable.json`/`pools.json` and `barge inspect` | S1 |
| S3 | Path containment and the descriptor walk, area declarations (`barge.area/1`), owned roots and markers, holds, root listing, `barge area` verbs | S1, S2 |
| S4 | Scratch allocation, conclusion, expiry sweep with plan/digest, grants (foreign-family delegations and legacy claims) with their recorded declarations, `barge scratch` verbs | S3 |
| S5 | Custody state machine, same-uid descriptor observation, intent journal, quarantine, whole-root delete with Finder and ENOTEMPTY rules, replay and settlement | S3 |
| S6 | Cache kinds, the three placement forms (`setting`, `held-root`, `link`), adopt/undo with byte-level and link/root identity proofs, refusal of a non-empty real directory, in-place adoption of today's hand-made placements | S3, S5 |
| S7 | `barge maintain` preview/real, exit codes, `describe` completion, host-job handoff contract (the plist change itself stays an operator gate) | S4, S5, S6 |
| S8 | Consumer adoption: Oxide declares its areas and switches scratch, caches, registration, then owned-root mechanics; Effigy moves to Barge scratch. Separate briefs, Oxide's own Queue project, explicit operator gates | S1–S7, separately qualified |

Dependencies that are not slices: a real-drive qualification (later, operator
gated), Linux (later, additive behind `os`), reserve/floor pruning (later,
after a qualified space measurement), and the retirement of any foreign format
(after its owner's adoption).

### Implementation status

The design is frozen and each slice lands with its own brief and independent
review. This section records what has landed and the concrete choices a slice
had to make inside these rules. It is a status note, not a change to any rule
above.

- **S1** landed on `main` (`ad6373e`): the workspace with both crates, the
  toolchain pin, RustQuality strict activation, the `os` traits with the macOS
  implementation, volume identity and mount verification, the JSON and error
  contract, and `barge describe --json`.
- **S2** landed with this note: the account state directory with its atomic
  publish, bounded reads and fixed lock order; the `barge.registry/1` strict
  reader and its fixture-tested writer; the read-only Oxide observations; and
  `barge inspect --json`. S2 creates and changes no registration: no CLI command
  writes Barge's state.

Choices S2 made where this design left the mechanic to the slice:

- **Registry document.** `state/registry.json` holds
  `{"schema": "barge.registry/1", "volumes": [...]}`. Each record carries exactly
  the fields of [the registry table](#registry-of-volumes), with
  `deny_unknown_fields` at every level, at most 4 MiB and 64 records.
  `generation` is per record: a new record starts at 1, a changed one increases
  by one, and an unchanged one keeps its generation, so a stale reader is
  detectable. A write needs the exclusive registry lock by argument, so a
  registry write without the lock cannot happen by accident.
- **Finding Oxide's registration.** The design names
  `<internal pool>/oxide-managed/removable.json` but not how the internal pool is
  found, which Oxide's own configuration decides. S2 looks at
  `<pool>/oxide-managed/removable.json` for every pool Oxide's `pools.json`
  records, then at Oxide's current and retired default pools
  (`Library/Caches/oxide/build-pool` and `Library/Caches/oxide/pool`). The first
  candidate that is present decides: one that is present but unreadable is
  `unknown_state` and never a cue to look further. `barge inspect` names every
  path it looked at. A pool Oxide created but never recorded in `pools.json`, and
  that is not one of the defaults, is not located by S2.
- **Absence is a fact.** A foreign file that is not there is reported with
  `state: absent` and a detail naming the path — never an empty record and never
  `unknown_state`. `ObservationRow.version` is the version the source states; it
  is empty when the source states none, which is an absent or unreadable file, or
  a format without a version field (the pools record is a bare array).
- **Bounds as applied.** The registry read is 4 MiB, 64 records and 10 s; the
  registration read uses Oxide's own 16 KiB cap; the pools read uses Oxide's own
  64 KiB cap and reports at most Oxide's own 16 recorded rows, with counts when
  the file holds more.
- **Which command refuses.** An invalid registry refuses `barge describe` with
  `unknown_state` naming the file, and refuses every mutating path, because a
  report with no volumes would read as "none registered". `barge inspect` reports
  it as `registry.state: invalid` with the reason, because diagnosing that state
  is what the command is for. Neither command creates a file, directory or lock:
  both take only a shared lock, and only when the lock file already exists.
- **Lock order.** The order is a checked rank (`state::LockSet`): acquiring a rank
  at or below one already held refuses with `usage` before any file is touched,
  the intent write is the last rank and takes no lock file of its own, and the
  `locks/` directory is created only by a mutating call.
- **`barge.inspect/1`.** Its `data` is the state report, the registry as absent,
  valid or invalid with its volume rows, the observation rows, and Oxide's
  registration and pools in full — including the observed volume an import would
  describe (`origin: observed_oxide`, availability from the registration's
  `mode`, durability `undeclared`). That observed volume is a separate type with
  no conversion into a registry record. `barge.inspect/1` is named by its own
  `schema`; it is not added to the `formats` list, so S2 leaves
  `barge.describe/1`'s field set unchanged.

- **S3** landed on `main` with this note: path containment and the
  descriptor walk from a verified volume root; `barge.area/1` declaration
  validation and persistence; owned roots with their strict `barge.root/1`
  markers; cooperative holds (`barge.hold/1`); bounded root listing; and the
  `barge area` verbs (`declare`, `show`, `list`, `root create`, `root list`,
  `hold`, `release`). S3 deletes, prunes and moves nothing, and every fixture
  lives under one temporary directory: the tests use injected volume facts over
  real local directories, and no test reads or writes a real drive or the real
  account state.

Choices S3 made where this design left the mechanic to the slice:

- **Marker shape.** The marker's top-level field set is exactly the one above.
  The nested `area` object carries the pinned `namespace` and `area` and one
  optional `effective`, which records the effective declaration applied at
  creation (folder, absent policy, prune policy, reserve participation) exactly
  as O-003 asks. A marker without `effective` (the pinned two-field form) is
  valid and owns the root's identity only; a missing value never widens a
  policy. The effective declaration is the caller's; a stored declaration that
  differs is reported (`stored_differs: true`) and still never substituted.
- **Root identity.** `create_root` returns the root directory's device and inode
  read through the created descriptor, the verified volume UUID, the root id and
  nonce, and the SHA-256 of the exact marker bytes. The digest is computed by one
  small in-crate SHA-256 (`digest`), because the workspace pins its dependencies
  exactly and stage 1 needs one digest rather than a cryptography surface; S5's
  replay re-reads the same bytes and digest. `read_marker` validates every field
  of the bytes it read — the private mode, the owner, the volume, the walked
  path, the id and nonce shapes and the timestamps — and reports everything else
  as `unknown`; it compares no digest, because the expected digest belongs to the
  caller that recorded the root, and S5's replay is that caller.
- **The walk and folder creation.** Resolution steps from the verified mount
  root through `root_dir` and then the effective folder (an override for the
  volume, else the declared folder). Each component is read with `lstat`, refused
  if it is not a real directory of this account on this device, opened with
  no-follow, and its opened identity compared with the checked one. The first
  absent component ends the walk; `create_root` creates the rest as plain `0755`
  directories and the root as `0700`, exclusively. An existing component is only
  adopted when it is this account's directory on this volume, and every adoption
  path — the named one and the one after a lost `mkdir` race — opens the
  component and compares its opened identity with the checked one, so a
  directory swapped in behind a competing creator is refused rather than
  adopted. A root name that is already taken is retried with a fresh id rather
  than adopted.
- **Collisions.** A declaration is checked on every registered volume: equality
  and nesting between two declarations and against the resolved `scratch/` and
  `caches/` folders (overrides included) are refused as `refused_containment`;
  with no registered volume the folder strings are compared. That is the
  "outside every managed folder" predicate kept placements will use in S6;
  nothing in S3 depends on the O-004 reading further than this.
- **Holds.** `barge.hold/1` records the root id, holder id, pid, the platform's
  process start identity, the boot identity, scope, purpose and time. Reading
  decides `live` when the boot and start identity match, `stale` only when the
  recorded process positively disagrees (a changed boot, a gone pid, or a reused
  pid with a different start identity), and `unknown` when a fact cannot be read
  (unreadable record, foreign-owned, non-regular or non-private file, unknown
  version, unreadable start identity). A live or unknown hold makes a root not idle; only stale and
  absent holds leave it idle. Holds are capped at 64 per root and written under
  the root lock, after the registry read, atomically and privately. The CLI's
  `area hold` records a hold by root id (the id the root's creator has); the
  ownership and marker re-verification before a destructive step is S5's custody,
  not this record.
- **`describe`.** `barge.describe/1` gains a `roots` list (optional on read via
  `#[serde(default)]`, so a document written before S3 still reads) whose rows
  carry the marker's recorded path and the path the current configuration
  resolves, the marker state and the root's holds. Its `areas` rows are now
  filled from the stored declarations. Listing a root walks the configured
  resolution only: after a `root_dir` change the old root is not visible under
  the new configuration, is never moved or recreated, and only the later explicit
  relocation command can move it. A volume that cannot be verified is skipped by
  the root listing and stays visible in the volume rows, so `describe` remains
  usable with a drive unplugged.
- **Semantic kinds.** `RootKind` is `scratch_run`, `cache_root` or `area_root`;
  `RootOrigin` is `created` (S3) or `adopted_existing` (S6). A conclusion is
  recorded only on a `scratch_run` marker, so an area or cache root carrying one
  is unknown.
- **Command documents.** The area verbs name their own envelopes:
  `barge.area.declare/1`, `barge.area.show/1`, `barge.area.list/1`,
  `barge.area.root.create/1`, `barge.area.root.list/1`, `barge.area.hold/1` and
  `barge.area.release/1`. The declaration, marker and hold formats stay
  `barge.area/1`, `barge.root/1` and `barge.hold/1`, and are reported in
  `formats`. Every mutating verb reads the registry first and refuses an invalid
  one before any effect, including creating the state layout; a volume-scoped
  verb also proves the volume is present and is the recorded one, and that its
  durability is declared, before that layout or any lock file is created.
- **Bounds as applied.** A declaration is at most 64 KiB with 128 rows; a marker
  and a hold record are 4 KiB each; a root carries at most 64 holds; a root
  listing keeps 4096 rows and examines at most 200 000 names, reporting
  `truncated` and counts when it stops early.

- **S5** is this slice: the custody state machine with `Safe` required by type,
  the bounded same-uid descriptor observation behind `os`, the `barge.intent/1`
  journal, the quarantine rename, whole-root deletion with the Finder and
  `ENOTEMPTY` rules, and replay and settlement. It is a library capability only:
  no CLI verb deletes, nothing is scheduled, no drive or real account state is
  touched by its tests, and its native proof belongs to S4. Its unit and
  counterfactual tests run on fixture directories with injected OS facts.

Choices S5 made where this design left the mechanic to the slice:

- **The observation primitive.** `ProcessObserver::scan_open_descriptors` walks
  the pid list with `proc_listpids`, keeps the processes whose `PROC_PIDTBSDINFO`
  uid is this account's, lists each one's descriptors with `PROC_PIDLISTFDS`, and
  matches each vnode descriptor's `PROC_PIDFDVNODEPATHINFO` path against the root
  by path components, under both the spelling the walk reached and the one the
  filesystem resolves. `proc_listpids` and `proc_pidfdinfo` are declared in the
  macOS module because `libc` does not declare them; every `unsafe` stays in that
  one seam. A process of another uid is skipped (O-001's named limit), but a
  process list, a process entry or a descriptor entry that cannot be read is an
  error, and an entry or time bound that is reached is `Bounded`. Both make the
  verdict `Unknown`. The concrete bounds are 4096 processes, 65 536 descriptors
  and 10 s per scan; a root carries at most its 64 holds. `lsof` is never read.
- **Hold decisions.** A hold is stale only when its process is positively gone or
  another boot wrote it. A live pid whose start identity differs is `Unknown`,
  not stale: the failure matrix's reused-pid row treats staleness as proved only
  when every recorded identity positively disagrees. S3 decided that case stale;
  S5 corrects it, because `Unknown` blocks deletion and a wrong stale would let a
  reused pid's hold be ignored.
- **The intent record.** `barge.intent/1` sits at
  `state/intents/<intent-id>.json`, at most 4 KiB, with exactly `schema`,
  `intent_id`, `root_id`, `volume_uuid`, `mount_hint`, `root_dir`, `folder`,
  `root_name`, `quarantine_name`, `device`, `inode`, `marker_digest`,
  `plan_digest`, `phase`, `generation`, `recorded_ms` and `phase_ms`. The
  quarantine leaf's name is `.barge-prune-<intent-id>` and is re-derived and
  checked, never trusted as free text. A phase advances only forward and each
  publish is the atomic state write, so a stale writer cannot rewind an intent it
  raced. The journal read is bounded at 4096 records; a stray name, an
  unreadable record or a truncated journal is `unknown_state`, never a skipped
  intent.
- **Deletion order.** The intent is durable, custody is re-decided, the root is
  renamed, `renamed` is durable, the volume and the leaf's identity are
  re-verified, the tree is classified once without mutation, then deleted,
  then the marker is unlinked last, `marker_last` is durable, the leaf is
  removed, and `deleted` settles the intent. The two-pass walk is what makes "an
  unknown entry stops the delete and keeps the root" literal: nothing is
  unlinked until every entry is classified, and the delete pass removes only what
  was classified plus Finder metadata that appeared again. Anything else that
  appears mid-delete stops it, so the delete never widens.
- **A settled intent is kept.** `deleted` is the settlement; the record stays in
  `state/intents/` as the journal's own account of what happened, and replay
  treats it as non-pending work. A rolled-back `intent` phase is the one record
  removed, because nothing happened.
- **Replay proofs.** Only two proofs settle or resume an intent: the quarantine
  leaf exists with the recorded device and inode and the marker bytes digest to
  the recorded digest (resume, or roll back when the root is still in place at
  phase `intent`), or the leaf and marker are positively absent after a recorded
  `marker_last` (settle). Everything else stays pending and is reported. The
  marker digest is returned whenever the marker bytes could be read, even though
  a quarantined leaf's marker necessarily fails the recorded-path check after the
  rename; that is why `read_marker_with_digest` exists beside `read_marker`.
- **Bounds as applied.** Depth 12, 200 000 names per directory, a 120 s per-root
  deadline, and three `ENOTEMPTY` attempts one second apart. The volume is
  re-verified every 512 examined entries and before the first unlink. Hitting a
  bound after the intent is published leaves the intent pending; hitting one
  before it is a refusal that changed nothing.
- **`describe`.** `barge.describe/1` gains an optional `custody` object
  (capability string, the facts it establishes, the cases it excludes, whether a
  kernel proof is claimed — always false — and `lsof` as a diagnostic only). Root
  rows gain an optional `marker_digest`, which is the observation S4 passes to
  custody and to a delete. Both are additions inside the major version, so an
  older document still reads.

## Proposed validation

Design-only validation for this contract is `effigy docs:check` once, run from
the repository root. No Rust, native, APFS or qualified-runner run is part of
this design task; the tests below are proposed for the implementation slices.

### Unit and counterfactual

Unit tests carry the decision tables; counterfactual tests prove the refusals
are real by constructing the state each rule must refuse.

| Area | Unit | Counterfactual |
| --- | --- | --- |
| Containment | component validation; descriptor walk accepts a real directory | absolute path, `..`, empty component, symlinked component, component on another device, component swapped between `lstat` and `open` all refuse with nothing created |
| Path resolution | `root_dir` empty vs set; folder overrides; recorded-beats-configured | change `root_dir` after adopt: recorded path still used, nothing moved, `describe` reports both |
| State | atomic publish, bounded read, strict schema, lock order | truncated/oversized/unknown-version registry refuses every mutation and is never rebuilt from markers |
| Areas | declaration validation, collision detection, effective-policy recording | two declarations over one folder, a declaration over `scratch/`, a namespace escaping the volume all refuse |
| Markers | strict validation of each field, identity checks | absent, foreign-uid, symlinked, oversized, future-dated, unknown-version and `hosting` markers are never eligible |
| Scratch | conclusion transitions; eligibility table; sweep bounds; grant lookup per unit | unconcluded root, `closure: none`, `kept_reason: exception`, an Effigy marker with a live pid and no delegation, an Oxide `hosting` root, a plain `oxide-validation` directory and an unmarked unit with no legacy claim all survive a sweep; a delegated Effigy marker past 24 h and a claimed unmarked unit past its idle age are the only foreign/unmarked units that do not |
| Caches | the three placement forms; adopt/undo records; link and root identity; setting parse | a setting already pointing at the owned root changes no byte; a non-empty real directory refuses with a plan and nothing renamed or deleted; an empty real directory becomes a link and undo restores it empty; an absent path gets a link through a temporary name; a diverged file after adopt refuses undo; a symlinked original is recorded, not swapped |
| Custody | state machine transitions; `Safe` required by type | unreadable hold, bounded-out scan, live pid with a mismatched start identity, foreign-uid evidence all yield `Unknown` and refuse |
| Deletion | phase machine; Finder metadata classification; ENOTEMPTY retry; marker last | unknown entry stops the delete; symlink inside quarantine stops it; injected ENOTEMPTY three times leaves a pending intent; injected device change stops before the first unlink |
| Contacts | JSON documents match their schemas; unknown major refuses; optional-field addition accepted | a `barge.describe/1` document with a removed field or a changed type is refused by its own reader |
| Foreign state | Oxide `removable.json` v1/v2 and `pools.json` parsers; the pinned Oxide and Effigy scratch marker field sets; delegations and legacy claims | unknown field, unknown version, unparsable and unreadable inputs become `unknown_state` observations; an Effigy marker that omits `task`/`checkout`/`pid`/`pid_start` is valid, and one that adds a field is refused; a unit whose family has no delegation or whose scope has no claim is reported, never deleted |
| Prune/maintain | plan canonicalisation, digest mismatch, ordering | a changed plan digest, a changed root identity, a changed registry revision, a live hold, a revoked claim and a unit that became marked each stop the affected unit only |

The host reaper's current behaviours are the counterfactual baseline for scratch
expiry: units are the children of each project folder and the top-level entries
that are files or have a dot in the name; a unit goes once its newest entry is
fourteen days old and no process has it open; it never follows a symlink, never
deletes outside `scratch/`, skips the `oxide` project and Effigy-marked and
bare eight-character directories, keeps a busy unit, retries `ENOTEMPTY` and
stops at a run budget. Barge must reach the same set of deletable units only
through recorded grants, and must be at least as conservative on every one of
those conditions; the test asserts both the equivalence under grants and the
difference without them (nothing deleted, everything reported).

### One bounded native

One focused native design proof, proposed for the scratch and deletion slices
together, on owned scratch only:

- One owned APFS sparse image (`hdiutil`, `-owners on`), at most 128 MiB,
  attached at a unique `/Volumes/<name>`, backed by a file inside the guard's
  own scratch, never a real drive.
- Under Oxide's qualified shared runner (`bounded-test.py` /
  `scripts/bounded_runner.py` at the pinned supervisor bytes), with the fixed
  wall clock, 512 MiB monitored owned bytes, host reserve watchdog, positive
  closure and ordinary-detach cleanup. Barge writes no second supervisor.
- Cases: allocate two roots concurrently and prove distinct ids; conclude one
  positively and leave one unconcluded; expire with a synthetic old conclusion
  and prove the unconcluded one survives; plant Finder metadata plus an unknown
  entry and prove the delete stops and the root survives; delete a clean root
  whole and prove the marker is removed last; inject `ENOTEMPTY` and prove the
  pending intent; pull the volume (ordinary `hdiutil detach`) mid-delete and
  prove a pending intent with the quarantine leaf kept, then replay after
  re-attach; prove a stale mount point is never written to; plant one Oxide
  marker, one Effigy marker and one unmarked unit, prove all three survive with
  no delegation or claim, then declare the delegation and claim and prove only
  the eligible units go.
- Explicitly not claimed: a real USB/Thunderbolt surprise removal, a real
  drive's flush cost, the internal SSD write saving, and any qualification of
  Oxide's or Effigy's adoption.

### HIGH premise checklist and elevation path

Nine premises, recorded in the proof's own plan and receipt:

1. A private `HOME`, `CARGO_HOME` and state directory, so no real account state
   is read or written.
2. The account home used for state resolution is the fixture's, and the real
   account state is untouched (asserted by a before/after digest of the real
   state directory).
3. Environment isolation: `BARGE_STATE_DIR`, `TMPDIR`/`TMP`/`TEMP` and any
   inherited scratch point inside the guard's owned scratch; no inherited real
   paths.
4. Explicit adoption only: every fixture placement is adopted with an explicit
   command and a recorded plan, never by an implicit default.
5. Owned image with ownership enabled (`-owners on`), ordinary detach, never a
   forced detach of anything but the owned image's own whole-disk device.
6. Every flag and environment hook used is listed with its purpose; no debug-only
   product bypass exists.
7. Temporary storage: everything inside the guard's one owned root; no home, no
   drive root, no fixed `/tmp/<name>`.
8. `mktemp -d` under that root for every fixture directory, never a shared name.
9. The qualified runner bytes and selector are named and hash-pinned; Barge adds
   no second supervisor, session or accounting harness.

Elevated tier: this design deletes storage and writes to removable media, so the
implementation of S4–S7 is Elevated. The elevation path is Oxide's, reused
rather than reinvented: a HIGH premise record, then an uncounted diagnostic
(keep-going, latched first fault, dependent-skip, no promotion of unknown to
pass) before any counted run, then a separate counted author run and a separate
independent reviewer run, each separately activated, each at an exact head. A
missing native receipt is a missing proof, not a pass; this design task produces
none and claims none.

## Open design issues

Named, with the proposed resolution and what the implementation brief must do.
None is a qualification claim, and none contradicts a frozen ruling; O-004 is
the one reading the owner should confirm before implementation starts.

- **O-001 — Custody is cooperative, not kernel-proven.** macOS offers no
  unprivileged "no writer" primitive. The design's verdict is holds plus a
  same-uid bounded descriptor observation, with the residual foreign-uid /
  escaped-process case named. Proposed: keep this, expose
  `describe.custody.capability`, keep the intent plus quarantine recovery, and
  add no force path. The implementation brief must pick and test the concrete
  observation primitive (`proc_pidinfo` fd enumeration versus a `/dev/fd` walk
  for the current process family) and record its refusal path.
- **O-002 — Account state resolution under a non-standard account.** The design
  uses the password-database home, not `$HOME`. Proposed: keep it, and report a
  mismatch. The implementation brief must decide what `barge` does when the
  account home is on an unavailable volume (proposed: refuse every mutating
  command; `describe` still reports).
- **O-003 — Area declarations: file or caller.** A library consumer can pass a
  declaration in memory while a CLI consumer stores one. Proposed: the caller's
  declaration is authoritative for its call; the stored document is the CLI's
  form; both write the effective declaration into the root marker so a later
  reader sees what was applied. The implementation brief must confirm that a
  stored declaration never silently widens a caller's policy.
- **O-004 — Kept containment when `root_dir` is empty.** The rulings say kept
  data is outside the "group folders"; with `root_dir = ""` the mount root is
  the owning folder. Proposed reading: kept must be outside the resolved feature
  folders and every declared area, not outside `root_dir` itself. The
  alternative is that kept data requires a non-empty `root_dir` on its volume,
  which would force a layout change on the Vault drive. Owner confirmation is
  needed before S3, because it decides the containment predicate used by
  adoption and by every later use.
- **O-005 — Legacy scratch layout at the root level.** Effigy's existing runs
  sit at `<scratch>/<8-char>` beside project folders. Proposed: recognise both
  levels, use the marker (not the name) as ownership, and leave Effigy's move
  under `<scratch>/effigy/` to its own adoption. An eight-character directory
  with no marker is covered only by a `top_level_adhoc` claim, never by its
  name; the implementation brief must decide the collision rule for a project
  literally named with eight lowercase characters and no marker: proposed, it is
  a project folder and is never treated as a run root.
- **O-006 — Host job identity and schedule.** Proposed: one host job calling
  `barge maintain` with preview-then-real, replacing only the reaper's scratch
  part; Oxide's prune entry stays until stage 4. The plist content, label,
  install step and the removal of the current reaper are operator-gated host
  changes, not part of any code slice.
- **O-007 — Declared MSRV drift.** The proposed `rust-version = "1.97.1"` is
  justified by Oxide's current pin. If Oxide's toolchain moves before Barge's S1
  lands, S1 re-derives the value from Oxide's manifests rather than inheriting
  this line, and records the choice in the RustQuality activation.
- **O-008 — Space measurement for `space` and later reserve work.** Stage 1
  reports available/total bytes from the filesystem that holds the area, with
  source and time, and claims nothing about exclusive occupancy. Reserve and
  floor are out of scope; the measurement primitive and its accuracy are
  qualified with that later slice, not assumed here.
- **O-009 — Who signs a scratch grant, and how the handover is proven.** Every
  foreign-family or unmarked deletion needs a recorded grant: a delegation from
  the family's owner, or a legacy claim from a human for one scope. Proposed:
  grants are operator declarations (the owner of the data or Tom), written into
  the volume's `scratch_grants`, per volume and per scope, with no default and no
  inference; a delegation is offered by the consumer only after its own
  contract names Barge as the expiring authority. The implementation brief must
  fix the declaration verb and its review, and the handover proof must compare
  one `maintain --plan` run against the reaper's own dry run before the host job
  is changed, so a missing grant is visible as a reported leftover rather than
  a silent leak.
