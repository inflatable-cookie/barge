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
writer custody and safe whole-unit deletion; the scheduled-job interface; the
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
| `unit` | The deletion unit custody and deletion act on: a directory or a file, its parent, the plan's expected identity, its optional marker and family, its recorded grant and the plan digest. |
| `custody` | Writer-custody evidence for a deletion unit, verdicts, deletion authorization. |
| `delete` | Whole-unit deletion, Finder tolerance, quarantine and replay. |
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
  cache-adopt-<kind>.json       barge.cache-adopt/1 (pending cache recovery)
  cache-undo-<kind>.json        barge.cache-undo/1 (pending undo captures)
  holds/<root-id>/<holder>.json barge.hold/1
  intents/<intent-id>.json      barge.intent/2   (barge.intent/1 still read)
  locks/                        registry.lock, volume-<uuid>.lock, root-<id>.lock,
                                unit-<key>.lock
  journal/                      bounded append-only prune/delete observations
```

Every write is atomic: a private `create_new` temporary file in the same
directory, sync, `rename`, then sync the directory. A failed write before the
rename leaves the previous bytes. Reads are bounded (size cap, entry cap,
deadline) and strictly validated; a registry that fails validation makes every
mutating command refuse, and is never rebuilt from markers.

Lock order is fixed and documented so two processes cannot deadlock: registry
lock → volume lock → root lock (a Barge root id, or a unit's own derived key) →
intent write. The registry lock is never held across a deletion or a volume walk;
a long delete holds the volume and unit locks and re-reads the registry state it
needs before publishing its intent. A unit whose name is a Barge root id takes
that root's lock, which is the lock a hold on it takes too; any other unit takes
`unit-<key>.lock`, keyed by the SHA-256 of its volume, parent and name, so a unit
name can never name a path outside `locks/`.

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

The durable record of a root's identity, and the rule for which of these facts
may be compared after a remount, are owned by the [stage 4
design](stage4-design.md#root-identity): the device is a within-run fact there,
and the marker's absolute path becomes a mount-relative one in `barge.root/2`.
This design's marker fields and validation are otherwise unchanged.

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
the same way, and skips symlinks, the `oxide` project and Effigy-marked units.
Since 2026-10-27 it treats every other plain-named directory as a project, without
name-shape or known-project lists (the prior script was saved as
`reap.py.bak-20261027`). Barge requires an explicit grant instead of inferring
ownership from a name. A legacy claim is declared once, per scope, by the owner of
the data:

- The record shape is the volume's `scratch_grants`
  ([registry](#registry-of-volumes)); one claim per scope, with its declared
  idle age, unit shape and excluded families.
- `scope.kind` is `project` (children of `<scratch>/<project>/`) or
  `top_level_adhoc` (entries directly under `<scratch>/` that are files, have a
  dot in their name, or are bare eight-character directories under the O-005
  collision ruling). That last case is a ruled exception to the reaper's set;
  without a claim it is kept. A claim never covers the scratch root as a whole;
  an eight-character project claim and a top-level claim cannot overlap.
- A claimed unit is eligible only when no marker of any recognised family is
  present anywhere in its bounded tree, it is not a symlink at any level, the owning project folder (when the
  scope is a project) has a valid claim, the newest entry in the unit is older
  than the claimed idle age, no hold covers it, custody is `Safe`, and the
  delete is whole-unit with the Finder and `ENOTEMPTY` rules below. A unit that
  becomes marked between plan and effect is kept. A walk that hits its bound is
  unknown and never planned; S5 independently stops before capturing a nested
  recognised marker and keeps the intent pending.
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
Barge wrote (a missing after-file also refuses, even if adopt created it;
divergence preserves the setting, any held-root marker and the placement record);
a `link` removes the link and restores an empty directory where one was
replaced, or removes the link where the original was absent; a `held-root` undo
removes only the marker it wrote, and refuses if the directory's identity
changed. Undo never reverses a relocation; that operation's own undo does.

The implementation uses exclusive capture for settings, published links and
adopted markers, following the [capture rule](#capture-verify-unlink). A bounded
`barge.cache-undo/1` journal (at most two effects and 64 KiB per kind) records the
parent identity, original and private random names, expected file snapshot or
link identity, captured facts and phase before an unlink. Settings and their
held-root marker are both captured and verified before either is removed.
Settings adoption and failed-adoption rollback use this same capture journal
with an explicit operation field. Existing settings are captured and verified
against the pre-adopt snapshot before new bytes are published exclusively; an
absent settings path and the backup path also use exclusive publication. The
private staged file's identity is recorded before publication and copied into
the adoption intent before the capture journal is settled. Recovery requires
that recorded after-identity, rather than accepting newly observed matching
bytes. Rollback captures and verifies its recorded setting/link/marker effects
before any restoration or unlink; a raced replacement preserves pending
intent/capture evidence and is never overwritten or deleted.

Restored settings are staged under the recorded private name plus `.restore`
and published exclusively at the original name; a concurrent replacement is
never overwritten. An unchanged setting is put back without rewriting it.
An empty directory is restored exclusively with its recorded mode.

A mismatch is durably recorded before exclusive put-back. A taken original
name, unreadable capture, failed put-back or interruption keeps the journal,
placement record and any private objects; another adopt or undo refuses while
that journal exists. Recovery requires inspection of those recorded identities;
there is no automatic retry or inference of successful removal from absence.
Only a completed undo removes its journal after removing the placement record.
The residual is the capture rule's same-user discovery of a private random name
within the capture-to-unlink window, rather than a check at the public name.


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

### Deletion units

Custody and deletion act on one **deletion unit** (S5's follow-up, barge#016).
A unit is the thing a plan names and the effect acts on:

- a **parent**: a relative path inside one area folder, `""` for a direct child
  of it, opened descriptor-relative component by component and re-checked to be
  on the unit's device - never re-resolved from a string;
- the **unit name**, one entry inside that parent;
- its **kind**: `directory` or `file`;
- the **expected identity** the plan recorded: device, inode and kind, re-read
  and matched by custody before the first effect;
- an **optional marker**: its filename and family (`barge`, `effigy_scratch`,
  `oxide_scratch`), plus the SHA-256 of the bytes the plan read. A file never
  carries one; a Barge-owned root is exactly a directory unit whose marker is
  `.barge-root.json` of the `barge` family;
- the **grant** the deletion rests on; and
- the **plan digest** that authorised it.

Barge's own roots are therefore the `kind: directory, marker: barge` case of the
same unit, and S4's units - a delegated Effigy or Oxide run, a claimed unmarked
directory, a claimed top-level file - are the other cases. One delete path and
one journal shape serve all of them, so no slice keeps a second delete of its
own: a file unit is captured, verified and unlinked under the same rule a
directory's entries are, and there is no unlink by name anywhere.

Every unit needs a recorded grant, and the grant must be the form its marker
family needs: a **native root** for Barge's own marker, that family's
**delegation** for a foreign marker, and a legacy **claim** for an unmarked
unit. The intent records the grant's kind and reference, and custody re-reads the
volume's `scratch_grants` when it decides, so a grant revoked between the plan
and the effect keeps the unit:

- a foreign family's delegation covers that family's marked units **inside the
  volume's scratch folder** (`folders.scratch`, default `scratch/`) and nothing
  else on the volume; and
- a legacy claim is bound to the volume's scratch folder and to exactly the scope
  it declares, derived from the unit's place rather than from which folder string
  the caller passed: `project:<name>` is a child of `<scratch>/<name>/`, and
  `top_level_adhoc` is an entry directly under `<scratch>/` that is a file,
  whose name contains a dot, or is a bare eight-character run-shaped directory
  under the explicit O-005 collision ruling. The claim's recorded unit shape must match its
  scope (`child` for a project, `entry` for the top level), so a claim never
  authorizes a unit outside its declared scope or a shape it did not declare.

A unit whose grant does not cover its marker family is refused before any effect,
so a missing or mismatched grant can never become a deletion, and a unit that
becomes marked between the plan and the effect is kept.

### Capability limits on macOS and APFS

There is no unprivileged macOS primitive that positively proves "no process has
this subtree open" or "this process has exited and left nothing behind". The
design states the limit and builds only on facts it can establish:

| Fact Barge needs | Available evidence | Limit |
| --- | --- | --- |
| A cooperative holder is done | its hold file was released, or its recorded process is positively gone | a process that never took a hold, or escaped its group, is outside this |
| No same-uid process has an open descriptor under the root | each process's effective uid and start identity from the kernel's own process table (`sysctl`/`KERN_PROC_ALL`), then `proc_pidinfo`/`PROC_PIDLISTFDS` and `PROC_PIDFDVNODEPATHINFO` for this account's processes only, matching vnode paths; a descriptor whose path the kernel hides is checked against the process's own code-signing status (`csops`/`CS_OPS_STATUS`) | a process of another effective uid is classified and reported, never inspected; a process created after the scan is not observed; a hidden vnode descriptor is an OS blind spot, counted and reported rather than refused, only for a process that status verifies as a platform binary with an unchanged start identity, and nothing can read where such a descriptor points |
| The root is the inode that was verified | `fstat` device + inode through the opened descriptor before and after each step | a rename by another user between steps stops the operation |
| The volume is still the registered one | descriptor `fstatfs` + `ATTR_VOL_UUID`, re-verified | a vanished volume fails the call, it does not answer "absent and safe" |

`lsof -nP +D` (the host script's current tool) is not used as proof: it is an
external process, unbounded in time and output, and its silence is not a kernel
guarantee. It may appear only as a diagnostic in a report, never as a custody
verdict.

The verdict is therefore explicitly *cooperative plus same-uid observation*.
Three residuals are named rather than implied: a process of another effective uid,
which the kernel's process table reports and custody counts but never inspects
(Tom, 2026-10-26, ruling bcaa83d8); a vnode descriptor whose path and vnode facts
the platform hides on a process it verifies as a platform binary, which is
counted and reported as the OS blind spot (Tom, 2026-10-26, decision e528a712,
Q-008); and a process that dropped its hold and kept writing. All are named here
and in `describe` (`custody.capability`), and are
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

`Safe` requires all of: the unit's record and grant well formed and the grant
the form its marker family needs; every recorded hold for the unit released or
positively stale (Barge holds are keyed by a Barge root id, so a unit Barge never
created has none to read); the same-uid open-descriptor scan complete within its
bound and empty for this unit; the unit's device, inode and kind equal to the
observation the caller was given; its marker, when it records one, still holding
exactly the bytes the caller observed at its family's own filename, and a unit
that records no marker carrying none of the three recognised families; the
volume verified; and no pending intent naming another unit. The scan classifies every candidate from the kernel's own process table by
its effective uid **before** it inspects any descriptor: a process of another
effective uid is counted and reported in the evidence as not observed, which is
the named blind spot and does not block `Safe`; only this account's processes
have their descriptors read. The scan also excludes the calling `barge` process's
own descriptors — Barge holds descriptors inside the root for its own walk and
its own checks, and those are not another writer — by its pid and the start
identity read once and verified; no other process is excluded, so another `barge`
process, its child or its parent holds a descriptor like anyone else. A
descriptor listing the kernel answers `EPERM` for is re-observed by kernel
identity inside a bounded settle window before it is decided, and settles only on
a positive fact — a later listing, a gone process, a changed start identity or a
changed effective uid — never on elapsed time (barge#012, below). A vnode
descriptor whose path and vnode facts both answer `EPERM` is an **OS blind spot**
when the process's own code-signing status reports the platform binary flag and
its start identity is unchanged before and after that read: it is counted and
reported in the evidence, and it does not block `Safe` (Tom, 2026-10-26, decision
e528a712, barge#013). Every readable descriptor of that same process is still
evaluated, so one that lies inside the root still makes the verdict `Unsafe`, and
every other hidden descriptor — a process that is not a verified platform binary,
a status read that fails, an identity that moves around it — stays unreadable.
Any unreadable
hold, an unreadable effective uid, a same-uid descriptor that stays unreadable
after its retries or that no verified platform binary explains, a scan that hits
its entry or time bound, a foreign-uid holder, or a `lsof`-style diagnostic that
errors makes the verdict `Unknown`,
which is a refusal. Deletion and swap take a `Safe` value by type, so a caller
cannot pass a boolean.

Custody is taken before a link swap, before a unit's quarantine rename, and
before a link removal, and it is re-checked after the intent publish and before
the first destructive step. It is not a lease on the data: a consumer that is
mid-write without a hold is a consumer Barge cannot see, which is exactly the
limit above and the reason adoption is explicit and holds are the documented
protocol.

### Whole-unit deletion, Finder metadata and quarantine

Deletion is whole-unit only, of units the plan named with a matching grant,
descriptor-relative, and bounded:

1. Verify the unit and its marker (or, for a unit that records none, that it
   stayed unmarked); take custody; write `barge.intent/2` (`intent` phase)
   recording the unit's kind, name and parent, its marker filename and family,
   the planned private capture name, the expected device and inode, the grant
   reference, the volume UUID, the plan digest and the time. The private name is
   recorded **before** the capture, and the intent is durable before any rename.
2. Rename the unit inside its parent to `.barge-prune-<intent-id>` with the
   platform's exclusive rename (`renameatx_np` with `RENAME_EXCL` on macOS),
   then record the `renamed` phase. The rename is atomic and never replaces an
   existing name: a taken target refuses and the intent stays pending. A
   cross-device or changed-kind, changed-inode result stops before deletion.
3. For a directory, walk and delete through descriptors opened from the parent's
   descriptor: `openat` with `O_NOFOLLOW` per directory, and every entry removed
   through **capture, verify, record, unlink** (below): the capture and its
   verdict are durable before the unlink, never following a symlink, with a depth
   bound (12) and per-directory name bound (200 000) and a per-unit deadline
   (120 s). A file unit has no contents to walk: it goes straight to its own
   capture and unlink under the same rule.
4. Finder tolerance is exactly Oxide's two forms and nothing broader:
   `.DS_Store`, or `._` followed by a nonempty name. A Finder-named **regular
   file** is verified by class whether the classification recorded it or Finder
   rebuilt it between the observation and the capture - a regular file, no-follow,
   owned by this user, on the directory's device, inside the size bound - because
   Finder's store has no stable inode to match. A directory, a link or anything
   else that happens to carry a Finder name is not Finder metadata.
   Every other unexpected entry stops the deletion and keeps the unit. Finder
   metadata is deleted as part of the whole unit; it never establishes
   ownership, never makes a unit reusable and never permits a narrower delete.
5. `ENOTEMPTY`/`EEXIST` from `rmdir`/`unlink` retries the enclosing directory at
   most three times with a one-second pause, because Finder can recreate
   `.DS_Store` mid-delete. Each retry re-deletes only what the walk already
   recognises, through the descriptor of the directory that was verified as the
   captured one. After the third failure the intent stays pending, the
   quarantine leaf is kept, and the command exits `3`. It does not widen what it
   deletes.
6. The unit's marker, when it records one, is unlinked last, through the same
   capture and verify as every other entry; a unit that records no marker records
   no marker step and goes from its contents to its leaf. Only after that may the
   captured leaf be removed, and it is **captured and verified again first**: the
   visible `.barge-prune-<intent-id>` name is exclusively renamed to a
   `.barge-del-*` private name, the captured object's kind, device and inode are
   checked against the intent's, and only that captured object is removed - a
   directory after its contents, a file by its own unlink. A swap in the unit's
   parent therefore puts its own object back and stops the deletion instead of
   being removed by name. Then the `removed` phase is recorded, and the `deleted`
   phase settles the intent.
7. On replay, a pending intent is resolved only from its recorded identity, and
   replay's windows are conservative. It first restores every recorded mode, then:
   - it resumes the deletion only when the captured leaf is still at its visible
     `.barge-prune-<intent-id>` name with the intent's kind, device and inode and,
     when the unit records one, the marker digest matches its record, and the phase
     is `renamed`;
   - at `marker_last` it resumes the leaf's own capture and removal only when the
     leaf is at that visible name and the unit's marker is positively **absent**
     (which a unit that records no marker satisfies by construction), which is the
     state that phase records; the leaf is then captured, verified and removed
     under the capture rule, and `removed` is recorded;
   - within a resumed walk it re-verifies each **recorded, verified** capture -
     device, inode and kind, all read through the descriptor it verified - and then
     removes it, which is how a nested captured file or directory a stop left
     behind is finished; the record, not the name, is what authorises that removal;
   - the quarantine leaf is the one place that is not resumed from a private name:
     when the leaf itself is held under a recorded `.barge-del-*` name, its
     recorded mode is restored and the intent stays pending, because only the
     durable `removed` phase proves Barge removed the leaf;
   - it completes only when the leaf is positively absent **and the durable
     `removed` phase was recorded** - after Barge's own final unlink - which is the
     only proof of completion. Absence alone never resolves an intent: an absent
     leaf without `removed` is `Unknown`, and so is a recorded nested capture whose
     private name is gone.
   A recorded mismatch stops a replay before it walks: the object Barge never
   observed is still in the leaf, under its private name or back under the
   original one, and the replay names it and unlinks nothing. A private
   `.barge-del-*` name with no record - unrecorded, collided or otherwise
   unverifiable - is never unlinked either way: nothing recorded the identity of the
   object under it for a walk to check, so it stays pending. A symlink inside a
   quarantine stops the deletion and leaves the intent pending; it is never
   followed and never removed.

#### Capture, verify, unlink

A check followed by an unlink by name is not safe: a same-uid racer can replace
one entry after its check, and every later check passes, so a deletion by name can
remove objects it never verified. Every entry is therefore removed through one
rule, and the marker with them:

1. **Capture.** Exclusively rename the name to `.barge-del-<random 128-bit
   nonce>` in the same directory. The rename is atomic and never replaces an
   existing name; a taken private name is retried with fresh random bytes.
2. **Verify the captured object.** `fstatat(private, AT_SYMLINK_NOFOLLOW)` —
   the deleted entry's own facts, read under its private name — must report the
   device, inode and kind the walk observed before the capture, and this
   account's owner. A Finder-named regular file is verified by class instead, a
   regular file of this user on this device within the size bound, whether the
   classification recorded it or Finder rebuilt it between the observation and the
   capture: Finder metadata has no stable inode to match, which is exactly why it
   is tolerated by class. The parent binding is re-read through the
   descriptor the capture went through, so a volume that changed under it stops
   the deletion before any unlink.
3. **Unlink only on a match.** A file is unlinked under its private name. A
   directory is opened through the private name (no-follow), its opened identity
   is compared with the verified one, its contents are deleted through that
   descriptor, the private name is re-verified as the same inode, and only then
   is it removed with `AT_REMOVEDIR`, where `ENOTEMPTY` stops.
4. **On a mismatch, record first, then never unlink.** The mismatch is published
   into the intent **before** anything puts the object back: the private name, the
   name it came from, the identity the walk observed there, the identity the
   captured object actually had (absent when it could not be read), why the
   capture did not verify, and a put-back answer of `pending`. Only then is the
   object exclusively renamed back to its original name, and the answer
   (`renamed`, `taken` or `failed`) is recorded; a record that still says
   `pending` is honest about a process that stopped, and blocks a replay just the
   same. A successful put-back returns an object Barge never observed to an
   ordinary name, where a later walk would find it and unlink it, so the order is
   what makes the protection real: a crash between the record and the put-back, or
   between the put-back and its answer, leaves the intent pending with the
   evidence. If the record cannot be written at all, the object is **not** put
   back: it stays under its private name, both names are reported and the deletion
   stops with the intent pending (exit `3`). Where the put-back is attempted and
   the original name is taken, or it fails, the object stays under its private
   name too. A private name is only ever unlinked against the durable record of
   *this* operation's capture and verification: a run or a replay re-reads the
   recorded device, inode and kind and unlinks only when the object still matches,
   and it deletes a recorded capture through the descriptor it verified, never by
   trusting the name. An unrecorded, collided or unverifiable private name is never
   unlinked and keeps the intent pending, because nothing proves which object it
   holds, and no replay resumes an intent that records a mismatch.

The guarantee this states:

> Deletion removes only objects it verified after atomically capturing them under
> a private random name; it never follows a link, never recurses into an unverified
> directory, never unlinks a captured object that fails verification, and never
> replaces an existing name. The residual is a same-user process that discovers the
> private 128-bit name inside Barge's own quarantine within the capture-to-unlink
> window.

#### Durable custody of captured objects

The capture rule above is only as strong as what survives a stop, so three durable
facts carry it (Oxide Supervisor's #155 custody ruling, applied here by the amended
barge#014 brief). An absent name is never proof that Barge deleted its object.

1. **Positive completion.** After Barge's own final `unlinkat` of a verified
   captured private name - the quarantine leaf's removal - a `removed` phase is
   published durably before the intent settles. An `ENOENT`/vanished answer during
   a live run stops the deletion and reports that another process may have moved
   the captured object. A replay settles an intent whose leaf is absent only when
   `removed` is already recorded; both names absent without it is `Unknown` and
   stays pending. The same holds one level down: **a capture the intent records
   whose private name is gone is `Unknown` too** - the record proves the capture
   happened, not that Barge unlinked it - so the replay reports it and unlinks
   nothing, rather than walking past it and completing. Settlement beyond that is
   an explicit operator route that
   re-verifies both names absent; this stage adds no new verb. **The crash window
   between the successful final unlink and the durable `removed` phase is
   therefore pending, not complete**: the intent names what Barge did, and an
   operator decides.
2. **Recorded and verified captures only.** A `.barge-del-*` name is unlinked only
   while the intent durably records that capture and its post-capture
   verification - device, inode, kind and verdict (`observed`, or the
   Finder-metadata class verdict) - and a replay re-verifies the same identity
   before it deletes anything. The record is a **snapshot published before each
   unlink and each chmod**, one atomic publish per entry, so the deletion's
   practical size inside its 120 s deadline is smaller than the name bound alone
   suggests; a root that reaches the deadline stops pending. A capture whose unlink
   has succeeded is left out of the next snapshot, and the marker's boundary is
   refreshed immediately after its own unlink, so the `marker_last` phase resumes;
   after an ordinary entry's unlink a stop can still leave that finished capture in
   the last published snapshot, and a replay then reads it as a missing capture,
   reports it and stays pending rather than guessing. That is the conservative
   window the phase machine closes with `removed`. A snapshot published while a
   replay finishes a leftover also **keeps every capture the intent already
   records that it has not reached yet**: those objects are still under their
   private names, and dropping their record would leave them unverifiable if the
   replay then stopped. The chain therefore starts from the intent's recorded
   captures, in their outermost-first order, and a capture enters or leaves it
   only around the unlink it authorises. Every capture whose verification
   failed is durably appended to `foreign_captures` - private name, original name,
   observed device/inode/type, the identity actually found - **before** its
   put-back is attempted. A replay may **re-verify and remove a recorded, verified
   capture** - the same device, inode and kind, through the descriptor it
   verified - and that is how it finishes a nested capture a stop left behind. A
   listed name recorded in `foreign_captures`, an unrecorded name, a name whose
   identity no longer matches, and an unverifiable name are never unlinked, stay
   reported, and keep the intent pending. The
   quarantine leaf is captured and verified under the same rule before its final
   removal, so the visible area-level name is never removed by name. A root abort
   never relabels a nested foreign capture as an ordinary entry: it stays under its
   private name, inside the captured parent's private name.
3. **Mode custody.** A captured directory this account cannot write or search
   cannot have its contents removed, so Barge adds the owner write and search bits
   - and only after the object's original **full `st_mode`**, the file-type bits
   included, is recorded durably, in the same capture record. A scope guard restores the recorded mode on
   every return, `?` error and panic that aborts or puts back, so a changed mode is
   never left behind without the intent recording the original. A restore that
   fails leaves the intent pending with the mode recorded. **A replay restores the
   recorded modes first, before any other deletion or reconciliation action** -
   before it even reads the marker or decides the phase - and refuses to go on when
   a recorded name no longer holds the recorded object, or is gone.

Deletion never touches a unit the plan did not name with a grant that covers
it: a foreign marker family without that family's recorded delegation, a
marker-less unit without a recorded legacy claim, an `adopted_existing` root
without an explicit plan entry, a unit on another device, or a path outside the
area. A whole-unit delete of an adopted existing cache root deletes the
consumer's data, so it is allowed only through the area's declared prune policy
or an explicit operator command that names the unit.

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
| Finder recreates `.DS_Store` while an entry is captured | the captured name is not the observed inode, or the file is not the allowed class | capture, verify by class, unlink; anything that is not a regular file of this user on this device inside the size bound stops the deletion |
| An entry is replaced between its check and its capture | the captured object's device, inode or kind differs from the observation | never unlink; record the mismatch before any put-back, put the object back under its original name, and stop with the intent pending, exit `3`; no replay resumes that intent |
| The mismatch record cannot be written | the intent publish fails | never unlink and never put back: the object stays under its private name, both names are reported, and the intent stays pending |
| The process stops between the mismatch record and its put-back | the record says `pending` and the object is under its private name or at its original one | the intent stays pending; no replay resumes it and nothing is unlinked |
| The captured object's own facts cannot be read | the parent binding fails, or the private name is already gone | the mismatch is recorded without a captured identity, and the intent stays pending |
| The captured object vanishes mid-run | the unlink or `rmdir` answers `ENOENT` | stop, report that another process may have moved it, keep the intent pending |
| A crash lands between the successful final unlink and the `removed` phase | the leaf is absent but `removed` is not recorded | `Unknown`: the intent stays pending, because an absence is not a completion |
| A `.barge-del-*` name is not recorded, or no longer matches its record | the intent's `captures` chain | never unlink it; report it and keep the intent pending |
| A recorded capture's private name is gone at replay | the intent records it, and `removed` is not recorded | `Unknown`: report it and keep the intent pending; an absence is never proof Barge unlinked it |
| The marker cannot be read at replay | the leaf's marker does not validate | the recorded modes were already restored first, and the intent stays pending |
| A captured directory is read-only | its owner write or search bits are missing | record the original mode, apply `mode \| 0o300`, and restore the recorded mode on every abort, `?` error and panic |
| The recorded mode cannot be restored | the mode change back fails | keep the intent pending with the mode recorded, and restore it first on replay |
| An entry's original name is taken while it is captured | the put-back rename answers `EEXIST` | keep the object under its private name, report both names, stop with the intent pending, exit `3` |
| A rename would replace an existing name | the exclusive rename answers `EEXIST` | nothing is renamed; a capture retries with a fresh private name, the quarantine rename stops with the intent pending |
| A `.barge-del-*` leftover is left by a stopped deletion | the private name is recognised under the intent, with the capture it recorded | unlink it only when the intent records that capture and the object still matches the recorded device, inode and kind, through the descriptor it verified; an unrecorded, foreign or unverifiable leftover is never unlinked, stays reported, and keeps the intent pending |
| Finder rebuilds a `.DS_Store` between its check and its capture | the captured object's inode differs, or the file is not the allowed class | verify by class: a regular file of this user on this device inside the size bound is removed; anything else is a mismatch and stops the deletion |
| Finder writes a non-metadata entry | refusal in the walk | stop, keep the root, report the escaped name; never widen the delete |
| Two allocators race | exclusive `mkdir` with the fresh id | the loser gets `EEXIST` and retries with a new id; no shared root, no adoption of the other's marker |
| Two expiries race | root lock plus intent publish | the second sees the pending intent and skips that root; a stale reader's identity check fails and it changes nothing |
| Holder alive but unreadable | process start identity unavailable | custody `Unknown`; refuse; keep |
| Holder pid reused by another process | pid + start identity + boot identity mismatch | treat the hold as stale only when all three positively disagree with the record; otherwise `Unknown` |
| Crash between marker and payload | marker present, root empty | root is a valid empty run root; expiry may take it under the ordinary policy; nothing is inferred as concluded |
| Crash between intent and rename | intent `intent`, root still in place | replay rolls back the intent and re-evaluates the root; not a completion |
| Crash between rename and `renamed` phase | quarantine leaf exists with recorded identity | replay resumes deletion from the recorded identity |
| Crash after `marker_last` before `removed` | marker absent, quarantine leaf may be present | replay removes a matching captured leaf; an absent leaf without durable `removed` stays pending |
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
  own brief, review and (where they touch a real drive) operator gate. The
  [stage 4 design](stage4-design.md) carries owned-root identity, the absence
  proof and the one gate that authorizes pruning a whole owned root; Oxide's
  roots stay Oxide's to prune until its own adoption slice.

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
| S5 | Custody state machine for a deletion unit, same-uid descriptor observation, intent journal, quarantine, whole-unit delete with Finder and ENOTEMPTY rules, replay and settlement | S3 |
| S6 | Cache kinds, the three placement forms (`setting`, `held-root`, `link`), adopt/undo with byte-level and link/root identity proofs, refusal of a non-empty real directory, in-place adoption of today's hand-made placements | S3, S5 |
| S7 | `barge maintain` preview/real, exit codes, `describe` completion, host-job handoff contract (the plist change itself stays an operator gate) | S4, S5, S6 |
| S8 | Consumer adoption: Oxide declares its areas and switches scratch, caches, registration, then owned-root mechanics; Effigy moves to Barge scratch. Separate briefs, Oxide's own Queue project, explicit operator gates | S1–S7, separately qualified |

Dependencies that are not slices: a real-drive qualification (later, operator
gated), Linux (later, additive behind `os`), reserve/floor pruning (the
[stage 3 space policy](stage3-space-policy.md) owns that design), owned-root
identity across remounts and the absence proof a release prune needs (the
[stage 4 design](stage4-design.md)), and the retirement of any foreign format
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
  the bounded same-uid descriptor observation behind `os`, the `barge.intent/2`
  journal, the quarantine rename, whole-unit deletion with the Finder and
  `ENOTEMPTY` rules, and replay and settlement. Its follow-up (barge#016)
  generalised custody and the journal from a Barge-marked root to a deletion
  unit - a directory or a file, an optional marker of a recorded family, and the
  grant that authorises it - so S4's sweep deletes only through this one path and
  keeps none of its own; `barge.intent/1` stays readable and maps to a
  Barge-marked directory unit. It is a library capability only:
  no CLI verb deletes, nothing is scheduled, no drive or real account state is
  touched by its tests, and its native proof belongs to S4. Its unit and
  counterfactual tests run on fixture directories with injected OS facts.

Choices S5 made where this design left the mechanic to the slice:

- **The observation primitive** (amended under Tom's ruling bcaa83d8).
  `ProcessObserver::scan_open_descriptors` reads the kernel's own process table
  with `sysctl`/`KERN_PROC_ALL` — every process's pid, effective uid, start
  identity and command name — and **classifies each candidate before it inspects
  any descriptor**. A process whose effective uid is not this account's is
  counted and reported in the evidence as not observed: that is the named blind
  spot, it does not block `Safe`, and nothing about the process is read. Only
  this account's processes are then enumerated with `PROC_PIDLISTFDS`, and each
  vnode descriptor's `PROC_PIDFDVNODEPATHINFO` path is matched against the root
  by path components, under both the spelling the walk reached and the one the
  filesystem resolves. A path the kernel hides as `EPERM` is checked against the
  same descriptor's `PROC_PIDFDVNODEINFO` facts, and only when both are hidden
  is it opaque; how far that goes is the `CS_OPS_STATUS` rule below. This is what
  removed the `EPERM` wall: identifying a
  process through its own `PROC_PIDTBSDINFO` facts is refused for another
  account's processes, and reading the pid list first could not tell those apart
  from a process whose facts were genuinely unreadable. `proc_pidfdinfo` is
  declared in the macOS module because `libc` does not declare it; every `unsafe`
  stays in that one seam. `libc` declares neither `kinfo_proc` nor its offsets,
  so the module reads the four fields it needs at the offsets `<sys/sysctl.h>`
  gives them and checks itself: the entry for this very pid must report this
  account's uid and the start identity `PROC_PIDTBSDINFO` independently reports,
  or the table is refused as unknown. A table that fills its bound is `Bounded`;
  an unreadable table, an unreadable effective uid, an unreadable same-uid
  descriptor, or an entry or time bound that is reached makes the verdict
  `Unknown`. The concrete bounds are 4096 processes, 65 536 descriptors and 10 s
  per scan; a root carries at most its 64 holds. `lsof` is never read.
- **The calling process excludes itself, and nothing else** (corrected here after
  S6's spent diagnostic). Custody opens the area folder and the root for its own
  walk and its own checks, so the first implementation's scan always found its
  own handles inside the root and refused every CLI-driven delete. The request
  now names the calling process by pid and by the start identity custody reads
  once with `ProcessObserver::process_start`, and the scan compares that identity
  with the entry the kernel's table reports: the process is skipped only while
  they agree, because a reused pid is a different process. A mismatched or
  missing identity, or a calling process the table never showed, is `Unknown`,
  never a skip; a skip that never happened proves nothing about what it left out.
  Everything else is unchanged: a descriptor held by another same-uid process,
  another `barge` process, its child or its parent is still `Unsafe`.
- **An intermittent `EPERM` is settled by re-observation, never by elapsed time**
  (corrected here under barge#012, found by barge#007's owner-context diagnostic
  at `9ad4ce04`). That diagnostic got `EPERM` from
  `proc_pidinfo(PROC_PIDLISTFDS)` for a same-euid `/usr/libexec/dmd` (pid 69326)
  in two cases, after a full same-euid census had passed, while Python and a
  locally built ad-hoc-signed C probe read the same pid afterwards. The cause is
  **not** established: that instance was already more than two minutes old
  through both refusals, so neither a process-start or exit race nor a signing
  restriction is proved, and the rule below claims neither. A single `EPERM` is
  a question, not an answer. When listing one same-uid process's descriptors
  answers `EPERM`, the scan re-reads that pid's kernel identity through
  `KERN_PROC_PID` — pid, start time and effective uid, which the kernel's table
  reports for every process without inspecting one — and lists again after a
  short backoff: three retries of 25, 75 and 100 ms, a settle window of at most
  250 ms per process, always inside the scan's own 10 s bound. Both deadlines are
  projected before each backoff and **checked again after it**, so a late wake
  never buys one more listing past either. They are checked a third time when a
  terminal answer arrives, because the read that produced it may itself have been
  slow: an answer read after the scan's deadline makes the scan bounded, and one
  read after the process's settle window closes is a refusal, since the bounded
  re-observation did not finish inside its bound. It settles only on a positive
  fact: a later listing succeeds; the kernel reports no such pid, or a retried
  listing answers `ESRCH`, and a gone process holds no descriptors; the start
  identity changed, so a different process now has the pid and is re-evaluated
  from scratch under the same rule and inside the same budget; or the effective
  uid changed, so the pid now names another account's process and is classified
  as the named blind spot above instead of refused. The same identity still
  answering `EPERM` after its retries is `Unknown` and refuses. An unreadable
  listing for any other reason refuses at once, and when it answers a retry its
  errno joins the sequence beside the `EPERM`s: the recorded sequence is every
  failed listing, `EPERM` or the terminal fact that ended the re-observation.
  Nothing here infers safety from elapsed time: a process the scan cannot read
  still refuses.
- **A verified platform binary's hidden descriptors are an OS blind spot**
  (corrected here under barge#013, Tom's decision e528a712, answering Q-008).
  Some of this account's own Apple daemons — `/usr/libexec/dmd` is the measured
  example (pid 69326 on 2026-10-26, 6 of 9 descriptors) — answer `EPERM` to both
  `PROC_PIDFDVNODEPATHINFO` and `PROC_PIDFDVNODEINFO` for protected vnode
  descriptors, persistently and from every context, while
  `proc_pidinfo(PROC_PIDLISTFDS)` succeeds for the process. That is not an
  intermittent answer, so the re-observation above cannot settle it, and refusing
  it made a full-scan `Safe` verdict unreachable on a real session. The rule is
  narrow and proved: when a vnode descriptor of a same-uid process answers
  `EPERM` to **both** flavors, the scan reads the process's code-signing status
  through `csops(pid, CS_OPS_STATUS)` behind `ProcessObserver`, and the
  descriptors are an OS blind spot only when `CS_PLATFORM_BINARY` is set **and**
  the kernel's start identity for the pid is the classified one before and after
  that read. The scan then records the process (pid, start identity, command,
  count of hidden descriptors) and does not block; every readable descriptor of
  that same process is still matched against the root, so one inside it still
  makes the verdict `Unsafe`. The record names every such process, bounded at 32
  of them: meeting more is `Unknown`, not a shorter list. Everything else is unchanged and refuses with
  `Unknown`: a process that is not a platform binary, a `csops` failure or an
  unreadable status, a start identity that changes around the read, and any other
  unreadable fact. There are no path or name lists, a foreign effective uid keeps
  ruling bcaa83d8's treatment, and the bounded re-observation of barge#012 still
  applies first. `csops` is declared in the macOS module beside
  `proc_pidfdinfo`, because `libc` declares neither, and it is readable for any
  pid, which is what lets it speak about a process whose own descriptors it
  hides.
- **What the evidence says.** The evidence records `self_excluded: <pid>` (the one
  process the scan excluded), `foreign_uid_processes` (how many processes of
  another effective uid it classified and did not inspect) and `retry`: how many
  same-uid processes' listings answered `EPERM` and were re-observed
  (`retried_unreadable`), and at most eight bounded records of them, each with
  the pid, command, kernel start identity, the errno sequence of every failed
  listing including the terminal one, identity changes and resolution, so the
  next native diagnostic can tell an intermittent answer from a persistent one.
  It also records `os_blind_spot`: how many platform-verified processes had
  hidden vnode descriptors, how many descriptors were hidden in total, and the
  record of **every** one of them with its pid, kernel start identity, command
  and hidden-descriptor count, which is the OS blind spot above. That record is
  bounded at 32 processes but never truncated: a scan that meets more refuses,
  because a `Safe` verdict must not count a blind-spot process it cannot name. **Every**
  verdict carries that retry record, not only `Safe`: an
  `Unsafe` or `Unknown` verdict keeps it, and the `custody_not_proven` failure
  puts it in its `detail` (`retried_unreadable` and `retried`), because a refusal
  is exactly where a diagnostic needs it. A pending deletion's report keeps it
  too (`custody_retry`) when custody refuses after the intent was written. The
  scan's own answers carry it as well: `Complete`, `Bounded` and `Unreadable`. `describe.custody` names all of it: the
  exclusion, the foreign-uid blind spot that is reported rather than observed, the
  OS blind spot of a verified platform binary's hidden descriptors, and
  the retry rule with its positive facts and its two rechecked deadlines. A
  same-uid process the scan cannot read is an `Unreadable` answer rather than a
  returned error, so the record reaches the refusal; a returned error is only a
  fact the scan cannot begin with (this account's uid, or the kernel's process
  table). The tests run the real macOS observer on a fixture root under scratch:
  the caller's own open handles stay open and the verdict is `safe`; a same-uid
  child holding the payload open is `unsafe`; real `/usr/bin/login` identities,
  read-only from the kernel's table, are counted as foreign and do not block; an
  unreadable uid, a hidden descriptor of a same-uid process that is not a
  verified platform binary, a mismatched
  start identity and a forced bound are `unknown`; and the counterfactuals refuse
  — the exclusion removed refuses the caller's own descriptor, identification
  through the process's own facts refuses the real login process, and a stated
  `EPERM` refuses at once with the retry schedule removed. The retry rule's own
  cases are stated with an injected answer sequence: `EPERM` then success is
  `safe` with the retried pid recorded, `EPERM` then `ESRCH` is resolved as gone
  with both errnos in the sequence, `EPERM` then no such pid is resolved as gone,
  `EPERM` with a changed start identity is re-evaluated and its descriptors read,
  `EPERM` with a changed effective uid is foreign, `EPERM` through all three
  retries stays `unknown` with the bounded errno sequence kept on the refusal, and
  a scan time bound shorter than the first backoff answers `bounded` with the
  record kept, and `EPERM` then another unreadable errno records both in the
  sequence. The platform exemption's cases are stated the same way, with a
  hidden-descriptor count and a code-signing answer injected because this account
  owns no Apple-signed binary to hide them: a verified platform binary's hidden
  descriptors are `safe` with the blind spot recorded, the same fixture without
  the platform flag refuses, a `csops` failure refuses, a start identity that
  changes around the read refuses, a platform binary whose readable descriptor
  lies inside the root is `unsafe`, a scan that meets more than the 32-process
  record bound refuses rather than drop one, and a real-observer test reads the kernel's
  own table to verify one live same-uid platform binary and to show that this
  locally built test binary is not one. The deadline checks are also unit-tested on their own: a projection
  that would end past either bound is not taken, a delayed wake past either bound
  stops the retry instead of listing again, a terminal answer inside both bounds
  is accepted, and one read after the settle window or after the scan deadline is
  not.
- **The prediction this correction replaces.** Before barge#013 the design
  expected a handful of this account's own system daemons to refuse
  `PROC_PIDFDVNODEPATHINFO` for some of their descriptors (measured on macOS
  26.6.2: 8 of 1004), which made a whole-system scan `Unknown` and a full-scan
  `Safe` verdict unreachable. That is now the OS blind spot above, decided by
  Tom's ruling e528a712 rather than refused; Q-008 keeps the measurements.
- **Hold decisions.** A hold is stale only when its process is positively gone or
  another boot wrote it. A live pid whose start identity differs is `Unknown`,
  not stale: the failure matrix's reused-pid row treats staleness as proved only
  when every recorded identity positively disagrees. S3 decided that case stale;
  S5 corrects it, because `Unknown` blocks deletion and a wrong stale would let a
  reused pid's hold be ignored.
- **The intent record** (generalised under barge#016). `barge.intent/2` sits at
  `state/intents/<intent-id>.json`, at most 4 KiB, with exactly `schema`,
  `intent_id`, `volume_uuid`, `mount_hint`, `root_dir`, `folder`, a `unit`
  object, `plan_digest`, `phase`, `generation`, `recorded_ms`, `phase_ms`, an
  optional `captures` list and an optional `foreign_captures` list. The `unit`
  object carries exactly the unit's `parent`, `name`, `kind` (`directory` or
  `file`), its expected `device` and `inode`, an optional `marker` (filename,
  family and digest), the planned private `capture_name` and the `grant`
  (kind and reference). The capture name is the
  `.barge-prune-<intent-id>` leaf in the unit's parent, recorded **before** the
  capture and re-derived and checked on every read, never trusted as free text.
  The phases are `intent`, `renamed`, `marker_last`, `removed` and `deleted`.
  The major moved because the old field set could describe only a Barge-marked
  directory: `barge.intent/1` stays readable, is read as its own field set and
  maps to a directory unit directly inside its area folder, marked
  `.barge-root.json` of the `barge` family, under the native-root grant - the
  only shape v1 could describe - and the next publish writes v2. A reader
  refuses an unknown major, and refuses a document whose fields do not match its
  major (both shapes deny unknown fields), so a v1 record cannot be read as v2
  or the reverse. The document is parsed twice - once for its major, and then
  **from its own bytes** into that major's struct - so a repeated JSON member
  (`schema`, `foreign_captures`, `unit`, or any nested field) is refused as
  unknown state rather than silently resolved to its last value; a reader that
  reused the first parse's value would let a duplicate `foreign_captures` erase
  the recorded mismatch evidence a replay depends on. `captures` is the
  in-flight chain of recorded captures - at most twenty, one per depth level plus
  headroom, and never more than the record's 4 KiB cap allows - and
  each entry carries the private name, the original name, the device, inode and
  kind of the verified object, its verdict (`observed` or `finder`) and, when
  Barge changed it, the original full `st_mode`; a restore applies its permission
  bits, and the recorded type bits must agree with the recorded kind. It is a
  snapshot: a later publish
  replaces it, so it never grows with the tree, and it is published before each
  unlink. `foreign_captures` is empty in the common case and absent from the
  document then: at most four failed captures, each with the private name, the
  original name, the observed identity, the captured object's own identity
  (absent when it could not be read), the put-back answer (`pending` before it is
  attempted, then `renamed`, `taken` or `failed`) and a bounded reason. Each of
  those is published before its put-back, and a later record of the same capture
  replaces the earlier one rather than duplicating it. A deletion stops at the
  first failed capture, so a run adds at most one, and a record that would exceed
  either bound is refused rather than quietly dropped. A replay that reads any
  `foreign_captures` entry refuses to resume the intent. Adding the optional
  fields and the `removed` phase are additions inside the major version, so a
  document written before them still reads. The
  quarantine leaf's name is `.barge-prune-<intent-id>` and is re-derived and
  checked, never trusted as free text. A phase advances only forward and each
  publish is the atomic state write, so a stale writer cannot rewind an intent it
  raced. A leftover publish keeps the phase and still increases the generation,
  and the deletion stops on it, so no phase advances from a record it has not
  read back. The journal read is bounded at 4096 records; a stray name, an
  unreadable record or a truncated journal is `unknown_state`, never a skipped
  intent.
- **Deletion order.** The intent is durable, custody is re-decided, the unit is
  renamed inside its parent, `renamed` is durable, the volume and the leaf's
  identity are re-verified, a directory's tree is classified once without
  mutation, then deleted, then the unit's marker - when it records one - is
  unlinked last, `marker_last` is durable, the leaf is captured and verified
  again under a private name and that capture is durable, the captured leaf is
  removed (a file by its own unlink), `removed` is durable, and `deleted` settles
  the intent. A file unit has no contents to classify: its own verified identity
  is the whole unit. The two-pass walk is what makes "an
  unknown entry stops the delete and keeps the unit" literal: nothing is
  unlinked until every entry is classified, and the delete pass removes only what
  was classified plus Finder metadata that appeared again. Finder metadata is the
  two named forms *as a regular file* of this user on this device: a directory, a
  link or anything else that happens to be named `.DS_Store` or `._name` is not
  Finder metadata and stops the delete. Anything else that appears mid-delete
  stops it too, so the delete never widens.
- **Capture, verify, unlink** (corrected here under barge#014, the same race class
  oxide#155's exact-head review found). A check followed by an unlink by name is
  not safe: a same-uid racer can replace each successive entry after its check,
  and every later check passes, so a deletion by name can remove objects it never
  verified. Every entry — and the marker last — is now exclusively renamed to
  `.barge-del-<random 128-bit nonce>` in its own directory, verified under that
  private name against the device, inode and kind the walk observed, and unlinked
  only on a match; a Finder file that appeared after the classification is
  verified by class instead. A mismatch never unlinks: the object goes back under
  its original name, or, when that name is taken, stays under the private name
  with both names reported, and the deletion stops with the intent pending.
  Directories are recursed through the descriptor of the verified inode,
  re-verified before `AT_REMOVEDIR`, and `ENOTEMPTY` stops. The guarantee is
  stated in "Whole-unit deletion, Finder metadata and quarantine".
- **A mismatch is never unlinked, and no replay resumes it** (corrected here under
  barge#014's exact-head reviews). The first form of the capture rule let a replay
  capture a leftover private name again and verify it against a *fresh*
  observation, so a foreign object a put-back collision had kept under a private
  name could be unlinked by a later run without ever matching the observation its
  capture failed. The second form recorded only the captures whose put-back did
  not succeed, so a successful put-back — which leaves the foreign object back
  under an ordinary name — could still be walked and unlinked by a replay. Now
  every failed capture is recorded with both identities, no run and no replay
  unlinks a private name the intent does not record as its own verified capture,
  and a replay that reads a foreign record stops before it walks. A private name
  with no record is equally unverifiable. The third review
  finding was the order: the record used to be written after the put-back, so a
  crash or a failed publication could leave the object back at an ordinary name
  with nothing to stop a replay. The record is now published first, its
  put-back answer afterwards, and a record that cannot be written means the object
  is never put back at all. Racer and counterfactual tests replace every successive
  entry of a three-level fixture, not one payload; regressions replay a put-back
  collision, a successful put-back, a lost put-back, an unrecordable mismatch and
  a mismatch with no captured facts, and prove the foreign object and the intent
  survive each.
- **Durable custody of captured objects** (added by the amended barge#014 brief,
  the Oxide Supervisor's #155 ruling applied to Barge). Three durable facts carry
  the capture rule across a stop: the `removed` phase after Barge's own final
  unlink, with an absence without it staying `Unknown`; the `captures` chain,
  published before each unlink and each chmod, with the verdict and the original
  full `st_mode`, so only this operation's recorded and verified private objects
  are ever unlinked and a replay re-verifies the same identity; and
  `foreign_captures`, published before each put-back, so a failed capture is never
  relabelled as ordinary content by an abort or a replay. A captured directory
  whose owner cannot write or search it has its mode changed only after the
  original is recorded, and a scope guard restores it on every return, `?` error
  and panic, with a replay restoring recorded modes first. The durable record costs
  one atomic intent publish per entry, which is the deletion's real throughput
  bound inside its 120 s deadline. The crash window between the final unlink and
  the `removed` phase is named in "Durable custody of captured objects" and stays
  pending for an operator.
- **Finder metadata is verified by class even when it was classified** (corrected
  here under barge#014's second exact-head review). A `.DS_Store` or `._name` file
  the classification recorded used to be verified by inode, so Finder rebuilding
  it between the observation and the capture was a mismatch and stopped a
  deletion it should have completed. A Finder-named regular file is now verified
  by class whether or not it was classified, with the same size bound; a
  Finder-named directory, link or oversized file is still a mismatch.
- **Test fixtures and the run root.** Every fixture lives inside one run root: the
  fresh `run.*` directory a caller names in `BARGE_TEST_SCRATCH_ROOT` when it has
  one — a task confined to one directory — and otherwise a fresh `run.*` root the
  test process creates and removes with the fixture. Nothing is created directly
  in the shared scratch parent, and a supplied root is never removed by a test.
  The library testkit and the CLI contract fixture resolve it the same way.
- **Exclusive renames.** `DirOps::rename_child` is the platform's exclusive
  rename (`renameatx_np` with `RENAME_EXCL`): an existing target name answers
  `TargetExists` and nothing is renamed, so the quarantine rename, a capture and a
  put-back can never replace an entry. The one replacing rename left in the layer
  is the state publish, which replaces the temporary name it created itself.
- **Listing one handle twice.** `list_children` rewinds the directory stream it
  opens over a duplicated descriptor. `dup` shares the directory offset, so before
  this correction a second listing through one handle began where the first ended
  and answered empty; the old delete pass masked it because its `ENOTEMPTY` retry
  re-opened the name. The capture rule re-lists the leaf through the handle it
  verified, so the stream is now read from the directory's first name.
- **A settled intent is kept.** `deleted` is the settlement; the record stays in
  `state/intents/` as the journal's own account of what happened, and replay
  treats it as non-pending work. A rolled-back `intent` phase is the one record
  removed, because nothing happened.
- **Replay proofs.** An intent is resolved only from its recorded identity, and
  the windows are conservative. A replay may resume a deletion at `renamed` only
  when the quarantine leaf is at its **visible** `.barge-prune-<intent-id>` name
  with the recorded device and inode and the marker bytes digest to the recorded
  digest (it rolls back instead when the root is still in place at phase
  `intent`). At `marker_last` it resumes the leaf's own capture and removal only
  when the leaf is at that visible name and the marker is positively absent. It
  settles only when the leaf is positively absent **after a recorded `removed`
  phase**, the durable evidence of Barge's own final unlink. A leaf held under a
  recorded `.barge-del-*` name is not a resume point **for the leaf**: its recorded
  mode is restored first and the intent stays pending, because only `removed`
  proves Barge removed it. Elsewhere a resumed walk re-verifies each recorded,
  verified capture - device, inode and kind, through the descriptor it verified -
  and removes it; that is how a nested capture a stop left behind is finished, and
  the record rather than the name authorises the removal. A foreign capture, an
  unrecorded name, a name whose identity no longer matches and an unverifiable name
  are never unlinked and keep the intent pending. Absence alone never resolves an
  intent: an absent leaf without `removed` is `Unknown`, and so is a recorded
  nested capture whose private name is gone. Everything else stays pending and is
  reported. The
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

- **S6** is this slice: the four cache kinds, the three placement forms
  (`setting`, `held-root`, `link`), adopt and undo with byte-level and
  link/root identity proofs, the refusal of a non-empty real directory, and the
  in-place adoption of today's hand-made placements, with `barge cache adopt`,
  `undo` and `list` as the CLI's form of them. Nothing is moved: adopt never
  renames, copies or deletes a cache's data, and undo never deletes it either.
  No real account's setting, link or cache was adopted by this slice; adopting
  Tom's real npm, bun or Cargo placements stays a separate operator gate, and
  its tests run on fixture homes, fixture settings files and fixture volumes.

S6 adoption now publishes a synced `barge.cache-adopt/1` recovery record under
its volume lock before cache effects. There is at most one pending record per
kind (four kinds), with the placement read cap of 64 KiB. A synced private
staging file is claimed through exclusive hard-link publication, so a competing
volume cannot replace the same kind's journal. Replay holds the volume lock and
reverifies that volume before restoring any path. It retains the original
file bytes/identity or directory mode, expected setting bytes, a pre-publication
marker root id/directory identity, observed link publication identity and the
placement candidate before its final publication/readback. `cache undo --kind`
replays only matching observed effects; it restores an interrupted empty-directory
removal with the recorded mode. Divergence or an unobserved publication identity
keeps the intent and effect for explicit operator recovery. A pending adoption
blocks another adoption. Cache recovery records are distinct from deletion
intents and never authorize deleting cache payload.

Cache targets are walked from the verified caches descriptor, checking every
existing component's kind, device and opened identity and refusing `..` or
symlink ancestry. Missing suffixes are created component by component only in
explicit adoption. Undo rechecks a settings file's exact snapshot through its
opened parent immediately before mutation, and a link's stored device/inode
immediately after custody and before unlink. Link records retain original
empty-directory permission bits; restoration verifies the newly created
identity before applying the exact mode through its descriptor. Older records
without the required link identity or mode refuse destructive undo.

Choices S6 made where this design left the mechanic to the slice:

- **One record per kind.** A placement record is
  `state/placements/<placement-id>.json` (`barge.placement/1`), at most 64 KiB
  and one of at most 4096 rows, with exactly `schema`, `placement_id`, `kind`,
  `original_path`, `volume_uuid`, `root_dir`, `folder`, `recorded_path`,
  `adopted_ms`, `generation` and `form`. `deny_unknown_fields` applies at every
  level, a stray name in `placements/` is `unknown_state`, and a record that
  does not validate is refused rather than read as absent. One kind has one
  record: a second adopt of the same kind refuses until that record is undone,
  so a reader never has to choose between two claims about one tool's path.
  `registry` still owns the volume records; the placement record lives in
  `cache`, beside the mechanics that write it, as S3's area record lives in
  `area`.
- **What each form records, and what undo proves.**
  - A `setting` record carries the settings file's identity and its exact before
    and after bytes (hexadecimal, with their digests, sizes and modes), the key's
    address (an optional section and the key), whether adopt changed a byte, the
    directory the setting names, whether that directory was created, and the
    backup copy's path. Adopt writes the backup (`...barge-backup`, hidden,
    private) before it changes anything and preserves the file's original mode.
    Undo restores the recorded before-bytes only when the file still digests to
    the recorded after-bytes, and it removes the file only when adopt created it;
    a file someone edited after adopt is `unknown_state` naming the expected and
    actual digests, with nothing changed.
  - A `link` record carries the link's path, its exact recorded text, the owned
    root's id and marker digest, and what the original path was (`absent` or
    `empty_directory`). Adopt publishes through a temporary name in the same
    directory, so the path is never briefly missing, and proves the published
    text. Undo requires the link to still hold that text; it removes the link,
    re-creates the empty directory when one was replaced, and never deletes the
    owned root or anything in it.
  - A `held-root` record carries the adopted directory, its device and inode,
    the marker's id, nonce and digest, the pre-adoption observation (device,
    inode, entry count, newest entry mtime, and the directory's own allocated
    bytes — not the tree's), and the link that already published it, when there
    was one. Undo proves the directory is still that inode on that device and
    that its marker is still the recorded bytes, then removes only the marker.
    It leaves the data and any pre-existing link exactly as they were.
- **The in-place adoption of today's layout is explicit.** `--adopt-existing-root`
  is the flag that writes a marker into a directory that is already there;
  without it, an unmarked link target refuses with `refused_boundary`, exit `2`,
  a plan, and nothing recorded. With it, the existing link is recorded and the
  marker is written; still nothing is renamed, linked, copied or moved. The
  marker is `barge.root/1` with `kind: cache_root`,
  `origin: adopted_existing` and `area: {namespace: "barge", area: "caches"}` —
  `caches/` is Barge's own folder, so the marker names no consumer declaration.
- **A directory is "on the volume" by path and device.** The design's distinction
  between a directory that already sits on the registered volume and one on the
  internal drive is decided by both facts: the path must be under the recorded
  mount point and the directory's device must be the volume's. Either alone
  would accept the other case — another directory of this machine, or a stale
  mount-point path.
- **Where a link's root lives, and what a setting creates.** A `link` placement's
  owned root is created directly under the volume's `caches/` folder with a
  fresh id, through the same containment walk and marker writer an area root
  uses. A `setting` placement creates `<caches>/<npm|bun>` as a plain directory
  — today's layout — so an unmarked cache directory is never prunable data, and
  it records that it created it.
- **Link publication is exclusive.** The temporary link reaches the original
  path only when `rename_child` reports `Renamed`. `TargetExists` refuses,
  removes only the temporary link and keeps the raced-in entry, even if its
  link text matches the intended target; a matching foreign publication is
  never accepted as Barge's own successful rename.
- **The settings parse.** npm's `cache=` is line-scoped at the file's top level;
  bun's `[install.cache] dir` is line-scoped inside its one section. More than
  one candidate key, a duplicated section, a malformed section header, a
  non-UTF-8 file, or a file over 8 KiB refuses (`unknown_state`) and is not
  edited. A missing key is added only by the explicit adopt, and a value that
  already names a directory under the volume's `caches/` folder is recorded with
  no byte written — today's expected case.
- **An empty directory's custody.** A true directory cannot be replaced by a link
  while a symlink cannot be renamed over it, so the empty-directory case is the
  one moment the tool's path is briefly absent. It runs only after the
  directory's own identity is re-checked, its listing is proved empty, and the
  bounded same-uid open-descriptor observation is complete and empty; a failure
  after the `rmdir` puts the empty directory back. A marker cannot be the proof
  here, because a directory being replaced has none.
- **Command documents.** `barge.cache.adopt/1`, `barge.cache.undo/1` and
  `barge.cache.list/1`. `--plan` is a bare flag that performs the whole
  inspection and reports the intended changes with `placement: null`, writing
  nothing; the same refusals carry `changed: false` and a `plan` list. A refusal
  always precedes every effect: the registry, the volume's presence and identity,
  the declared durability and the absence of an existing record for that kind are
  proved first.
- **`describe`.** `placements` rows are filled from the records: the kind, the
  original and recorded paths, the path the current configuration resolves, the
  observed link state, and `absent: fail` for caches (Q-001). A volume that
  cannot be verified leaves the configured path equal to the recorded one, so
  `describe` stays usable with the drive unplugged, and a publication that cannot
  be inspected is `unknown`, never `missing`.
- **Bounds as applied.** A placement record is at most 64 KiB with 4096 rows; a
  settings file at most 8 KiB; a refused directory's plan keeps the exact entry
  count and at most 16 names; a pre-adoption listing keeps 200 000 names. The
  link-text cap is 4096 bytes.
- **The OS layer's additions.** `DirOps` gains `create_child_symlink` and
  `read_link` (both no-follow: reading a link returns its text or refuses a
  non-link), and `EntryFacts` gains `allocated_bytes`, which is what the
  pre-adoption observation records. `area` gains Barge's own feature-folder
  resolution (`Feature::Caches`) and one public `write_marker`, so creation and
  in-place adoption share one marker writer, one field set and one read-back
  proof instead of two. The writer stages synced bytes at
  `.barge-marker-<root-id>`, publishes by exclusive rename and syncs the opened
  directory. An occupied marker name is refused without replacing it; the
  staged file and in-place adoption intent are kept for inspection. Successful
  readback proves the published inode, nonce and exact digest.
- **Custody before a link removal.** `barge cache undo` of a `link` placement
  reopens the volume's `caches/` folder, proves the owned root is the recorded
  id, inode and marker bytes, and requires a `Safe` custody verdict (released or
  stale holds, a complete and empty same-uid descriptor observation, no pending
  intent) before it removes the link. A plan reports the removal without taking
  custody, because a plan has no effect.
- **An effect without its record is rolled back.** A placement record is
  published last, after the setting write, the link or the marker. When that
  publish fails (`ENOSPC`, `EACCES`, a changed state directory), adopt undoes the
  effect it just made: it removes the link and restores the empty directory it
  replaced, removes the marker it just wrote after re-proving it, or restores the
  settings file's recorded before-bytes and removes an adopted marker. The
  refusal's detail names every step that was rolled back, so no filesystem effect
  is stranded without the record that proves it. The owned root a link pointed at
  is left in place: it carries its own marker, holds no payload, and S6 removes
  nothing on a volume.
- **A fixture-only account home.** The native proof needs a private account home
  (its original paths resolve from the account home, never from `$HOME`), so S6
  adds `BARGE_ACCOUNT_HOME`, the documented fixture-only sibling of
  `BARGE_STATE_DIR`: it is consulted only when set, it must be an absolute real
  directory of this account, and a normal run leaves it unset. State resolution
  is unchanged: `BARGE_STATE_DIR` still owns the state directory, and neither
  variable is inferred from `$HOME`.
- **The native proof's plan.** [The S6 native proof plan](s6-native-proof.md)
  records the elevated run's fixed command, the pinned supervisor bytes, the nine
  HIGH premises traced through the case bodies, the receipt shape, and the
  activation order. Effigy gains `cache:native` and its uncounted
  `cache:native:diagnostic` twin; both are owner-activated and neither is part of
  an ordinary validation pass.

The read-only S6 apparatus command `barge diagnostic descriptor-census --root
<existing-directory> --json` uses the same binary and production observer as
cache operations, with custody's scan bounds, self-exclusion and barge#012's
bounded identity re-observation. It returns
`barge.diagnostic.descriptor-census/1`, preserving counts, retry evidence and
the complete product `os_blind_spot` record when supplied;
complete observation is `passed`, unreadable or bounded is `infrastructure`.
It grants no custody authority, writes nothing and never substitutes for the
operation's own scan. The [proof contract](s6-native-proof.md#the-harness-preflight)
owns its apparatus use, receipt interpretation and activation limits.

- **S4** is this slice: project scratch allocation and conclusion, the expiry
  sweep with its plan and digest, the recorded grants (foreign-family
  delegations and legacy claims) with their declaration verbs, and the
  `barge scratch` documents. It deletes only through S5's custody and delete
  path, and its one bounded native carries the scratch and deletion cases
  together. Its tests use injected volume facts over real local directories and
  never read or write a real drive or the real account state.

Choices S4 made where this design left the mechanic to the slice:

- **Family field sets, pinned.** The Oxide marker is exactly the twelve fields
  its supervisor's `MARKER_KEYS` names, all required: `version` (integer 1),
  `run_id` (32 lowercase hex), `root` (the absolute root path), `owner_uid`,
  `device`, `inode`, `created_unix` and `concluded_unix` in seconds,
  `outcome`/`closure`/`kept_reason`/`nested_scope` as their pinned value sets,
  plus the source's own conclusion consistency (a concluded root is `closed`
  with a positive closure). The file must be a private regular file of this user
  at most 4 KiB, only `version` 1 is read, and the marker's `root`, `owner_uid`,
  `device` and `inode` must equal the unit the walk reached. The Effigy marker is
  exactly its `Marker` fields, with `task`, `checkout`, `pid`, `pid_start`, and
  the optional `keep`/`concluded_ms` defaulted as that struct defaults them;
  `run` and `created_ms` are required, `pid_start` is text, and an extra field is
  refused. Every strict-parse failure keeps the unit and is reported with its
  reason, never as absent.
- **The unit vocabulary.** The sweep recognises both layouts: a marked
  directory directly under the scratch root is one unit, and a child of an
  unmarked project folder is one unit. `oxide-validation/` is never a unit and no
  claim may cover it; the `oxide` project is never claimed, though marked family
  roots inside it are classified under delegation. Eight-character names follow
  the explicit O-005 collision ruling below. A dotted or non-directory top-level
  entry is one `top_level_adhoc` entry. An entry named
  `.barge-prune-*` is a pending deletion's quarantine leaf, is never a unit, and
  is settled only by a replay.
- **The policy constants.** Scratch is Barge's own folder, not a declared area,
  and `barge.registry/1`'s frozen field set has no scratch knob, so the design's
  ages are constants the plan reports: native and Oxide seven days, Effigy's keep
  retention 24 hours and its undecidable-liveness age seven days. Adding an
  override needs a registry field and its own slice; a flag would be an
  unrecorded policy.
- **Effigy eligibility is at least as conservative as Effigy's own sweep.** A
  recorded `pid` that is positively alive keeps the directory however old it is,
  because Barge cannot prove it is a different process from the marker's text
  start identity (and a live pid whose identity is unreadable is kept the same
  way). Eligible is a kept conclusion older than 24 hours, or an *unconcluded*
  run whose recorded process is positively gone, or one whose liveness cannot be
  decided and whose creation is older than seven days. Barge thus deletes a
  subset of what that family's own sweep would.
- **Nested markers in marked roots.** Before a native or delegated root is
  planned, a bounded no-follow walk checks for recognised markers other than the
  unit's own top-level marker. A nested marker or incomplete walk keeps and
  reports the root; the shared deletion walk checks again before capture.
- **The plan is the authorization.** `barge.scratch.plan/1` holds only stable
  facts (the volume, folder, policy constants, the candidate rows with their
  marker identity and unit kind, and the kept rows with their reasons) and its SHA-256 is the
  digest. A real run requires `--plan-sha256`; it recomputes the plan and refuses
  when the digest differs, changing nothing else. Because a change to any unit's
  eligibility changes the plan, that is a whole-run refusal rather than a
  partial run, which is the conservative reading of "the real run takes exactly
  that plan". A change *inside* the run's own plan-to-effect window is caught by
  the per-candidate re-verification, which abandons only the affected unit.
- **Order of a real run.** Replay first would make the preview's digest
  unreproducible, so the order is: compute the plan, require its digest, replay
  every pending intent from its recorded identity, then for each candidate
  re-read the grant under the volume, re-assess the unit and delete it. A preview
  never replays.
- **Grant declaration verbs (O-009).** `barge scratch delegate` records one
  family's delegation and `barge scratch claim` one scope's legacy claim, each
  with the declared-by name and time, into the volume's `scratch_grants`; either
  can be withdrawn without touching data. Barge records the declaration and
  cannot authenticate its author, so the review is the operator's; the verbs
  refuse a claim naming `oxide` or a dotted name, and refuse a unit shape that
  does not match its scope. Eight-character names follow O-005 below.
- **The foreign and unmarked deletion proof.** S4 passes the previewed kind,
  identity, optional family marker and recorded grant to S5's shared deletion
  unit. Custody re-reads those facts and the grant before the intent; a newly
  marked unit is kept. `barge.intent/2` records the unit and its private capture
  name; S5 captures, verifies and journals every removal and replays only from
  recorded identity. A marker is removed last; a file has no contents or marker.
  The durable `removed` phase alone proves the final unlink. `barge.intent/1`
  remains readable as a Barge-marked directory unit.
- **Custody drops its own descriptor.** S5's identity check opened the root to
  read its device and inode, and the same-uid scan then matched that descriptor,
  so every real deletion would have been refused. The handle is dropped before
  the scan. A caller's own open handle, including an Oxide-linked caller's, is
  still observed and still blocks the delete.
- **Conclusion and the owner hold.** Allocation records the consumer as the
  root's owner in a `barge.hold/1` whose purpose is the fixed owner string, so
  "the recorded owner" is a recorded fact. Conclusion requires that hold to be
  live, re-reads the marker immediately before writing, refuses a second
  conclusion (terminal), and records a positive closure only when no other hold
  on the root is live or undecidable and no pending intent names it.
- **Volume absence.** `barge scratch expire` on a permanent volume refuses
  `volume_absent` loudly and writes nothing; on an ephemeral volume it reports
  `volume_present: false`, an empty plan and no effect. Neither recreates or
  writes a stale mount point.
- **Command documents.** `barge.scratch.alloc/1`, `barge.scratch.conclude/1`,
  `barge.scratch.expire/1` (with `barge.scratch.plan/1` inside it),
  `barge.scratch.grants/1`, `barge.scratch.delegate/1`, `barge.scratch.claim/1`
  and `barge.scratch.revoke/1`. They are command documents and are not added to
  `barge.describe/1`'s `formats` list.
- **Bounds as applied.** A sweep examines 4096 entries within 1 s, reports
  `truncated` and counts when it stops, lists a directory to 200 000 names, walks
  a claimed unit to 12 levels for its newest entry, and keeps the deletion's own
  120 s per-root deadline and three `ENOTEMPTY` attempts. The plan and its rows
  use the root row cap.
- **What the native proves, and what it does not.** The proof's case map is
  bound to its dispatch by its own self-check: every case name resolves to a
  `case_<name>` method, no case method is undispatched, and every dependency is
  evaluated first, so a renamed case cannot pass unnoticed. Its `ENOTEMPTY` case
  injects a writer that recreates an entry inside the quarantined leaf while the
  delete runs; a native cannot inject a platform fault at an exact step, so the
  case asserts the design's observable — one pending intent and the kept leaf,
  whichever conservative stop the race produces — and the marker-last
  `ENOTEMPTY` retry itself stays proved by the counted unit counterfactual.
- **The native's same-uid observability preflight (owner ruling, then the
  Supervisor's execution-identity correction).** Some hosts refuse
  `proc_pidinfo` on processes of their own effective user — observed here as
  `EPERM` on system daemons — and custody correctly reads an unreadable process as
  `Unknown` and refuses. Whether this account may read its own processes is an
  environment capability, not a product fact, so before any case, inside the same
  qualified supervisor, the census is taken by **Barge's own binary, never by the
  proof's Python**: one probe root is allocated, positively concluded, released
  and aged in the guard's owned image, then really deleted by
  `barge scratch expire`. That deletion's custody runs the product's
  same-effective-uid descriptor scan — the same observer, the same bounds, the
  same foreign-uid classification and exited-process handling, and barge#012's
  bounded same-uid `EPERM` re-observation — so the scan's execution identity is
  exactly the one every case's deletion uses. A product custody refusal (an
  unreadable same-uid process after its bounded re-observation, or a scan bound)
  makes the run `infrastructure`, unqualified, aborted before any case (exit 4),
  with the product's own reason recorded in the receipt; any other probe outcome
  is an apparatus defect, never a pass. No product rule, path exception or case
  oracle changes; hidden descriptors of a verified platform binary follow the
  named OS blind-spot ruling, while unverified hidden descriptors still refuse.
- **Grants are re-verified in the deletion's window, policy included.** The
  plan's assessment reads the grants, but the authority that matters is read
  again inside S5's locked deletion: `delete_unit_on_volume` takes the volume
  lock, re-reads the registry record, and runs the caller's own authorization
  check on it before custody or the intent. S4's check re-evaluates the **whole
  current eligibility policy** under that record — the family delegation or the
  scope's claim, the claim's unit shape and the claim's own idle age — and
  compares the recorded identity, so replacing a claim with a stricter one
  between the plan and the effect refuses the unit instead of authorizing it under
  the older policy. A withdrawal or stricter replacement before that point refuses
  with `refused_boundary` and no effect; a change attempted after it waits for the
  window. Grant writes therefore take the registry lock and then the volume lock,
  in the documented order, and re-read and modify the document under both: a
  concurrent writer's declaration is merged rather than discarded, and a stale
  document can never restore withdrawn authority. The write's registration
  preflight runs before the state layout exists, so a grant verb naming an
  unregistered volume refuses and creates no directory, lock or file; the locked
  re-read still re-checks presence before publishing.
- **Conclusion facts are validated, not just recorded.** A positive closure is
  only written for a `closed` outcome, so a retained, unclosed or failed run can
  never carry the one closure that expires it. The expiry reader applies the same
  rule to both families: a native conclusion whose `outcome` is not `closed` is
  never eligible, and an Oxide marker must have a 32-hex `run_id`, `outcome:
  closed`, and a readable, non-future `created_unix` that is not later than its
  conclusion — an unusable value is kept, never defaulted to zero.
- **A quarantined file unit replays.** A claimed ad-hoc entry may be a regular
  file, and replay no longer opens such a leaf as a directory (`O_DIRECTORY`
  would fail and leave the intent pending forever). A verified file leaf reaches
  its own phase handling: `renamed` finishes the unit, `marker_last` removes the
  leaf through the same verified capture path and records `removed` before settlement.
- **An unsettled replay is unsettled.** `has_pending` covers both a new pending
  intent and a replay that stopped short (`replay.pending`), so a real run with
  an empty candidate plan and an unresolved earlier intent exits `3` instead of
  reporting success.
- **The examination budget is charged per entry.** A claimed unit's walk spends
  one examination for every entry it reads, not one per directory, so a unit
  holding thousands of files cannot be walked past the plan's own bound; the
  deadline is checked by the same spend. A project folder whose listing stopped
  at the name bound sets the plan's `truncated` flag instead of presenting a
  partial plan as whole.
- **The native's same-uid census is the product's own.** The preflight no longer
  observes the host from Python at all: the Barge binary runs the scan through a
  real probe deletion, so the census is the product's `MacOs` custody scan with
  its own declared bounds (4096 processes, 65 536 descriptors, 10 s per scan, and
  barge#012's three retries inside a 250 ms settle window), its foreign-effective-
  uid exclusion, its exited-process handling and its bounded `EPERM`
  re-observation. An unreadable same-uid process or a bound is `infrastructure`,
  unqualified, before any case, and the receipt records the census outcome and the
  product's reason.
- **Premise 2 is a complete-content assertion.** The proof observes the real
  account state before the run and after it through descriptors: the state
  directory is opened with `O_NOFOLLOW` and every name relative to its parent's
  descriptor with `O_NOFOLLOW`, so a symlinked or swapped path is an incomplete
  observation and never a followed one, and each entry's name, kind, mode, size,
  mtime and bytes are hashed. The entry cap, the byte cap, the deadline, a short
  read (fewer bytes than the file's own facts report), a file that grew, or any
  size or mtime change between the entry's facts, its open and the end of its
  read make the observation `incomplete`; the deadline is re-checked after every
  file read and again before a complete observation is reported, so a read or
  filesystem step that finished past the bound cannot be labelled complete. Two
  incomplete observations never prove equality, and an incomplete or changed
  reading makes the receipt unqualified.
  The resolver is proved from fixture-only evidence, never from `describe` (which
  observes live Oxide state), and from the environment the **child commands** run
  with rather than the proof process's own: the child's `BARGE_STATE_DIR` must be
  the fixture state directory and its `HOME` the private fixture home, distinct
  from the password-database account home, and a delegation recorded through a
  state-only verb must land in the proof's fixture registry with its withdrawal
  removing it.
- **A failed ordinary detach keeps the backing.** Cleanup detaches ordinarily and
  stops there: it never force-detaches by mount path, and a failed detach leaves
  the image attached and its backing in place and makes the receipt unqualified.
  The surprise-removal case alone force-detaches the positively rebound owned
  image whole-disk device; all other detaches remain ordinary.
- **The native's oracles are the design's behaviours.** The allocation case
  launches both allocations before awaiting either; the pull case uses the
  owned-image surprise removal mid-delete, and checks the
  quarantine leaf and its recorded device and inode *after* re-attaching, before
  the replay.
- **The native ENOTEMPTY injection.** The fixed case compiles a proof-only
  interposer under the supervisor's scratch and injects it into one real Barge
  CLI invocation. Before Barge renames the exact fixture directory, the shim
  pins its device and inode from `BARGE_PROOF_ENOTEMPTY_PATH`; it returns
  `ENOTEMPTY` on three matching `unlinkat(AT_REMOVEDIR)` calls, logs each to
  supervisor evidence and passes every other call through. The receipt carries
  both source and library hashes, exact-target firings, the pending reason and
  an observed durable `removed` phase on unshimmed replay. Missing observations
  fail the case. A separate unshimmed unit of the same shape must delete normally.
  The runner always supplies the designated scratch parent to the qualified
  supervisor and does not inherit an adopted scratch root or injection hooks.
- **What S4 does not claim.** No real-drive sweep, no reaper or LaunchAgent
  change, no Oxide or Effigy adoption, and no native receipt until the owner
  activates the runs. `scratch:expiry:native` and its `:diagnostic` twin are
  declared and held.

## Proposed validation

Design-only validation for this contract is `effigy docs:check` once, run from
the repository root. No Rust, native, APFS or qualified-runner run is part of
this design task; the tests below are proposed for the implementation slices.
Every fixture an implementation test creates lives inside one fresh task-owned
run root (`BARGE_TEST_SCRATCH_ROOT` when a caller supplies one), never directly in
a shared scratch parent.

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
| Custody | state machine transitions; `Safe` required by type | unreadable hold, bounded-out scan, live pid with a mismatched start identity, foreign-uid evidence all yield `Unknown` and refuse; a hidden vnode descriptor of a verified platform binary is recorded as the OS blind spot and does not block, and the same descriptor refuses without the platform flag, with a failed status read, and with a start identity that moved around it |
| Deletion | phase machine; Finder metadata classification; ENOTEMPTY retry; marker last; capture and verify for every entry | unknown entry stops the delete; symlink inside quarantine stops it; injected ENOTEMPTY three times leaves a pending intent; injected device change stops before the first unlink; a racer that replaces each entry after its check removes nothing foreign and leaves the intent pending; a symlink and a directory swap are captured and never followed or recursed into; a recreated Finder file is removed by class; a put-back collision keeps both names and records the failed capture; a successful put-back is recorded too; the record is durable before the put-back (probed at the put-back itself); a lost put-back and an unrecordable mismatch keep the object private and the intent pending; a mismatch with no captured facts is still recorded; an exclusive rename onto an existing name refuses; no replay unlinks an unrecorded leftover private name and a replay resumes only recorded, verified captures; replays after a collision, a successful put-back, a lost put-back and an unrecordable mismatch keep the foreign object and the intent; a classified Finder file rebuilt between observation and capture is removed by class and one over the size bound stops; a captured private name that vanishes mid-run leaves the intent pending; an absent leaf without `removed` stays pending and settles with it; a nested mismatch with a put-back collision keeps the foreign object under its private name and never relabels it; an unrecorded private name planted before a replay is never unlinked; a read-only captured directory and a read-only leaf are removed whole; a replay resumes a captured nested directory and finishes the delete; a recorded capture that vanishes before a replay keeps it pending, a capture swapped at its replay chmod is not chmodded, and a marker that cannot be read still restores the recorded mode first; a leaf swapped at the area level before its final removal is put back and never removed, with the removed phase never reached; a marker-last replay never removes a swapped leaf and a stop after `marker_last` before the leaf capture resumes the leaf capture on replay; the classification never recurses into a directory swapped between its read and its open; a replay restores a captured leaf's recorded mode when the visible name is empty; the recorded mode is durable before the chmod (probed at the change); an injected listing and an injected entry-read failure after the chmod each restore the recorded mode and record the abort; a failed restore stays pending with the mode recorded and a replay restores it first; the old check-then-unlink removes the racer's object where capture-and-verify keeps it |
| Contacts | JSON documents match their schemas; unknown major refuses; optional-field addition accepted | a `barge.describe/1` document with a removed field or a changed type is refused by its own reader |
| Foreign state | Oxide `removable.json` v1/v2 and `pools.json` parsers; the pinned Oxide and Effigy scratch marker field sets; delegations and legacy claims | unknown field, unknown version, unparsable and unreadable inputs become `unknown_state` observations; an Effigy marker that omits `task`/`checkout`/`pid`/`pid_start` is valid, and one that adds a field is refused; a unit whose family has no delegation or whose scope has no claim is reported, never deleted |
| Prune/maintain | plan canonicalisation, digest mismatch, ordering | a changed plan digest, a changed root identity, a changed registry revision, a live hold, a revoked claim and a unit that became marked each stop the affected unit only |

The host reaper's current behaviours are the counterfactual baseline for scratch
expiry: units are the children of each project folder and the top-level entries
that are files or have a dot in the name; a unit goes once its newest entry is
fourteen days old and no process has it open; it never follows a symlink, never
deletes outside `scratch/`, skips the `oxide` project and Effigy-marked units,
keeps a busy unit, retries `ENOTEMPTY` and stops at a run budget. Since
2026-10-27 all other plain-named top-level directories are projects; the prior
script is backed up as `reap.py.bak-20261027`. For unmarked units under recorded claims, Barge
reaches the same set and is at least as conservative on those conditions, apart
from the explicit O-005
ruling: an unmarked bare eight-character directory can be deleted as one unit
under a top-level claim, although the reaper treats it as a project and only
considers its children. Separately delegated families follow their own recorded
eligibility, even when the reaper skips them. The tests pin that exception and the no-grant result (nothing deleted, everything reported).

### Test fixtures on the real OS

A test that exercises the real OS layer needs a real APFS path, so it cannot use
an injected fact. Those fixtures live inside one **run root**: the fresh
`/Volumes/Builds/scratch/barge/run.*` directory the invocation names in
`BARGE_TEST_SCRATCH_ROOT`, or a `run.*` directory the test process creates and
removes for itself. Nothing is ever created directly in the shared scratch parent
(`/Volumes/Builds/scratch/barge`), a supplied root is never removed by the test,
and every fixture removes itself, so a task that must confine its writes to one
fresh directory can supply that directory and hold the whole suite inside it.
The containment control proves the three properties directly: a supplied root
holds the fixture and survives it, a self-created root is removed with its
fixture, and the fixture base is a run root rather than the shared parent.

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
  pending intent; pull the volume (force-detach only the verified owned whole-disk device as the surprise-removal premise) mid-delete and
  prove a pending intent with the quarantine leaf kept, then replay after
  re-attach; prove a stale mount point is never written to; plant one Oxide
  marker, one Effigy marker and one unmarked unit, prove all three survive with
  no delegation or claim, then declare the delegation and claim and prove only
  the eligible units go.
- Explicitly not claimed: a real USB/Thunderbolt surprise removal, a real
  drive's flush cost, the internal SSD write saving, and any qualification of
  Oxide's or Effigy's adoption.
- Pull mid-delete (Tom, 2026-10-27, decision 7e4f14c8). An ordinary detach
  refuses (`EBUSY`) while the deleting process holds descriptors on the image.
  The preferred proof is therefore ordinary-only: hold the CLI at a declared
  shim pause after capture, `SIGKILL` its group, detach ordinarily, prove the
  absent-volume refusal, then replay after re-attach. A forced detach of the
  proof image's own positively identified whole-disk device is allowed in this
  one case only as a fallback, if the ordinary-only proof cannot be made to
  work. That needs its own brief amendment and fresh proofs. Every other
  detach stays ordinary.

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

  **Implementation note (architect ruling, Oxide Supervisor message 2ff249cb,
  2026-10-26).** The collision rule is decided by the claim, never by the name,
  and S4 implements it this way:

  - An explicit recorded `project:<name>` claim is valid for **any** project name
    the unit vocabulary allows — including an eight-character `underlay` or
    `figmatic`. There is no known-names list anywhere in the classifier: a name
    never establishes ownership, and a project claim never depends on one.
  - Marker precedence is unchanged: an Effigy- or Oxide-marked root inside a
    claimed project folder is that family's unit and needs the family's own
    delegation. A project claim covers only unmarked children.
  - An unmarked eight-character top-level directory with **no** project claim
    survives and is reported; it is an ad-hoc unit only when a `top_level_adhoc`
    claim covers it, and only then is the directory itself deletable.
  - A `project:<name>` claim for an eight-character name and a `top_level_adhoc`
    claim may not both be recorded: the same directory could be read either way,
    so the second declaration refuses and the operator withdraws one first. That
    is why the reaper's replacement stays the design's staged sequence of
    declarations rather than one switch.
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
  qualified with that later slice, not assumed here. The [stage 3 space
  policy](stage3-space-policy.md) owns that design, including `f_bavail` versus
  `f_bfree`, the flush before a post-delete measurement and purgeable space.
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
