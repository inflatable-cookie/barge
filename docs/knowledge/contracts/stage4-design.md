# Stage 4 design: remount-stable owned-root identity and conservative absence proofs for pruning

This contract is the design for Barge's owned-root identity and for the absence
proof that lets a root be pruned once the consumer that used it is positively
gone. It exists because one live prune refused 23 otherwise-eligible roots with
"the recorded root's device or inode changed", and because the same question —
"is this path really gone?" — was being answered separately inside Oxide.

It fixes: what a root's identity is and which facts may be compared across a
remount; the versioned identity and absence documents; where they are written
and under which lock; what "gone" may and may not be proved from; the one gate
that authorizes deleting a whole owned root; the failure, race and cancellation
matrix; the resource bounds; the alternatives rejected; the test plan; one
bounded native design; and the implementation slices in dependency order.

[Architecture](../architecture.md) owns the concepts, the invariants and the
boundary with Oxide. The [stage 1 design](stage1-design.md) owns the crate and
CLI shape, the state directory, the registry, areas, root markers, holds,
custody, whole-unit deletion and the plan/intent mechanics this contract
extends. Where they differ, they win and this file is corrected.

The marker writer emits `barge.root/2`; the reader also accepts `barge.root/1`
at its exact recorded path. The v1 mount-prefix relaxation remains in F2.
Creation and in-place cache adoption record the immutable identity under a root
lock; verification reads a fresh bounded mount census. `barge describe` reports
the identity capability and each registered UUID's census, with unavailable
censuses reported as unknown. `barge area root identify --volume <uuid> --path
<mount-relative-path>` explicitly identifies a root without a record only after
verifying its volume and marker and obtaining safe custody. It never overwrites
a record.

The release gate plans and does not act: `barge prune --plan --area <ns>/<area>
--volume <uuid>` writes one `barge.prune.plan/1` inside `barge.prune/1`, with
every row's six conditions and their evidence, and changes nothing. Its age is
the bounded scan of the root's subtree this contract fixes, its reference set is
read from Barge's own placements, area declarations and holds and from Oxide's
registry, receipts and root markers strictly in place, and the intent, the
effect and the replay remain F4.

No part of this contract authorizes a native run, a real drive, a mount or host
setting change, a data move, a deletion, or any change to Oxide.

## Scope and authority

### Frozen inputs

- Tom's scope ruling and the authority list in [vision](../vision.md).
- The invariants in [architecture](../architecture.md#invariants), including the
  one this contract implements: owned-root identity survives remounts, as
  volume UUID plus a stable per-root identity, never the raw device number
  alone.
- The [stage 1 design](stage1-design.md) as frozen at `8b36b06`, the [stage 3
  space policy](stage3-space-policy.md), and the code merged on `main` at
  `ce9d6ab`: slices S1, S2, [S3](stage1-design.md#implementation-status) (the
  containment walk, areas, `barge.root/1` markers, holds, root listing) and S5
  (custody, the `barge.intent/2` journal, quarantine, whole-unit delete, replay).
  S3 records no durable identity for a root, and S5's intent carries the
  within-run observation only; that gap is what this contract closes, and every
  statement below about existing behaviour is checked against that tree.
- Oxide's `oxide outputs prune` as the implementation whose behaviour this
  replaces, and Oxide's absence rule as the compatibility target, both read at
  the pinned commit in [sources](#sources-and-version-identity).
- Decisions: `88cc123e` delegates design freezes to the Oxide Supervisor;
  `ff13f2ef` holds the 91 legacy Oxide output roots until Oxide's own absence
  rule (its task `40010b40`, which stays in Oxide) lands and this identity work
  is adopted; Q-004 fixes reading Oxide's records
  in place; `765f84c6` part (b) moved owned-root identity out of Oxide into
  Barge so it is designed once, here.
- The stage 1 freeze's own deferrals: the retirement of any foreign format needs
  its own slice, and reserve and floor pruning belong to the [stage 3 space
  policy](stage3-space-policy.md).

### The carried finding

On 2026-10-26 the installed `069b7fa6` ran Oxide's own prune: it deleted 114
roots and kept 276. Of those kept, 23 were kept with the reason text "the
recorded root's device or inode changed" — the message Oxide emits when a
recorded `st_dev`/`st_ino` pair no longer matches the live directory. The
metadata check itself is in Oxide's `prune.rs`, in both the plan-time
observation and the effect-time session open, so a mismatch is caught twice.

The probable cause — a reboot, a dock power cycle or a reconnect giving the same
APFS volume a different device number — is carried as **unproven**. Nothing in
this contract depends on the cause; [what the finding does and does not
prove](#what-the-finding-does-and-does-not-prove) separates the two facts and
says which one the design may tolerate and which it may not.

### In scope

Owned-root identity as a durable, versioned record; identity verification with
its tolerated and refused changes; the mount and volume facts that make it work
after a remount, including one UUID mounted twice; the generic positive-absence
proof for an absent consumer path; the release gate that authorizes giving up one
whole owned root, and how it combines with the merged stage 3 space prune; the
plan and intent additions that carry identity and absence
evidence; the read-only diagnostic that makes the carried finding decidable; the
Oxide compatibility reading; the implementation slices; the tests; one bounded
native design.

### Out of scope

Any live run, real drive or data change. Registering volumes, changing a
volume's mode, reserve or floor, and measuring space are the [stage 3 space
policy's](stage3-space-policy.md); this contract consumes its trigger and never
sets one.
Moving data, adopting a consumer's area, retiring a foreign format, changing the
host job. Rewriting an existing root's identity. Deciding whether Oxide's own
prune should stop comparing `st_dev`: that is an Oxide change, in Oxide's
project, and this contract only states the compatibility reading and the
evidence a Barge run can produce for it.

## Sources and version identity

Read at these exact versions; a later version is a new input, not an inherited
qualification.

| Source | Identity | Used for |
| --- | --- | --- |
| `inflatable-cookie/barge` `main` | `ce9d6abea57ac924bc4b6b5f64df8509ef03d0b6` (2026-10-26T17:48:28Z) | the stage 1 contract at `8b36b06` and its implementation status, the [stage 3 space policy](stage3-space-policy.md), and the merged S1, S2, S3 and S5 code (`crates/barge/src/os`, `state`, `registry`, `inspect`, `describe`, `contract`, `area`, `custody`, `delete`, `digest`), AGENTS.md and `effigy.toml` |
| `inflatable-cookie/oxide` `main` | `5ce34c5526dd4945ced71ca9a6eabadcdcad206e` (2026-10-26T14:17:11Z) | formats and rules Barge reads in place, as reference only |

Oxide references below point at that commit:

- `src/bin/oxide/prune.rs` — `observe_row_root` (the plan-time identity check
  and the reason strings), `RootSession::open` (the effect-time check),
  `checkout_absence` (the no-follow walk and its `Absent`/`Present`/`Unknown`
  verdicts), `nearest_ancestor`, `on_boot_data_volume`, `on_recorded_volume`,
  `REGISTRY_VERSION` (`oxide.outputs.registry/2`, rows carrying `root_dev`,
  `root_ino`, `marker_digest`, `nonce`, `generation`, `volume_uuid`), and
  `registry_path` (`<internal pool>/outputs/registry.json`).
- `src/bin/oxide/outputs.rs` — `MARKER` (`.oxide-output.json`),
  `MARKER_VERSION` (`oxide.outputs.root/2`) and the `Marker` field set
  (`checkout`, `checkout_id`, `label`, `recipe`, `kind`, `volume_uuid`,
  `nonce`, `checkout_volume_uuid`, `generation`, `created_ms`),
  `validate_root_device` (the root is a real directory on the verified volume,
  and never a mount root).
- `src/volume.rs` — `Binding`, `dir_at`, `verify`, `refuse_mount_root`.
- `docs/knowledge/contracts/workflow.md` — "Pruning owned output roots": the
  candidate rule, the operator-list waiver of receipt-history completeness, and
  the host automation that runs preview-then-real.

Barge's own `os` layer already reads `ATTR_VOL_UUID` through an
opened descriptor, refuses a non-APFS filesystem, refuses the boot volume, and
requires a managed volume to be a real directory directly under `/Volumes`
(`crates/barge/src/os/macos.rs`, `os/mod.rs::verify_volume`). S3 added the
no-follow component walk that compares each component's checked and opened
identity, which the absence primitive reuses. Neither has a way to
*find* a volume by UUID, only to verify a path against one; this contract adds
that seam where it is needed and names it.

Anything uncertain about these sources is a named [open design
issue](#open-design-issues), never a qualification claim.

## What the finding does and does not prove

### The reason text conflates two facts

Oxide compares `st_dev` and `st_ino` in one branch and prints one message for
both. A refused root therefore says only "one of two recorded numbers differs".
The first thing this design owes the operator is a verdict that names which one,
with both values, because the correct response differs completely:

- a changed **device** is normal and must never refuse on its own;
- a changed **inode** means the directory that was verified is not the directory
  that was recorded, and must keep the root.

The diagnostic for the carried finding is therefore part of the design, not a
follow-up: [the read-only census](#reading-the-census-without-adopting-anything).

### Why a device number moves across a remount

`st_dev` is the identifier of the *mounted* filesystem instance. On macOS it is
derived from the mount, so detaching and re-attaching a drive, a reboot, a dock
power cycle, or re-attaching an image gives the same APFS volume a new device
number even though nothing in the filesystem changed. It is a fact about this
mount, not about the data.

### Why an inode does not

A directory's inode number is filesystem metadata written when the directory was
created: it belongs to the volume, and it survives unmounting, rebooting and
re-attaching. It can only change if the directory itself was replaced, or if the
number was reused for a different object after the original was deleted. Both of
those are exactly the events that must keep a root.

### What the design does not assume

The design does not assume the 23 roots were kept by a device change. It assumes
only this: a fact that can change without the data changing (the device) is
never durable identity, and a fact that changes only when the data changes (the
inode, the marker's random identity) is. If the census later shows an inode
change, this design keeps those roots and reports them, which is what it would
have done anyway.

## Root identity

### The facts, and the three roles a fact may have

| Fact | Read from | Survives a remount | Role |
| --- | --- | --- | --- |
| `volume_uuid` | the kernel's `ATTR_VOL_UUID` through the verified mount descriptor | yes | identity |
| `root_id` | `.barge-root.json`, 128-bit random, written once | yes | identity |
| `nonce` | `.barge-root.json`, 128-bit random, written once | yes | identity |
| `inode` | `fstat` of the root directory through a descriptor opened from the verified mount | yes; reused only after deletion | identity, conjoined |
| `mount_relative_path` | the component chain walked from the verified mount root | yes | identity |
| `kind`, `area` | `.barge-root.json` and the identity record | yes | binding: must agree, does not by itself identify |
| `device` | `fstat` of the same descriptor | **no** | within-run consistency, diagnosis |
| `marker_digest` | SHA-256 of the marker bytes read now | changes when a conclusion is written | within-run proof only |
| `mount_at_record` | the mount point observed when the record was written | no | hint for finding the volume, never identity |

Two rules follow, and they are the whole contract:

1. **Nothing durable compares a device number.** A device is compared only
   within one run, between two readings of the same open descriptor, to prove
   that a walk did not cross to another filesystem or that the volume did not
   change under the walk.
2. **The marker digest is never recorded as identity.** A `scratch_run` root's
   marker legitimately gains its conclusion once (stage 1, "Allocation and
   conclusion"), so pinning its bytes would make a correctly concluded root
   unverifiable. The digest is a plan-and-intent fact, re-read at each step.

Identity is the conjunction of the four identity rows plus the two binding rows.
Any one of them disagreeing keeps the root.

### The identity record

One record per owned root, in account state, written when the root is created or
adopted and never rewritten afterwards:

```text
state/roots/<root-id>/identity.json     barge.root.identity/1
```

```json
{
  "schema": "barge.root.identity/1",
  "root_id": "<32 lowercase hex>",
  "nonce": "<32 lowercase hex>",
  "volume_uuid": "<uuid>",
  "mount_relative_path": "oxide/outputs/2026-10-26T08-15-00Z-3f9c",
  "inode": 1234567,
  "device_at_record": 16777234,
  "mount_at_record": "/Volumes/Builds",
  "kind": "area_root",
  "area": {"namespace": "oxide", "area": "outputs"},
  "origin": "created",
  "marker_generation_at_record": 1,
  "recorded_ms": 0
}
```

Validation is strict and bounded: exactly these fields, no unknown field, a
known version, `root_id`/`nonce` of the right shape, a canonical `volume_uuid`,
a `mount_relative_path` that passes [containment](stage1-design.md#path-containment-and-resolution)
in its relative form (non-empty components, no `.` or `..`, no leading or
trailing separator, at least one component below the mount), `kind` in
`scratch_run`/`cache_root`/`area_root`, `origin` in `created`/
`adopted_existing`/`identified` — the first two are stage 1's values, spelled
its way, and `identified` is [O4-002](#open-design-issues)'s repair — `inode` and `device_at_record` non-zero, at most 4 KiB, a regular
file of this user with private mode, no symlink. An invalid record is
`unknown_state`: the root is kept and every prune plan names it.

The record lives in account state, not in the marker, for three reasons:

- the [stage 1 design](stage1-design.md#read-only-inspection-versus-authoritative-writes)
  says only authoritative state authorizes an effect, and deletion authority is
  an effect; a fact that authorizes deletion belongs with the other
  authoritative facts, not in bytes that travel with the data;
- Oxide, the implementation this replaces, also records identity in its own
  registry (`root_dev`, `root_ino`, `nonce`, `generation`, `marker_digest`) and
  keeps the marker for ownership, so Barge mirrors the consumer it replaces
  instead of inventing a second shape;
- a marker that carries its own inode is self-referential, and it would make the
  marker's bytes depend on where the root happens to be, which is exactly the
  coupling this design removes.

`mount_relative_path` is the path below the verified mount root, not an absolute
path. That is the same rule the registry already applies to `mount_hint`: the
absolute path is an observation for humans, the thing that decides is the
identity. `state/roots/<root-id>/` is the sibling of the existing
`state/holds/<root-id>/`, so one root's facts sit together.

### The marker, and the stage 1 marker it replaces

`barge.root/1` records an absolute `resolved_path` and requires it to equal the
path actually walked. After a drive reconnects at a different mount point —
which macOS does routinely, appending " 1" when the old mount-point directory
survives — that field can never match again, and a correct root becomes
unverifiable. The frozen field is wrong for the invariant it has to serve, so
stage 4 writes `barge.root/2`:

```json
{
  "schema": "barge.root/2",
  "root_id": "<32 hex>",
  "nonce": "<32 hex>",
  "volume_uuid": "<uuid>",
  "owner": "barge",
  "kind": "area_root",
  "area": {"namespace": "oxide", "area": "outputs"},
  "mount_relative_path": "oxide/outputs/2026-10-26T08-15-00Z-3f9c",
  "mount_at_record": "/Volumes/Builds",
  "created_ms": 0,
  "generation": 1,
  "origin": "created",
  "conclusion": null
}
```

Every field other than the path pair is `barge.root/1`'s, with the same meaning
and the same validation, and `generation` keeps [the meaning stage 4 gives
it](#marker-generation). A v2 marker's path fields are checked exactly as the
identity record's are.

`barge.root/1` markers keep working, with one bounded relaxation: its absolute
`resolved_path` is compared to the live path after replacing only its mount
prefix with the verified live mount of the recorded UUID, and every component
below that prefix must still match exactly. A root at a different relative path,
or on a different volume, is still unknown.

That bridge is live from the first day, not hypothetical: S3 has landed a
`barge.root/1` writer, and its marker validation compares `resolved_path` to the
path it walked (stage 1, "Implementation status"), so every root S3 creates on a
drive that later reconnects at another mount point would otherwise be
unverifiable. No stage-4 code writes a v1 marker; it is read by the rule above
until the root is recreated or adopted, which is [O4-003](#open-design-issues).

#### Marker generation

Stage 1 gives the marker a `generation` field without saying what it counts. In
stage 4 it counts legitimate writes of that marker: `1` at creation and, for a
`scratch_run` root only, `2` after its conclusion. Any other value, a conclusion
on a root whose `kind` is not `scratch_run`, or a missing conclusion on a
generation-2 marker is unknown and the root is kept. This closes an
under-specified field with the smallest rule that keeps "unknown is never
eligible" true.

### Verifying a root

Verification is descriptor-relative from the verified mount root and re-runs on
every use; a stored verdict is never trusted. The steps:

1. Resolve the volume ([finding the volume](#finding-the-volume-after-a-remount))
   and refuse when it is absent, mismatched, duplicated or not APFS.
2. Open the mount root descriptor, and walk `mount_relative_path` component by
   component with `openat(…, O_DIRECTORY | O_NOFOLLOW)`; each component must be a
   real directory of this user and on the mount descriptor's device. A symlinked
   component, a cross-device component, a missing component or an entry that
   changed between `fstatat` and `openat` is unknown, not absent.
3. `fstat` the root through the walked descriptor: it is a directory of this
   user, its device equals the live mount's device, and its inode equals the
   record's.
4. Read `.barge-root.json` through a no-follow descriptor: at most 4 KiB, a
   regular file of this user with private mode, strict schema for its version,
   `owner: barge`, `root_id`, `nonce`, `volume_uuid`, `kind` and `area` equal to
   the record's, `generation` per [its rule](#marker-generation), and its path
   fields equal to the record's and to the walk.
5. Report the verdict and the evidence:

```text
Verified root                     the identity condition holds
  remount: {device_at_record, device_live, changed: bool}
  mount_moved: bool               the live mount differs from mount_at_record
Unknown(reason_code)              keep: never reusable, never deletable
Gone                              the volume verified and the recorded path is ENOENT
```

Reason codes, each a distinct row in the [matrix](#failure-race-and-cancellation-matrix):
`volume_unverified`, `duplicate_volume_uuid`, `not_a_real_directory`,
`path_changed`, `inode_changed`, `marker_absent`, `marker_unreadable`,
`marker_foreign_owner`, `marker_version_unknown`, `marker_schema_divergent`,
`marker_identity_mismatch`, `marker_generation_unexpected`,
`identity_record_missing`, `identity_record_invalid`.

A device change alone is **not** a refusal: it appears as `remount.changed` with
both numbers, and the verdict stays `Verified`. A mount-point change with the
same UUID and the same relative path reports `mount_moved` and stays `Verified`.
Both are reported on every plan row, so an operator can see what a remount did
without reading a log.

`Gone` means only "the volume is verified and this path does not exist". It is
never reached through an unverified volume: an absent drive, an unreadable
ancestor or a duplicated UUID is `Unknown`, so "the root is gone" and "I cannot
see the root" never read the same. A `Gone` root keeps its identity record, is
reported as `root_missing`, and never has that record deleted automatically.

### Finding the volume after a remount

The registry's `mount_hint` is used to look first, never as identity (stage 1
rule). After a reconnect the hint may be stale, so stage 4 needs one fact the
`os` layer does not yet expose: the live mounts that carry a given APFS UUID.

```rust
/// The mounts whose volume UUID matches, read now. Bounded and read-only.
pub trait VolumeLocator {
    /// Every live mount whose kernel APFS UUID equals `uuid`, at most 64.
    fn mounts_for_uuid(&self, uuid: &VolumeUuid) -> Result<Vec<MountPoint>>;
}
```

The platform implementation enumerates mounted filesystems (`getfsstat`), keeps
the candidates that are real directories directly under `/Volumes`, reads each
candidate's `ATTR_VOL_UUID` through its own descriptor and returns the matches.
`diskutil` output, `mount` output, and any other external process's text is
never identity (stage 1's rule about `lsof` applies to every such tool).

From that list the rule is one line and it is strict:

- **exactly one candidate** → verify it as stage 1 already does (`verify_volume`
  against the expected UUID), then proceed;
- **no candidate** → `volume_absent` for an ephemeral volume, reported loudly for
  a permanent one; nothing is created, deleted or recreated;
- **more than one candidate** → `duplicate_volume_uuid`: `unknown_state`, exit
  `3`, every root on that UUID is kept. Barge never picks a mount, and never
  merges facts from two mounts. This is the case a block-level clone of a drive
  produces.

The boot volume is refused as a managed volume by the stage 1 check and stays
refused; it appears in stage 4 only as the absence proof's
[ancestor volume](#the-ancestor-volume-proof), where it is read, never written.

### Where identity is written, and what happens when it is missing

A root is created in one durable step, in this order, all under the exclusive
root lock:

1. create the directory exclusively (`mkdir` with `0700`, `O_NOFOLLOW` handles);
2. `fstat` it through the new descriptor to read `inode` and `device_at_record`;
3. write `.barge-root.json` atomically (temporary name, `fsync`, `rename`,
   `fsync` the directory);
4. write `state/roots/<root-id>/identity.json` atomically, and `fsync` its
   parent;
5. only then may a consumer write payload.

Adoption of an existing directory (`held-root`, stage 1 S6) takes the same steps
with `origin: adopted_existing`.

S3's landed `create_root` stops after step 3 and returns the identity facts to
its caller without recording them, which is why F1 amends that one path: the
record is added where the facts are already read, not reconstructed later from
the marker. Roots S3 has already created — and a root whose creation crashed
between steps 3 and 4 — therefore have a valid marker and no identity record.
Such a root is `Unknown(identity_record_missing)`: never released, reported by
every plan, and never given an identity automatically — deletion
authority must never appear by observation. How, and whether, a human may give
it one is [open design issue O4-002](#open-design-issues). A root in that state
may still be deleted by the [stage 3 space prune](#how-this-gate-combines-with-the-stage-3-space-prune),
which carries its own observation; it is never *released* by this gate.

### No live identity rewrite

There is no code path that writes `identity.json` twice. A verdict never updates
it, a remount never updates it, and a `Gone` root never clears it. Two
consequences are accepted deliberately:

- a root whose inode changed can only ever be kept, reported and deleted by hand
  or by a future, explicitly reviewed repair operation;
- the census keeps showing the number of roots recorded at one device and used at
  another, which is the evidence the carried finding needs. Rewriting the record
  would erase that evidence, and it would bless a change that no code proved was
  safe.

### Lock order with the stage 1 locks

Stage 1's order is registry → volume → root → intent, enforced by
`state::LockSet`'s checked rank, and the [stage 3 space policy](stage3-space-policy.md#lock-order-with-the-stage-1-locks)
already adds the rule that matters most here: **one prune per volume**, with
`volume-<uuid>.lock` held exclusively for a prune pass and only the root lock
released between candidates. This contract adds no rank; it uses that shape for
the release pass and says which lock each step holds:

| Step | Locks held | Rule |
| --- | --- | --- |
| Mount enumeration, absence proof, VolumeLocator | none | proves facts about the world outside Barge's state; it takes no lock and is never cached across a run |
| Identity verification inside a preview | volume shared, then one root shared at a time | released before the next root; two roots are never held at once |
| Plan build (one area, preview) | registry shared, released; then volume shared, then one root shared at a time | the registry lock is never held across a directory walk or a delete; a registration write is never blocked by a preview |
| Identity write (creation, adoption, an operator repair) | volume shared, root exclusive | it is a root-lock-protected write and takes no other rank |
| A real release run | registry read and released; then `volume-<uuid>.lock` **exclusive** from the first re-verification to the final settlement; one root exclusive at a time; then the intent write last | the same serialisation stage 3 uses, so a release run and a space prune on one volume cannot interleave; the intent stays the last rank, so a pending intent is always the last durable record |

Three consequences worth stating, because a reader will check them:

- two runs on one volume cannot overlap at all, in either direction: the second
  waits within its own run deadline and then refuses `locked`, naming the lock
  file, having changed nothing;
- a stale plan that reaches its effect after another run already acted still
  finds that root's pending intent and skips that root, so serialisation plus the
  journal are both needed and neither replaces the other;
- no step ever takes a root lock and then asks for the volume lock: a consumer's
  `hold` takes the root lock alone, and a prune takes the volume first, so the
  only wait either side does for the other is the one stage 3's rule 5 describes.

## Absence: a consumer path that is positively gone

Identity answers "is this the directory I recorded". It does not answer "does
anyone still want it". Oxide's rule needs both: a released root is prunable only
when the checkout that used it is positively absent. Stage 4 carries that rule
as a generic primitive so no consumer invents its own.

### The proof

An absence proof is a bounded, descriptor-relative walk that concludes one of
three verdicts about one absolute path:

```text
Absent(proof)      the path is positively gone
Present            the path exists
Unknown(reason)    cannot be decided; never treated as absent
```

`Absent` requires all four of:

1. **A proven anchor.** For a path at or below `/Volumes/<mount>`, the anchor is
   the verified mount root of the volume the record names, found by UUID
   ([finding the volume](#finding-the-volume-after-a-remount)) — never the path
   text, never a stale mount-point directory. For any other path the anchor is
   `/`.
2. **A no-follow walk from the anchor.** Each component is opened with
   `O_DIRECTORY | O_NOFOLLOW` and must be a real directory (a no-follow
   `fstatat` says `S_IFDIR`). The `fstatat` reading and the descriptor `openat`
   returns are compared on device and inode, so a component replaced between the
   two calls is `Unknown(replaced)`. A symlinked component, an unreadable
   directory (`EACCES`), a depth-bound or deadline hit is `Unknown`.

   The walk makes **no ownership claim and no device claim on its components**,
   and both omissions are deliberate:

   - `/Users`, `/private` and the other ancestors of a consumer path are
     root-owned by design on macOS, so requiring this user's ownership on a
     component would make every `/Users/...` checkout permanently undecidable —
     a false refusal, and exactly the class of defect the carried finding is
     about. Ownership is a property of the *root* ([verifying a
     root](#verifying-a-root), which does check it).
   - `/` is the read-only system volume, and `/Users`, `/private` and
     `/Applications` reach the Data volume through firmlinks, so their device
     differs from the anchor's for every real checkout. A per-component device
     comparison against the anchor would refuse all of them.

   What proves the ancestry is local and current is therefore condition 4 alone —
   the volume proof — which is also how Oxide's `checkout_absence` is written:
   walk no-follow, decide `ENOENT` at the leaf, prove the volume separately.
3. **Only the leaf may be ENOENT.** A missing leaf gives `Absent`. A missing
   non-leaf component gives `Unknown(ancestor_missing)`: a renamed or deleted
   parent means the path may have moved, and a moved consumer path is not a
   deleted one.
4. **A verified nearest existing ancestor.** The leaf's parent — which condition
   2 has already proved to exist as a real directory, reached without following a
   symlink and not replaced since it was read — must be on a local, verified
   volume: [the ancestor volume proof](#the-ancestor-volume-proof). This is where
   "is this really the internal drive" is decided, and it is the only
   local-volume check the walk needs.

Anything else is `Unknown`, and `Unknown` is never absence, never idle, never
empty.

### Walk rules

| Situation | Verdict |
| --- | --- |
| The leaf is `ENOENT`; every ancestor existed and is a real directory | `Absent` |
| A non-leaf component is `ENOENT` | `Unknown(ancestor_missing)` |
| A component is a symlink | `Unknown(symlink_component)` |
| A component is a regular file or other non-directory | `Unknown(not_a_directory)` |
| A component was replaced between its `fstatat` reading and its `openat` (device or inode differ) | `Unknown(replaced)` |
| A component cannot be read (`EACCES`) | `Unknown(not_readable)` |
| The path is relative, empty, has no component below the anchor, or contains `.`/`..` | refused before the walk (`usage`) |
| The walk reaches its 64-component bound | `Unknown(depth_bound)` |
| The walk reaches its 1 s deadline | `Unknown(deadline)` |
| The anchor volume is absent, unverified, duplicated or not APFS | `Unknown(volume_unverified)` |
| The path is present | `Present` |

### The ancestor volume proof

The ancestor must be on a volume that can be *proved* local and current. Two
kinds are allowed:

- **`boot_data`** — the internal drive, proved live on every proof: read
  `/System/Volumes/Data`'s device and APFS volume UUID, and require the
  ancestor's device and UUID to equal them. A path under `/Users`, a checkout in
  a home folder, and anything else on the internal drive resolve here.
- **`registered(uuid)`** — a volume Barge has registered, proved by the same
  verified-mount route a root use takes, with the ancestor's device equal to the
  live mount's device and its UUID equal to the record's.

Every other outcome is `Unknown`: no mount (the volume is away), a stale
mount-point directory, a network or autofs mount, a filesystem that is not APFS
or HFS, a foreign mount at a registered mount point, a duplicated UUID, an
unreadable mount table. On macOS a removable ancestry that is not mounted is
exactly the "unknown, not absent" case the invariant names, and this is where it
is enforced.

The boot Data volume's device and UUID are read from the kernel on every proof,
never recorded: they are facts about this boot, and recording them would be the
same mistake the carried finding is about.

### The proof document

```json
{
  "schema": "barge.absence/1",
  "path": "/Users/tom/Dev/projects/example",
  "verdict": "absent",
  "anchor": {"kind": "root", "path": "/"},
  "ancestor": {
    "path": "/Users/tom/Dev/projects",
    "inode": 1234567,
    "device": 16777220,
    "volume": {"kind": "boot_data", "volume_uuid": "<uuid>", "file_system": "apfs"}
  },
  "leaf": {"name": "example", "result": "enoent"},
  "components_walked": 4,
  "proved_ms": 0
}
```

Bounds: 4 KiB, at most 64 components, at most one descriptor chain open at a
time. The document is evidence, not authority: it authorizes nothing until it is
re-established at the effect step. A plan carries it so a human can read why a
root was eligible; an intent carries it so a replay can say what was proved and
when. Neither is a cached permission, and neither is ever reused as a proof.

The `Present` and `Unknown` forms carry the same fields with `verdict` changed
and, for `Unknown`, a `reason` code from the table above.

### The consumer reference: what makes a root releasable

Absence proves something about a path. It says nothing about whether a consumer
still wants the root, so a root is releasable only when *every* reference that
names it is not current:

- **Barge's own references**: a placement record, an area record, or a hold. A
  placement whose original path is this root is current; a hold is current.
- **Foreign references, read in place** (Q-004): Oxide's receipt for a checkout
  selects at most one root per member; Oxide's registry rows carry `selected`,
  `pruning`, `reservation` and `cancelling` flags; Oxide's root marker names the
  `checkout` and `checkout_id` that own it. A row that is selected, reserved,
  pruning or cancelling is current.
- **The absence requirement** applies to the reference, not to the root: when a
  reference's own rule is "the consumer path is gone", that path needs a
  `barge.absence/1` proof of `Absent`. When a reference's rule is only "no
  longer selected", no absence proof is involved.

A reference set that cannot be read completely is `Unknown`, and the whole
batch it belongs to is kept — the same rule Oxide applies to an incomplete
receipt ("keeps the whole snapshot batch, not just that row").

### Oxide compatibility, read in place

Barge's primitive is a generalisation of the rule Oxide implements today, and it
is deliberately compatible in both directions.

| Oxide fact | Barge's equivalent | Note |
| --- | --- | --- |
| `/Volumes/<mount>/…` checkout with a recorded `checkout_volume_uuid` | anchor = the verified mount of that UUID; walk the components below it | Barge finds the mount by UUID, so a reconnect at another mount name still resolves |
| a recorded checkout outside `/Volumes` | ancestor on `boot_data` (the internal drive) or on a `registered` volume | Oxide's registered-row rule, generalised: Barge accepts any registered volume it can verify, not only APFS or HFS by name |
| `--operator-list` | the same ancestry rule: ancestor on `boot_data`, only the leaf may be `ENOENT`; Barge carries no operator list | identical on absence: at the pin the list waives **receipt-history completeness** for the named legacy roots only, and their checkout's no-follow walk must still end in a leaf `ENOENT`. Barge needs no waiver because it keeps any root whose reference set is incomplete |
| only the leaf may be `ENOENT`; a missing non-leaf ancestor is uncertainty, never absence | the same, for the same reason | identical |
| no ownership or device check on a walk component; the volume proof is the local-volume check | the same | identical, and required: `/Users` and `/private` are root-owned and reach the Data volume through firmlinks, so either check on a component would refuse every real `/Users/...` checkout |
| `Absent` requires the root's own identity, custody and no current selection | the same, as the [gate](#releasability-the-one-gate) | identical |
| `diskutil`/`statfs` text | never identity; kernel facts through descriptors | stage 1's rule |

Rules that keep the handover safe:

- While Oxide prunes, **Barge deletes no Oxide root.** The primitive is
  implemented and proven against Barge-owned roots first; Oxide's adoption is
  its own slice with its own brief (stage 1 S8, `F6` below).
- Barge never writes an Oxide marker, receipt, registry row or tombstone. It
  reads `oxide.outputs.root/2` markers, `oxide.outputs.registry/2` rows and the
  receipts strictly, bounded, and reports them.
- Where Oxide's own rule is stricter than Barge's — the complete-receipt
  requirement, and the operator list that can waive it for named legacy roots —
  Oxide's rule wins while Oxide owns the prune.
- Where the two rules agree, they agree exactly: leaf-only `ENOENT` with a missing
  non-leaf ancestor kept on both sides, the boot Data volume proof for a legacy
  checkout outside `/Volumes`, and the root's own identity, custody and current
  selection checks. The one difference is the receipt-completeness waiver, which
  Barge does not carry, so Barge keeps a root whose reference set it cannot read
  completely where Oxide may prune it under an operator list — the safe direction
  for a handover in progress.
- The 91 legacy roots held by `ff13f2ef` stay held: they need an operator list in
  Oxide and an adoption decision in Barge, neither of which this contract
  authorizes. Oxide's own implementation of the absence rule (`40010b40`) stays
  in Oxide, and this contract's reading is compatible with it.

### Reading the census without adopting anything

The carried finding needs a number, and answering it must not require adopting a
single root. `barge inspect` already reads Oxide's state in place and reports
observations; stage 4 adds one bounded, read-only observation:

```text
<internal pool>/outputs/registry.json   oxide.outputs.registry/2 (and /1)
```

with, per row: the recorded root path, `volume_uuid`, `nonce`, `generation`,
`root_dev`, `root_ino`, `marker_digest`, the selection flags, and — where the
recorded volume is present and verifiable — Barge's identity verdict computed by
[the same steps](#verifying-a-root), reported as
`{verdict, inode_equal, device_equal, recorded_device, live_device}`.

That row is exactly what separates "the device moved" from "the directory
changed", on the real drive, without a write anywhere. Nothing in it becomes
authoritative, no marker is read beyond one bounded 64 KiB read per root, and a
row whose volume is away reports `Unknown(volume_unverified)` rather than being
skipped. Reading this file is an extension of stage 1's "presence and version
only" boundary for Oxide's registry; it is [open design issue
O4-006](#open-design-issues) because it widens a read boundary, and it is the
one thing in this contract that the operator may want to see decided before F2.

## Releasability: the one gate

A whole owned root is released only when all six conditions hold, each re-proved
at the effect step, each with `Unknown` meaning keep:

| # | Condition | Owned by | `Unknown` means |
| --- | --- | --- | --- |
| 1 | The volume is verified: APFS, not boot, not duplicated, mounted once | stage 1, [finding the volume](#finding-the-volume-after-a-remount) | keep |
| 2 | The root's [identity verdict](#verifying-a-root) is `Verified` | this contract | keep |
| 3 | Custody is `Safe` (holds released or positively stale; same-uid scan bounded and empty) | stage 1, "Custody protocol" | keep |
| 4 | The area's declared `PrunePolicy` allows it and the volume's `durability` is `expendable`; age comes from [last activity](#last-activity) | stage 1, "Area declaration API" | keep |
| 5 | Every reference naming the root is not current, from a complete reference set | this contract, [the consumer reference](#the-consumer-reference-what-makes-a-root-releasable) | keep the batch |
| 6 | Every reference whose own rule requires absence has an `Absent` proof, re-established now | this contract | keep |

These six are the **release** question: is this root still wanted? Conditions 1,
3 and 4 are shared with the stage 3 space prune, which supplies condition 2 from
its own same-run observation and asks neither 5 nor 6 — [how the two combine](#how-this-gate-combines-with-the-stage-3-space-prune)
is part of this contract, not a caveat to it.

There is no seventh condition and no override. An operator command that names one
root explicitly still satisfies all six; it removes only condition 4's
`PrunePolicy` age gate, never the `expendable` durability requirement (stage 1's
rule for an adopted existing root), and it must say so in its plan.

### Last activity

`OldestIdleAfter` needs an age. Stage 4 fixes it, because a wrong clock makes a
live root prunable:

- the age is `now − last_activity_ms`;
- `last_activity_ms` is the newest modification time found by a bounded scan of
  the root, and never an entry's birth time, a marker's `created_ms` alone, or a
  cached value from an earlier run;
- a root with no scannable entry uses the marker directory's own modification
  time as its floor;
- a time in the future, an unreadable subtree, or a scan that hits its bound is
  `Unknown`, so the root is kept;
- a live hold is not required for idleness: a hold makes the root ineligible
  through condition 3, and idleness is a separate fact.

A consumer that wants a different clock (an `area_root` whose contents are
written once and read for weeks) declares its own policy; Barge does not infer
one. Which clock Oxide's roots should get is [open design issue
O4-005](#open-design-issues).

### Plan and intent

`barge prune --plan` (one area) produces `barge.prune.plan/1`; `barge maintain`
composes those plans into `barge.maintain.plan/1`, whose field set stage 4
completes with a `prune` array and the per-area plan digests. Both are canonical
JSON, both have a SHA-256, and a real run must present the exact bytes and
digest. Every row carries the six conditions with their evidence:

```json
{
  "schema": "barge.prune.plan/1",
  "area": {"namespace": "oxide", "area": "outputs"},
  "volume_uuid": "<uuid>",
  "rows": [
    {
      "root_id": "<32 hex>",
      "mount_relative_path": "oxide/outputs/2026-09-01T10-00-00Z-77ab",
      "identity": {
        "verdict": "verified",
        "inode": 1234567,
        "device_at_record": 16777234,
        "device_live": 16777890,
        "remount": {"device_changed": true},
        "mount_moved": false,
        "marker_digest": "<64 hex>"
      },
      "custody": "safe",
      "idle": {"last_activity_ms": 0, "age_seconds": 864000},
      "references": [
        {"source": "oxide_receipt", "current": false},
        {"source": "oxide_registry", "current": false}
      ],
      "absence": {"path": "/Users/tom/Dev/projects/example", "verdict": "absent"},
      "bytes": {"logical": 0, "allocated": 0, "complete": true},
      "eligible": true
    }
  ],
  "eligible_count": 1,
  "examined_count": 12,
  "truncated": false
}
```

A row that is not eligible carries `eligible: false` and its first blocking
reason, and every row carries its evidence whether or not it is eligible —
that is what makes one plan a diagnostic as well as a permission.

`barge.intent/2` gains two optional objects, `identity` and `absence`, holding
what was proved at plan time and what was re-proved at effect time, so a replay
after a reboot has the evidence it must re-establish rather than a permission it
must trust. S5 has landed that record, and its barge#016 follow-up moved the
major to `/2` when custody and deletion were generalised from a Barge-marked
root to a deletion unit; `barge.intent/1` stays readable and maps to a
Barge-marked directory unit. The unit object carries the within-run observation
(`device`, `inode`, the optional marker digest) and `plan_digest` stays the
permission; the two objects are additions inside major 2, applied to a
directory unit's identity the same way, and F4 updates S5's strict reader in the
same slice, so a document written by an older binary still reads while the new
fields are understood by both.

## Formats and versions

| Document | Version | Where | Stage 4 change |
| --- | --- | --- | --- |
| `barge.root` marker | `/2` written, `/1` read | `.barge-root.json` in each owned root | path pair becomes mount-relative plus a mount hint; `generation` defined |
| `barge.root.identity` | `/1` | `state/roots/<root-id>/identity.json` | new |
| `barge.absence` | `/1` | in a plan row and an intent | new |
| `barge.prune.plan` | `/1` | `barge prune --plan` | new: the *release* plan for one area, with identity, custody, idle, reference and absence evidence. Distinct from the stage 3 space plan, `barge.space.prune/1`, which selects by measurement, participation and least-recently-used order and needs no absence proof |
| `barge.maintain.plan` | `/1`, fields completed | `barge maintain --plan` | adds the `release` array and per-area plan digests; the space stage's own rows are the stage 3 design's |
| `barge.intent` | `/2`, landed in S5 (with `/1` still read); optional fields added | `state/intents/<intent-id>.json` | adds `identity` and `absence`; the unit's `device`, `inode` and optional marker digest stay the within-run observation |
| `barge.describe` | `/1`, optional fields added | `barge describe` | adds `identity.capability` (what may be compared across a remount) and `volumes[].mounts` (how many live mounts carry each registered UUID) |
| `barge.inspect` | `/1`, optional fields added | `barge inspect` | adds the Oxide registry observation and the per-row identity census |
| `barge.prune` | `/1` | `barge prune --plan` | the command document around the plan, as `barge.maintain/1` wraps `barge.maintain.plan/1`: the plan, its SHA-256, when it was planned, the volume's state and whether the Oxide reference snapshot was complete |

The versioning rules are stage 1's: adding an optional field stays inside a
major; removing one, changing a type or a meaning is a new major.
`barge.prune.plan/1` landed in F3 with the field set this contract fixes, so the
reader's "exactly this field set" rule holds from the first write; the concrete
row fields F3 completed are recorded in [F3's own note](#f3-landed-surface).
`barge.maintain.plan/1` landed in S7 without the release fields this contract
adds to it; a `release` array there is still F4's, because it can only name what
a release plan proved.
`barge.intent/2` has landed in S5, so its additions are optional fields with the
reader updated in the same slice, and an intent written before F4 (a `/2`
record, or a `/1` record mapped on read) stays valid and readable.

One naming rule, because two plans now exist: `barge.prune.plan/1` is the
**release** plan — the evidence that a root may be given up because nothing
references it and its consumer path is positively gone. `barge.space.prune/1`
is the stage 3 **space** plan — the evidence that a root is idle, expendable,
counted by its area and old enough to reclaim for room. Neither substitutes for
the other, and `barge maintain` runs the space stage and the release stage as
separate steps with separate digests.

One word, two facts, so the documents never get confused: `barge.absence/1` is a
proof about a **path** being gone, while stage 3's `volume_absent` error kind is
a proof about a **volume** being away. A present volume with an absent path and
an absent volume with an unprovable path are different states with different
consequences, and no rule in either contract converts one into the other.

## Reserve, floor and the space prune belong to stage 3

The [stage 3 space policy](stage3-space-policy.md) owns the space measurement and
its limits, the registration of the reserve and the floor, the reserve prune
trigger, area participation, idleness, least-recently-used order, the
concurrency rules under consumer holds, and the `barge.space/1`,
`barge.space.guard/1` and `barge.space.prune/1` formats. This contract owns
identity, absence, the reference check and the release gate, and consumes stage
3's trigger rather than setting one: nothing here may be read as authorizing a
reserve or floor value, and no rule here changes when a space prune runs or what
it may reclaim.

Where the two meet is one question — may this whole root be deleted? — and
[the next section](#how-this-gate-combines-with-the-stage-3-space-prune) says
exactly which conditions each path supplies.

## How this gate combines with the stage 3 space prune

Two paths delete an owned root, and they answer different questions:

- a **space prune** answers "is this root idle and expendable, and does the
  volume need the room?" Its evidence is stage 3's measurement, participation
  and order.
- a **release prune** answers "is this root still wanted?" Its evidence is this
  contract's reference check and absence proof.

They share conditions 1, 3 and 4 of [the gate](#releasability-the-one-gate): the
volume is verified, custody is `Safe`, and the area's policy and durability
allow a delete. They differ in two places, and the difference is deliberate:

- **Identity.** The release path requires the durable record
  (condition 2 as written), because its decision can outlive the observation: a
  reference set is read, an absence proved, and a plan may be kept, replayed or
  settled after a reboot. The space path instead carries a *live* observation —
  device, inode and marker digest — from its plan into its intent and re-verifies
  it at every step (stage 3, "Pruning under the reserve"), so its identity check
  is made from a fact taken in the same run and never compared to a stale one.
- **References and absence.** Conditions 5 and 6 are the release question and
  apply only there. A space prune reclaims a root the area already declared
  expendable and idle; it does not claim the consumer is gone, and it must not
  read an absence proof as permission.

The invariant both paths share, and the reason neither is a weakening of the
other: **no delete happens without an identity check made from a fact that cannot
change behind the decision.** For the release path that is the durable record
plus a same-run re-read of the marker and the inode; for the space path it is a
same-run observation re-verified at the effect. A space prune never settles a
pending release intent, and a release run never reclaims for room.

Because the record is this contract's, making the space path honour it is F1's
addition rather than a change to stage 3's evidence rules: F1 adds one check to
the space prune's effect, "if a durable record exists it must verify and agree",
and a disagreement keeps the root on both paths. The [stage 3 space
policy](stage3-space-policy.md#pruning-under-the-reserve) states that check in
its own custody list, so neither contract silently widens the other.

## Failure, race and cancellation matrix

Every row is a rule the implementation must carry a test for. "Keeps" means the
data, marker, identity record and any pending intent are left unchanged and
reported. Stage 1's matrix still applies; these are the rows stage 4 adds.

| Trigger | Detection | Action |
| --- | --- | --- |
| Volume remounted since the record was written | live device differs from `device_at_record`, everything else equal | `Verified` with `remount.device_changed: true`; report both numbers; proceed |
| Volume reconnected at a different mount point | the live mount differs from `mount_at_record`; the UUID and relative path match | `Verified` with `mount_moved: true`; the record is not rewritten; the stale hint is reported |
| The recorded root directory was replaced | inode differs, marker otherwise matches | `Unknown(inode_changed)`; keep; report both inodes |
| An inode was reused for a different root | inode equal, marker `root_id`/`nonce` differ | `Unknown(marker_identity_mismatch)`; keep |
| A marker was rewritten outside a conclusion | marker `generation` or conclusion does not fit its `kind` | `Unknown(marker_generation_unexpected)`; keep |
| A conclusion races a prune | the marker digest read now differs from the plan's | abandon that root's effect before any rename; keep; the rest of the plan continues |
| Two live mounts carry one UUID | mount enumeration returns more than one | `duplicate_volume_uuid`: refuse, exit `3`, every root on that UUID kept, never pick one |
| One volume mounted twice at two mount points | same UUID, two candidates | same as above: a decision between two mounts is never made |
| A cloned volume is attached alone | nothing distinguishes it from the original | [O4-001](#open-design-issues): accepted and named; the clone carries only expendable owned roots and its own references |
| A different volume at the registered mount point | UUID mismatch at verification | `volume_mismatch`; never used, never deleted (stage 1 rule kept) |
| A stale mount-point directory exists and nothing is mounted | the anchor will not verify as a mount root of the UUID | `Unknown(volume_unverified)`; never written through, never recreated |
| The volume is absent while a plan is built | no candidate mount | ephemeral: report and skip; permanent: report loudly; no `Gone`, no delete |
| The volume is absent at the effect step | mount verification fails | pending intent if one was already written, else refuse; exit `3` / `2`; nothing deleted |
| The root disappeared while the volume verifies | the recorded path is `ENOENT` | `Gone`: report `root_missing`, keep the identity record, nothing to delete |
| The root disappeared while the volume cannot be verified | no verified mount | `Unknown`, never `Gone` |
| Drive pulled mid-delete after a remount | a descriptor-relative call fails after the quarantine rename | keep the pending intent and the quarantine leaf; exit `3`; replay only after the recorded identity re-verifies, with the changed device tolerated |
| A consumer path reappears between plan and effect | the re-established proof returns `Present` | keep that root, abandon its effect, continue the plan |
| A consumer path's ancestor changed between plan and effect | the re-established proof returns a different ancestor inode or volume | re-prove from scratch; if it does not prove `Absent`, keep |
| A non-leaf ancestor of a consumer path is missing | `ENOENT` above the leaf | `Unknown(ancestor_missing)`; keep |
| A walk component changed between its `fstatat` reading and its `openat` | the reading and the descriptor disagree on device or inode | `Unknown(replaced)`; keep |
| A consumer path's ancestor is a symlink, a foreign mount, a network or autofs mount, or a not-APFS volume | walk or volume proof | `Unknown`; keep |
| A consumer path's ancestor volume is a removable volume that is away | no candidate mount | `Unknown(volume_unverified)`; keep |
| A duplicate `barge.root/1` and `barge.root/2` root sit at one path | the v2 reader finds the marker it expected; a v1 marker where v2 was recorded, or the reverse, is a version mismatch | `Unknown(marker_version_unknown)`; keep both until the operator resolves it |
| A durable identity record disagrees with the space prune's live observation | stage 3 re-verifies `device`, `inode` and `marker_digest` at its effect and this contract's record is present | keep the root, report both, and skip it for that run; the space prune deletes nothing the record contradicts |
| A space prune is reclaiming a root while a release plan names it | one root lock at a time, intent last | the second sees the first's pending intent and skips that root; neither settles the other's intent |
| A reference set is incomplete | an unreadable, oversized or schema-divergent receipt or registry | keep the whole batch that names those roots |
| A hold is live, stale-but-undecidable, or foreign-uid | stage 1 custody rules | `Unknown`; keep |
| A hold's boot identity differs from this boot | all three of pid, start identity and boot identity disagree | the hold is positively stale; custody may be `Safe` |
| Clock skew, NTP correction or a restored snapshot puts activity in the future | `last_activity_ms` ahead of now | not eligible; `Unknown`; keep |
| An identity record is missing | marker valid, no `state/roots/<root-id>/identity.json` | `Unknown(identity_record_missing)`; keep forever; report on every plan |
| An identity record is unreadable, oversized or schema-divergent | strict bounded read | `Unknown(identity_record_invalid)`; keep |
| The state directory is on an unavailable volume | state resolution fails | refuse every mutating command; `describe` still reports (stage 1 `O-002`) |
| Two prunes run at once | `volume-<uuid>.lock` exclusive for a real pass, one root lock at a time, intent last | the second waits within its run deadline and refuses `locked` changing nothing; a stale plan that still reaches its effect sees the first run's pending intent on that root and skips it |
| Cancellation (SIGINT/SIGTERM) mid-plan | signal | stop after the current durable step; a plan has no effect to leave behind; exit non-zero |
| Cancellation mid-effect | signal after the intent write | leave the pending intent; exit non-zero; replay settles it |
| A bound is reached (entries, depth, deadline, descriptor chain) | the bound's own check | report what was not reached with counts; every unreached root stays unverified and kept |

## Bounded resources

Stage 1's table still applies. Stage 4's additions:

| Bound | Value |
| --- | --- |
| Identity record | 4 KiB, 1 s read deadline, `deny_unknown_fields` |
| Identity records per state directory | 65 536 files under `state/roots/`, 10 s to scan |
| Mount enumeration | 256 entries, 1 s, at most 64 candidates for one UUID |
| Live mounts per registered UUID | exactly 1; more is `duplicate_volume_uuid` |
| Absence proof | 64 components, 1 s, at most 3 descriptors open, no directory listing |
| Absence proofs per prune run | one per reference that needs one, at most 4096 |
| Prune plan per area | 4096 roots examined, 200 000 names per listing, stage 1's 600 s run wall and 512 MiB owned scratch |
| Oxide registry observation | Oxide's own registry cap, one 64 KiB read per root path, 10 s |
| Marker and identity read per root | 4 KiB marker + 4 KiB identity, re-read at each step, never cached across a run |

A bound that is reached reports what was not reached, with counts, and never
reports the unverified part as eligible.

## Alternatives considered and rejected

| Alternative | Why rejected |
| --- | --- |
| Stop recording the device at all and compare only UUID, path and nonce | "Simple dropping" loses the two proofs a device gives: that a walk stayed on one filesystem, and that the descriptor a delete reaches is on the verified volume. A cross-device rename or a bind of another filesystem into the path would go unnoticed. The device stays, with a narrower job. |
| Keep comparing `st_dev` and treat a change as unknown | This is the carried finding: a fact that changes on every remount becomes a permanent refusal, and the refusal looks like a safety property while it is a false negative. |
| Rewrite the record when the device changes ("repair on observe") | A write during verification would also bless an inode change that happened in the same window, and it would erase the evidence the finding needs. Identity is written once, by the step that creates the root. |
| Rewrite the record when the inode changes, after custody and a marker match | An inode change is exactly the event that must keep a root: the directory verified is not the directory recorded. Making it repairable turns a detection into a permission. |
| Put the inode in the marker | It makes the marker self-referential, couples the marker's bytes to its location, and puts deletion authority in bytes that travel with the data — against stage 1's "only authoritative state authorizes an effect". |
| Identify a root by its path alone | A path is a name, not an identity: delete-and-recreate at the same path would reuse the authority of the old root. |
| Identify a root by its marker nonce alone | Sufficient against forgery, insufficient against replacement: a copied root carries the same nonce, and without the inode and the path there is nothing to say the directory is the one recorded. |
| Record the absolute path and require it to match forever | A reconnect can change the mount point, so an absolute path is not stable. The frozen v1 field is read with a bounded prefix rule and v2 records the path below the mount. |
| Find the volume by parsing `diskutil`, `mount` or `statfs` text | An external process's text is not a kernel guarantee and is unbounded (stage 1's `lsof` rule). The kernel's `ATTR_VOL_UUID` through an owned descriptor is. |
| Accept any mount that reports the UUID, ignoring duplicates | Two mounts of one UUID make "the volume" ambiguous; picking one silently is how a delete lands on the wrong copy. |
| Refuse any volume whose UUID was ever seen twice | A reconnect legitimately re-uses a UUID after the old mount is gone; only *simultaneous* candidates are ambiguous. |
| A per-volume receipt file on the drive as identity | A block-level clone copies the receipt, so it adds nothing a UUID and marker do not already give. |
| Treat a missing ancestor as absence (the reaper's implicit behaviour) | A renamed or deleted parent means the consumer path may have *moved*, and its outputs are still live there. `/Users`-shaped examples are ordinary: a renamed project directory is not a deleted checkout. Oxide's `checkout_absence` refuses it too, and the reaper's silence is not a rule to inherit. |
| Require this user's ownership on every walked component | The ancestors of a real consumer path are root-owned by design (`/Users`, `/private`), so the rule would make every `/Users/...` checkout permanently undecidable. Ownership is checked on the root, where it means something; the walk proves containment and the volume proves locality. |
| Require every walked component to be on the anchor's device | `/` is the read-only system volume and `/Users`, `/private` and `/Applications` reach the Data volume through firmlinks, so a real checkout always crosses a device boundary at the anchor. The volume proof is the local-volume check instead. |
| Trust the plan's absence proof at effect time | The path can reappear between preview and effect, which is the ordinary case for a checkout being recreated. The proof is re-established. |
| Cache idleness or an identity verdict across runs | Both are facts about a moment. A cached verdict is a permission granted by the past, and the whole finding is what happens when a stale fact is compared to a live one. |
| Let an unverified root be adopted into identity automatically when its marker looks right | Deletion authority would then appear by observation, which is the one thing the design forbids. It stays an explicit operator action (O4-002). |
| Prune from the volume side using markers only, without account state | Stage 1 fixes the opposite: only Barge's authoritative state authorizes an effect, and a marker is data that can be copied. |
| Give Barge its own copy of the "is this checkout absent" rule per consumer | The rule is one rule; two copies drift, and Oxide's copy is the one the carried finding came from. It moves into Barge as a primitive with an Oxide-compatible reading. |
| Keep reading Oxide's prune registry only as "presence and version" | That leaves the carried finding undecidable without adopting roots. One bounded read-only observation answers it, at the cost of a widened read boundary that is named as O4-006. |

## Implementation slices and dependencies

Finite, in order; each slice lands with its tests and its documentation update.
No slice authorizes a native run, a host change, a data move or an Oxide change
on its own.

| Slice | Content | Depends on |
| --- | --- | --- |
| F1 | `barge.root/2` marker, `barge.root.identity/1`, the identity record added to S3's landed `create_root` and to adoption, [verification](#verifying-a-root) with its verdicts and reason codes, `identity.capability` and the mount census in `describe` | S3 (area, markers, holds) |
| F2 | `os::VolumeLocator` (`getfsstat` + `ATTR_VOL_UUID`), [finding the volume](#finding-the-volume-after-a-remount) with its duplicate refusal, the `barge.root/1` prefix rule, the `barge.absence/1` primitive and its ancestor volume proof | S1 and S3's containment walk (`os`), and F1 only for the shared walk helper |
| F3 | The [release gate](#releasability-the-one-gate): the reference readers (Barge's own records, and Oxide's receipts, registry rows and markers read in place), `last_activity_ms`, the `barge prune --plan` verb, and `barge.prune.plan/1` with its digest and evidence rows | F1, F2, S5 (custody and delete) |
| F4 | `barge.intent/2` with identity and absence evidence, S5's intent reader accepting them, replay under a changed device, and the `barge.root/1`-to-`/2` settlement for a pending intent written by an older reader | F3, S5 |
| F5 | `barge inspect`'s Oxide registry census and the drift report; the read-only operation that answers the carried finding without adopting anything | F1, F2, S2 (`inspect`) |
| F6 | Oxide declares its output areas and switches its prune to Barge's gate; the 91 legacy roots are released with an operator list | F1–F5, the [stage 3 space policy](stage3-space-policy.md)'s slices, Oxide's own project, `ff13f2ef`, operator approval |

`F1` and `F2` can run in parallel; `F2` needs nothing from `F1` except a shared
descriptor-walk helper, which now sits beside S3's containment code either way.
`F5` is read-only and could land before `F3`, which is useful because it produces
the evidence for the finding while the gate is still being built. Reserve and
floor pruning is not a slice here: the [stage 3 space
policy](stage3-space-policy.md) owns it, and this contract contributes only the
identity, custody and policy conditions a space prune already supplies. Nothing
depends on a real drive; `F6` needs its own operator gate because it changes what
happens to data on a real volume.

### F2 library primitives

The OS layer exposes a bounded UUID mount locator and a unique-mount verifier.
A mount census that exceeds its table bound or changes during observation is
unknown; duplicate UUID mounts are unknown with `duplicate_volume_uuid`.
The v1 marker reader can compare paths after replacing the recorded mount prefix
with a verified live mount, retaining exact comparison of every lower component.

`os::prove_absence` walks held directory descriptors without following links,
checks each opened identity against the no-follow observation, and accepts only
an absent leaf whose parent is on the verified boot Data or registered volume.
Its `barge.absence/1` document is evidence for one observation; it must be
established again at effect time. The primitive does not itself authorize a
prune or change any consumer's state.

### F3 landed surface

The release gate is planned, not performed: `barge prune --plan --area
<namespace>/<area> --volume <uuid>` builds one `barge.prune.plan/1`, writes it
where `--plan-out` asks, and prints it inside `barge.prune/1` with its SHA-256.
The verb refuses every form that is not the plan, because the effect it would
imply needs F4's intent evidence, its replay and its separate authorization.

Condition 1 is the plan's own precondition rather than a row field: a plan exists
only for a volume that verified, the command document's `volume.state` records
that it did, and an attached volume that fails to verify is a refused command
rather than a plan whose rows all say "kept".

Condition 3's pending intents are scoped to the unit's own path, as the custody
protocol now says: an intent for this unit, an ancestor of it or something inside
it makes custody `Unknown`, while an unrelated unit's intent — on the same volume
or another — never blocks this row (Supervisor decision `698d9ba3`). One
undecidable object never blocks an unrelated one here either.

The row's identity is the **listed** root's, bound to its durable record: the
record is looked up by this root's id (the marker's for an adopted root, else the
directory's own name when that is a root id), and then its `mount_relative_path`,
its inode and its marker's root id, nonce, kind and area are compared against what
was just observed at this path under this root's lock. The comparison's live side
is observed, never supplied: the root's owner is the owner read from its opened
descriptor, compared with the account's effective uid, and the live volume UUID and
mount are the verified volume this root was found on (with the root's device
compared to the area folder's own descriptor), never the recorded UUID compared
with itself. A root owned by another account, or a root whose record names another
volume, is therefore `unknown` and kept. A record that names another
path is a `path_changed` mismatch — a copied root that reuses an original's id in
a second area is kept, never verified on the original's record — and the row
always reports the path it observed rather than the record's stored one.

What the rows carry. Each row is one root the area listing found, in listing
order, and it carries conditions 2 to 6 whether or not it is eligible:
`root_id`, `mount_relative_path`, `identity` (the durable verdict, its reason
code, both devices, the remount and mount-moved facts and the marker digest this
plan read), `custody` with its reason, `idle` (`last_activity_ms`, the observed
`age_seconds`, the declared age, and the reason the age is unknown when it is),
`references` with `references_complete`, `absence` (the `barge.absence/1`
document itself, when a reference requires one), `bytes` (the scan's logical and
allocated totals and whether it reached the whole subtree), and then `eligible`
with the first blocking condition and its reason. Lists inside the plan are the
contract's bounded `RowSet`s, so a list whose counts disagree with its rows is
refused on read. A root with more than 64 references is kept: a set the plan
cannot report in full is never a short set.

The plan carries stage 1's 600 s run wall (`PLAN_WALL`, with a 30 s stop margin):
the clock starts with the plan's own first work, so its setup — the registry, the
volume, the resolve, the enumeration and the Oxide snapshot — is inside the bound
rather than before it. An enumeration that consumes the wall leaves every listed
root unreached, and a wall spent during setup means the Oxide snapshot is not read
at all: it is reported unknown, never a complete empty one. No root is attempted
inside the window, so reaching it reports `timed_out`, counts the listed roots it
did not reach in `unreached_count`, and gives every one of them a kept row with
`blocked_by: timeout` and no observation. A timed-out plan forces
exit `3`, and every unreached root stays unverified and kept — the bound never
reports the part it did not see as eligible.

What the reference readers read, and where. Barge's own: every placement record
(the whole census, so a truncated one is `Unknown`), the area declaration the
plan is built under (current when its policy never releases a root), and the
root's hold records, which the caller reads under the root's own lock (a live or
undecidable hold is current; a positively stale one is not). Oxide's, read in place and never written: the outputs registry
`oxide.outputs.registry/2` and its `/1` predecessor, every checkout receipt
(`oxide.outputs.transition/1`, `/2` and `/3`, each read for its own selection
site), and one root marker `oxide.outputs.root/2` (a `/1` marker is unknown,
because its field set predates the nonce and the checkout volume). A registry
row that is selected, **pending**, pruning, reserved or cancelling is current:
that is Oxide's own `Row::live` set, which is one flag wider than this contract's
list, and the wider one is the safe direction. An unfinished prune intent in the
registry journal names its root too. A reference that is not current contributes
the consumer path whose absence the gate must prove, together with the volume
UUID its record holds. The control directory is found where S2 already looks for
Oxide's registration (`<pool>/oxide-managed`), and only a pool that holds an
`outputs/` folder is read.

What "unknown" costs. An unreadable, oversized, schema-divergent or
unknown-version registry or receipt makes the reference set incomplete, and an
incomplete set keeps every root it covers — the batch, not one row. The registry itself is read at its own field set: its
`revision`, `complete` flag and `created_by` are required, an incomplete registry
must carry Oxide's own `incomplete` record (and a complete one must not), each
registry row must state Oxide's selection flags (`selected`, `pending`) and its
required identity fields, and every root kind any of these records states — a
registry row's, a prune intent's, a receipt selection's, a history entry's, a
prepared placement's, a reservation's or a migration journal root's — must be one
of the two Oxide writes (`external` or `local`). Each unfinished prune intent must
also carry the root identity Oxide pins and one of Oxide's six phases. A document
that omits any of these, or states a root kind this build does not know, is a
schema divergence — never a settled row, an empty registry or "no references" —
and a root marker that omits a field Oxide always writes — or states a root kind
outside `external` and `local` — is unreadable for the same reason: a divergent
marker is not understood ownership, so the reference set it belongs to is
incomplete and its root is kept rather than qualified by its own absence proof. A
marker that was observed and is gone when its bytes are read — or whose own facts
changed across that read — changed under the observation and is unknown for the
same reason: a marker's absence is only ever concluded from one look that found
none. A receipt is read at its own version's shape: each of
`oxide.outputs.transition/1`, `/2` and `/3` must carry exactly the fields and the
shape that version writes, `state` must be one of Oxide's five states, and a
document that carries another version's shape or omits a required field
(`state`, `updated_ms`, a placement's `member`, a selection's `kind` and
`generation`, a version-3 `layout`) is divergent. The placements must be the ones
the layout holds — one `target` placement for a whole target, at least one
unique, sorted member placement for a profile — and the migration journal of a
version-3 whole-target receipt is read strictly: every field Oxide pins there is
required (its phase, generation, member set, link, the old link's text and
identity, the parent's identity, its stage and backup, its pinned old marker, its
`prior_receipt_sha`, the registry revision and both times, exactly as
`oxide.outputs.transition/3` writes them), its phase must be one Oxide writes, its
generation must be the one that phase can carry, its member set must be non-empty,
sorted and unique, its link must be the checkout's own `target`, its paths
absolute, its identities non-zero and its `prior_receipt_sha` a SHA-256, and its
root must carry the 128-bit nonce; the root the journal names is the one the
checkout keeps while the move is in progress, so it is a current reference. An
arbitrary non-null journal fails these checks and is divergent, never an empty
complete receipt, and neither is a journal with one pinned field removed. The
registry's own unfinished prune intents are read the same way: every field Oxide
pins is required (its id, full root identity, phase, quarantine leaf, plan digest
and both times), its `version` must be exactly the `oxide.outputs.prune-intent/1`
format this build reads — **an unknown intent version is a divergence, never a
settled move** — its phase must be one Oxide writes, and its quarantine leaf,
digest, root identity and marker digest must be usable. A
divergent journal of either kind makes the shared snapshot incomplete, and an
incomplete shared source keeps the **whole batch** it covers: no other root of
that plan is eligible. Ownership must agree as well: a receipt or registry row
that carries the marker's checkout id but names a different checkout path
contradicts the marker, so which checkout owns the root is unknown and the root is
kept. A root that carries only an Oxide marker and no still-selecting record is
**not** releasable on the marker's own word: the marker is ownership, not a
selection, so its row is not current and its consumer path needs an `Absent`
proof of its own. So does an uncertain *discovery* or an interrupted
*census*: Oxide's own pools record can name the only pool that holds its
`outputs/`, so an unreadable or divergent pools record, a candidate `outputs/`
that exists and cannot be inspected, and two control directories that both hold
outputs all make the snapshot unknown rather than empty. The same holds inside the
receipt folder: a metadata error is never collapsed into "absent" (only `ENOENT`
is), a folder that exists and is not a directory is unknown, and a receipt that
was listed and is gone by the time it is read changed the census under the
observation, which is unknown too rather than an empty complete census. "No Oxide
record names this root" is only ever concluded from a complete look, never from a
look that failed or that watched the source change. A complete snapshot is reported on
the command document, and an incomplete one forces exit `3`. The census's own bound
is part of that: a folder that is empty, or whose entries all read, is only a
complete look while the observation is still inside its bound, so an overdue census
is unknown rather than a complete empty one. The one reference deadline covers
discovery too: finding Oxide's control directory is part of the observation, so an
expired bound there — and on the absence of a receipts folder as much as on its
contents — is unknown, never "no Oxide records". More than one distinct consumer path for one root, or two records that
disagree about one path's volume, is also unknown and keeps that root: the plan
proves what it can name exactly once.

The last-activity clock. `OldestIdleAfter` compares `now - last_activity_ms`,
and `last_activity_ms` is now the newest modification time found by a bounded,
descriptor-relative scan of the root's subtree, with the root directory's own
modification time as its floor. **The floor must exist**: a root directory that
reports no modification time leaves the age unknown, because its entries' times
can be decades old while the directory itself changed recently, so a child's time
is never used in its place. The scan never follows a symbolic link, and a
modification time ahead of this run's clock, an entry that reports no modification
time at all, an unreadable subtree, a name that exceeds one listing's bound, a
depth past 64 levels, or its deadline leaves the age unknown too, which keeps the
root. The deadline is checked before the scan, before every listing and again
before the scan reports a complete observation, so a listing — including one that
answers empty — cannot consume the bound and still leave a complete, old-looking
age behind. A missing modification time is never read as zero: zero would make a
live root look decades idle on both delete paths, which is why the scan treats it
as unknown rather than as an old timestamp. The same scan counts the row's bytes. The
bounds are `ACTIVITY_ENTRIES` (200 000 entries), `ACTIVITY_DEPTH` (64) and
`ACTIVITY_WALL` (5 s) per root in `area`. Both delete paths now use this clock:
`barge maintain`'s area policy asks the listing for the scan, and the release
plan does the same. A read-only listing (`barge describe`, `barge area root
list`) keeps the cheap floor instead, so a report never walks a consumer's whole
tree; a document that decides an age reads the scan's own `activity` evidence.

Two consequences worth stating. First, the release plan proves an absence only
for a reference that is not current; a current reference already keeps the root,
so no proof is spent on it. Second, nothing in the plan is a permission: F4 must
re-establish every predicate, and the plan's digest binds evidence, not consent.

Locking follows [the lock table](#lock-order-with-the-stage-1-locks) exactly, and
its rank check is what proves it: the plan reads the registry under the shared
observation lock and releases it, then takes the volume's shared lock for the
rest of the pass, then one root's shared lock at a time, released before the next
root so two roots are never held at once. **Every fact about a root is read under
that root's own lock**: `area::list_names` uses the `os` layer's names-only
enumeration (`DirOps::list_names`), which reads the directory stream and no
entry's own facts at all — no child is opened or stated, so no device, inode,
owner, mode or time is read, and an entry that cannot be stated cannot fail the
batch either; it is simply a row of its own. The marker, its digest, the root
directory's identity, its holds, the bounded activity scan and the root's own
Oxide marker are all read after the shared root lock is taken, so no row reports
an observation a consumer's hold or a delete could have been changing. An adopted
root needs **both** locks: a root whose name is its Barge root id takes that root
lock (which is the one a hold takes), and any other name first takes the deletion
unit's own lock key — the lock a delete of that unit takes — then reads the
marker, then takes the marker's root-id lock too, because a hold is keyed by the
root id rather than by the folder's name. The marker is read again under the
second lock, and a marker whose root id changed between the two reads keeps the
root. Every shared wait is bounded by the run's own lock budget
(`ReleaseBounds.lock_wait`, the state directory's own `LOCK_WAIT` by default). A
volume lock a live owner holds past that budget refuses that volume's preview
with `locked`, names the volume and the lock file, and changes nothing; a root
lock that cannot be taken keeps that root alone, as a row with `blocked_by:
lock`, no observation at all (no marker digest, no bytes, no reference set) and
no eligibility, while every other root of the area still plans. A lock file that
does not exist is nothing to wait for: a read-only command creates no lock, no
file and no directory, and the observation is a `None` rather than a new file.

[O4-001](#open-design-issues) leaves one decision to the implementation brief:
whether any additional unprivileged fact — a per-volume registration receipt
written at registration time, for instance — is worth the coupling it buys
against a block-cloned volume attached alone. F3 records it: none is added. The
residual is accepted as O4-001 proposes, because a clone carries only expendable
owned roots and the conservative mitigations (refusing simultaneous duplicate
mounts, reporting `mount_moved` and the mount census) are already in the
contract, and because a receipt on the drive is copied by the clone too. Treating
a clone as unknown would be an operator scope decision, not an implementation
detail.

## Proposed validation

Design-only validation for this contract is `effigy docs:check` once per changed
documentation input, with a fixed 120 s wall clock and 64 MiB of scratch under a
fresh `/Volumes/Builds/scratch/barge/run.*` that is removed afterwards. No Rust,
native, APFS or qualified-runner run is part of this design task; the tests below
are proposed for the implementation slices.

### Unit and counterfactual

| Area | Unit | Counterfactual |
| --- | --- | --- |
| Identity facts | the record's strict validation; the conjunction of volume UUID, root id, nonce, inode, relative path, kind and area; the device's within-run role | a record with an unknown field, a non-canonical UUID, a `..` component, a zero inode or a foreign owner is refused; a device-only change verifies with both numbers reported; an inode-only change, a marker `root_id`/`nonce` change, a path change and a `kind`/`area` change each keep the root |
| Marker versions | `barge.root/2` round-trip; `barge.root/1` read with the bounded prefix rule | a v1 marker whose relative path matches but whose mount prefix differs verifies; a v1 marker at a different relative path, or under a different volume UUID, is unknown; no code path writes a v1 marker |
| Marker generation | 1 at creation; 2 for a concluded `scratch_run` only | a conclusion on an `area_root`, a generation-3 marker, and a generation-2 marker with a null conclusion are all unknown |
| Volume resolution | one candidate verifies; none refuses by mode; two refuse | two mounts of one UUID (`FakeMounts`) refuse `duplicate_volume_uuid` and delete nothing; a mount-point change with the same UUID verifies and reports `mount_moved` |
| Identity write | the write order; the identity record written under the root lock | a crash between marker and identity record leaves `identity_record_missing`, never releasable, never adopted automatically, and still reclaimable by the space path; a second identity write is impossible by type |
| The two delete paths | the shared conditions and the two differing ones | a root with no identity record is kept by the release plan and may still be reclaimed by a space prune; a durable record that contradicts a live observation keeps the root on both paths; a space prune never reads an absence proof |
| Absence walk | leaf `ENOENT` gives `Absent`; every other outcome is enumerated | a missing non-leaf ancestor, a symlinked component, a component replaced between `fstatat` and `openat`, `EACCES`, a 65-component path and a depth-bound hit each give `Unknown` with its code; a relative path and a `..` component are refused before the walk |
| Absence ancestry | a path whose ancestors are root-owned and on another device than the anchor still proves | a real `/Users/...`-shaped path under a root-owned ancestor, crossing a firmlink whose device differs from `/`, with an `ENOENT` leaf proves `Absent`; the same shape with a `Present` leaf proves `Present`; ownership of the root, not of the ancestry, is what a delete depends on |
| Ancestor volume | `boot_data` and `registered(uuid)` proofs | an ancestor on a not-APFS volume, on a foreign mount, behind a stale mount point, on an unmounted removable volume, or on a volume whose UUID differs gives `Unknown` |
| The gate | each of the six conditions alone blocks a delete | six counterfactuals, one per condition, each proving the root survives with its reason; plus one proving all six together authorize it |
| References | receipts, registry flags and Barge's own records decide "current" | an incomplete reference set keeps the whole batch; a selected, reserved, pruning or cancelling row keeps its root |
| Plan and intent | canonical plan, digest, evidence on every row | a changed digest, a changed marker digest, a changed identity and a reappeared consumer path each stop the affected row only; a replay after a device change settles from the recorded identity |
| Oxide census | strict bounded reading of `oxide.outputs.registry/2` and `/1` | a version-1 row, an incomplete registry, a duplicated UUID and a row whose volume is away each report the honest verdict; nothing is written and no row becomes authoritative |
| Caches and scratch | the identity rule reaches every root kind | a `scratch_run` root concluded under generation 2 verifies; an adopted `held-root` with `origin: adopted_existing` verifies; a `cache_root` with a changed inode is kept |

The regression test the carried finding needs is a named pair: a fixture root
whose recorded and live device differ verifies under stage 4 and would have been
refused by the recorded-device comparison — the test asserts both, so a future
change that reintroduces the comparison fails.

### One bounded native

One focused native design proof, on owned scratch only, covering identity across
a remount and the absence primitive together:

- One owned APFS sparse image (`hdiutil`, `-owners on`), at most 128 MiB,
  attached at a unique `/Volumes/<name>`, backed by a file inside the guard's own
  scratch, never a real drive.
- Under Oxide's qualified shared runner (`bounded-test.py` /
  `scripts/bounded_runner.py` at the pinned supervisor bytes) with the fixed wall
  clock, 512 MiB of monitored owned bytes, the host reserve watchdog, positive
  closure and ordinary-detach cleanup. Barge writes no second supervisor.
- Cases:
  1. create two roots with markers and identity records, record `inode` and
     device, detach and re-attach the image, and prove the inode and relative
     path are unchanged, the device differs, and the verdict is `Verified` with
     `remount.device_changed: true`;
  2. prove the same pair of facts would have been refused by comparing the
     recorded device (the regression the finding needs);
  3. copy a root within the image, then point the identity record at the copy's
     path and prove the inode mismatch keeps it; delete a root, create a new one
     at the same path, and prove the new marker's identity does not match the old
     record;
  4. attach a second copy of the image and prove `duplicate_volume_uuid` refuses
     with nothing deleted; detach only the owned images by their own whole-disk
     devices;
  5. prove a `Gone` root (present volume, `ENOENT` path) and an `Unknown` root
     (unmounted image, same path) are reported differently;
  6. prove the absence primitive on the internal volume inside the guard's own
     scratch: a directory that exists is `Present`, after removal it is `Absent`
     with an ancestor on the boot Data volume by device and UUID, and a missing
     non-leaf ancestor and a symlinked ancestor are `Unknown`. The path crosses
     root-owned components (`/private`, `/private/tmp`) whose device differs from
     `/`, so this case also proves the walk does not refuse on either fact;
  7. prove the absence primitive under the image's mount, with the image as a
     `registered(uuid)` ancestor; detach the image and prove the same path is
     `Unknown`, never `Absent`;
  8. detach the image mid-delete and prove a pending intent whose replay after
     re-attach settles with the changed device, and prove the identity record's
     bytes are identical before and after the whole sequence.
- Explicitly not claimed: a real USB or Thunderbolt surprise removal, a real
  drive's flush cost, a block-cloned real drive, the internal SSD write saving,
  any reserve or floor behaviour, and any qualification of Oxide's or Effigy's
  adoption.

### The nine-premise HIGH checklist

Recorded in the proof's own plan and receipt:

1. A private `HOME`, `CARGO_HOME` and state directory, so no real account state
   is read or written.
2. The account home used for state resolution is the fixture's, and the real
   account state is untouched — asserted by a before/after digest of the real
   state directory, and by `barge describe` naming the fixture state root.
3. Environment isolation: `BARGE_STATE_DIR`, `TMPDIR`/`TMP`/`TEMP` and every
   inherited scratch point inside the guard's owned scratch; no inherited real
   path, and no inherited `BARGE_*` hook the proof did not set.
4. Explicit adoption only: every fixture root is created or adopted by an
   explicit command with a recorded plan, never by a default.
5. Owned image with ownership enabled (`-owners on`), ordinary detach, and a
   forced detach only of the owned image's own whole-disk device.
6. Every flag and environment hook used is listed with its purpose; the only
   debug-only hooks are the existing crash and fault injections, each bounded to
   the guard's scratch and each writing a fired-marker into the evidence
   directory.
7. Temporary storage: everything inside the guard's one owned root; nothing in a
   home folder, a drive root or a fixed `/tmp/<name>`.
8. `mktemp -d` under that root for every fixture directory, never a shared name
   and never a link to a real checkout.
9. The qualified runner bytes and selector are named and hash-pinned; Barge adds
   no second supervisor, session or accounting harness.

### Elevation

This design deletes storage and writes to removable media, so its implementation
is Elevated, and the elevation path is Oxide's, reused rather than reinvented: a
HIGH premise record, then an uncounted diagnostic (keep-going, latched first
fault, dependent-skip, and no promotion of unknown to pass) before any counted
run, then a separate counted author run and a separate independent reviewer run,
each separately activated at an exact head. A missing native receipt is a missing
proof, not a pass. This design task produces none and claims none.

## Open design issues

Named, with the proposed resolution and what the next brief must do. None is a
qualification claim, and none contradicts a frozen ruling.

- **O4-001 — A block-cloned volume is indistinguishable.** A clone of a whole
  disk carries the same volume UUID, the same inodes and the same markers, so
  when it is attached *alone* at the original's mount point nothing Barge can
  read tells them apart. Simultaneous mounts are refused
  (`duplicate_volume_uuid`). Proposed: accept the residual, because what it can
  delete is expendable owned data on a copy of a drive the operator made, and
  because the conservative mitigations (refusing duplicate mounts, reporting
  `mount_moved` and the mount census) are already in the contract. The
  implementation brief must decide whether any additional unprivileged fact
  (for example a per-volume registration receipt written at registration time)
  is worth the coupling. If the operator wants clones treated as unknown, that
  is a scope decision, not a design detail.
- **O4-002 — Repairing a root that has no identity record.** Tom's ruling
  `d9a9cd37` chooses one explicit operator command, never automatic. It takes
  the root lock, verifies the marker and volume, requires custody `Safe`, writes
  `origin: identified` once, and reports the decision and custody evidence.
  A missing record is otherwise unknown and never establishes release authority.
- **O4-003 — The `barge.root/1` prefix relaxation.** Stage 1's v1 marker records
  an absolute `resolved_path`, and S3 writes it and validates it against the path
  it walked, so the relaxation is live from the first day rather than a
  contingency: without it, every root S3 creates on a drive that later reconnects
  at another mount point is unverifiable. Stage 4 reads v1 that way — the mount
  prefix replaced by a verified mount of the recorded UUID, every component below
  it compared exactly — and writes v2. The architect (design freeze) should
  confirm the reading; the alternative, refusing every v1 marker after a
  mount-point change, is rejected because it would make S3's roots unprunable
  forever.
- **O4-004 — The mount enumeration primitive.** Proposed: `getfsstat(2)` for the
  candidate list plus `ATTR_VOL_UUID` read through each candidate's descriptor,
  with a 256-entry bound and at most 64 candidates for one UUID; `diskutil` is
  never parsed. The implementation brief must record the concrete refusal path
  for a truncated or partial mount table (proposed: `unknown_state`, never "no
  mount").
- **O4-005 — Last activity for an area root.** Decided by F3 as proposed, and
  landed in `area`'s scan: an `area_root` has no conclusion, so its clock is the
  newest modification time in a bounded scan of its subtree, with the root
  directory's own mtime as the floor, and a future time, an unreadable subtree,
  a reached bound or a missed deadline as `Unknown`. `last_activity_ms` is never
  a marker's `created_ms` alone. A consumer that wants a different clock declares
  one in its area; Barge does not infer one. The stage 3 space path's own
  idleness is still its recorded release, which this does not touch.
- **O4-006 — Reading Oxide's prune registry in place.** Stage 1 reads Oxide's
  registry "presence and version only", and this contract widens that boundary in
  two places: F3's release gate reads the registry, its receipts and one root
  marker as references, and F5's census in
  [reading the census](#reading-the-census-without-adopting-anything) reads the
  registry's rows to answer the carried finding. Every reading keeps the four
  properties that make the widening safe — read-only, strict, bounded, never
  authoritative — and a version, a field set or a bound this build does not know
  is `Unknown`, which keeps the roots it covers. The F3 slice row names registry
  rows as a reference source, so the earlier "used by `barge inspect` alone" is
  the one clause of this issue F3's own frozen row corrects; the properties it
  set are unchanged, and the reading is still retired when Oxide adopts its area.
- **O4-007 — Who may prune an Oxide root, and when.** Proposed: nobody but
  Oxide until Oxide declares its areas and switches, so the gate is proven
  against Barge-owned roots first; the 91 legacy roots then need an operator list
  and an adoption decision that `ff13f2ef` already tracks. Nothing in this
  contract authorizes a single Oxide root to be deleted by Barge.
