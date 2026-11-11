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
a record. `barge inspect` censuses Oxide's output-registry rows read-only and
reports, per row, whether the recorded device moved or the recorded directory
changed, with the drift counted — the evidence the carried finding needs, and no
adoption.

The release gate plans and does not act: `barge prune --plan --area <ns>/<area>
--volume <uuid>` writes one `barge.prune.plan/1` inside `barge.prune/1`, with
every row's six conditions and their evidence, and changes nothing. Its age is
the bounded scan of the root's subtree this contract fixes, its reference set is
read from Barge's own placements, area declarations and holds and from Oxide's
registry, receipts and root markers strictly in place, and the intent, the
effect and the replay remain F4.

No part of this contract authorizes a native run, a real drive, a mount or host
setting change, a data move, a deletion, or any change to Oxide. The one deletion
path it defines is the release effect in [F6-R1](#f6-r1-landed-surface): it runs
only when an operator runs it on a presented plan, and nothing in Barge calls it.

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
- **Writer custody, read in place** (R4): every checkout that names the root
  must have its writers positively closed. The control state
  (`<pool>/oxide-managed/state.json`) and the pool ledger (`<pool>/ledger.json`)
  of the internal pool, and of the removable pool inside it, are read once per
  plan. A checkout with a spawning, running or acquiring record, or with a running
  clean, is current. A checkout whose custody cannot be read as closed makes its
  set incomplete. A closed checkout adds no row. A fact that belongs to no one
  checkout makes every Oxide set incomplete, so the whole batch is kept: an
  unreadable pool, a held ledger row with no record, or an active or unclosed
  record that names no checkout, since no checkout can own its writers.
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
| a checkout's writers are closed in both pools: Oxide's `checkout_custody` over `state.json`, with the ledger's held rows bound to their records | the same reading, `pool_custody`: closed, held or unknown per checkout; a lost, unknown, unreadable, malformed, oversized, unknown-version or mid-read-changed fact is unknown | stricter in three places, each keeping more: a retired record without a lost fact is unknown; an unclosed lost slot is unknown whatever its holder's boot session; an acquiring record or a running clean is held |
| `diskutil`/`statfs` text | never identity; kernel facts through descriptors | stage 1's rule |

Rules that keep the handover safe:

- While Oxide prunes, **Barge deletes no Oxide root.** The primitive is
  implemented and proven against Barge-owned roots first; Oxide's adoption is
  its own slice with its own brief (stage 1 S8, `F6` below).
- Barge never writes an Oxide marker, receipt, registry row, tombstone, control
  state or ledger. It reads `oxide.outputs.root/2` markers,
  `oxide.outputs.registry/2` rows, the receipts and each pool's `state.json` and
  `ledger.json` strictly, bounded and in place, and reports them. It takes no
  Oxide lock and creates no file.
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
<internal pool>/outputs/registry.json   oxide.outputs.registry/3 (and /2, /1)
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
| `barge.inspect` | `/1`, optional fields added | `barge inspect` | adds `oxide_census`: the Oxide output-registry observation, its per-row identity census and the drift summary |
| `barge.prune` | `/1` | `barge prune --plan` | the command document around the plan, as `barge.maintain/1` wraps `barge.maintain.plan/1`: the plan, its SHA-256, when it was planned, the volume's state and whether the Oxide reference snapshot was complete |
| `barge.prune.plan` (F6-R1 additions) | `/1`, optional fields | `barge prune --plan --root` | `named_roots` (sorted, unique root ids) and each row's `age_waived`, present only when a root was named; both covered by the digest |
| `barge.prune.run` | `/1` | `barge prune --plan <file> --plan-sha256 <hex>` | new in F6-R1: the release effect's report, the presented digest, the fresh gate's digest for the same area, the replay's pending count and one outcome per presented eligible row |
| `barge.area` | `/1`, optional field added | the state directory's `areas/` | adds `outputs_grant` (the area-scoped `oxide_outputs` delegation), carried only by `oxide/outputs`; present or absent is the grant or its withdrawal, and the record's `generation` bounds a caller's request ([the adapter](#the-oxide-outputs-adapter-barge-is-the-covered-classs-only-deleter-2026-11-09)) |
| `barge.intent` | `/2`, optional fields added | `state/intents/<intent-id>.json` | adds `caller_intent_id` and `caller_request_digest` for the delegated outputs operation; both optional, written together, only on an `oxide_outputs` unit |

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

## The Oxide outputs adapter: Barge is the covered class's only deleter (2026-11-09)

Tom selected "Implement the reviewed mechanism-only slice on disposable
fixtures" in decision `63ad6fc4-52c3-4b75-bdf8-c1a89ac7eb35` v2 on 2026-11-09.
That ruling accepts, for this slice only: the area-scoped outputs grant as
deletion authority, Barge-only deletion for the covered root class, and the
adapter-specific D3 changes (Oxide keeps eligibility and its witnessed
pool-level writer decision; Barge re-proves unit custody at effect and does not
re-read Oxide's pool ledger as a gate). The owning protocol is Oxide's `docs/knowledge/contracts/barge-adoption.md`,
section "Joint protocol for Barge output deletion" (oxide#303, reviewed at
PR #262 head `2085816e`, merge `0747820e`, with the planner's ruling recorded
on Oxide main at `50d572bc`); Barge does not edit that file, and this section
records Barge's half of the protocol without duplicating its tables.

What landed in `crates/barge/src/oxide_outputs.rs`, all default library code:

- **The family.** `MarkerFamily::OxideOutputs` reads `.oxide-output.json`
  (Oxide's `oxide.outputs.root/2`). Its `validate_grant_shape` grant is a
  delegation named `oxide_outputs`, and `native_root` is refused for it. The
  filename is in the recognised marker list, so a claimed or captured unit that
  gains such a marker is kept, and a nested one stops a walk.
- **The grant.** An optional `outputs_grant` field on the stored
  `barge.area/1` record for exactly `oxide/outputs` (a declaration elsewhere
  refuses to carry it). The record's `generation` is the grant's generation
  while it is present; a re-declaration that omits the field withdraws it. The
  adapter reads the stored record itself — a caller's object is never
  permission — refuses an empty, withdrawn, wrong-family or stale authority,
  and re-reads it under the exclusive volume lock immediately before the
  intent. Custody re-checks the same grant at effect, so a revoked grant keeps
  the unit on every path.
- **Correlation.** `barge.intent/2` gained optional `caller_intent_id` and
  `caller_request_digest` fields. The request digest is the SHA-256 of the
  caller's intent id, the intent's preview digest, the bound root identity
  (volume UUID, inode, nonce, marker digest) and the recorded grant
  generation. Only an `oxide_outputs` directory unit under its delegation may
  carry the fields, and the two are written together. Old records and both
  scratch families read exactly as before.
- **The entry points.** `execute` takes the caller's live-intent claim plus the
  bound request and reports one of: `deleted`, `pending`, `rolled_back`,
  `refused`, `known_no_effect_locked`, `unknown`. It re-reads the intent from
  Oxide's own registry (the claim is never permission), refuses a settled or
  mismatched one, refuses a legacy (`oxide.outputs/2`, `/1` or absent) fence,
  answers unknown for an unreadable fence, control directory, registry or
  incomplete registry, and binds the unit by volume UUID + inode + nonce +
  exact marker bytes. `lookup` now takes the existing LiveIntent and DelegatedRequest instead of bare
  volume/caller-id arguments, plus the optional expected Barge intent id.
  Both pending and terminal matches validate volume UUID, caller id, supplied
  Barge id, canonical request and plan digests, outputs family/delegation,
  directory kind, inode, marker digest and exact volume-relative location.
  The location is checked against the UUID's registered root directory and
  declared outputs folder, then against every component below the original request's recorded mount
  prefix, identified by the platform's mount-parent rule. An extra component
  before the area is a mismatch; an unidentifiable prefix stays unknown.
  The journal's mount hint and the request's original mount prefix need not
  match: a first effect after remount records the new mount. Neither mount
  hint nor device number is durable identity. Missing registration/declaration
  or a changed relative location stays unknown; this reads location binding,
  not a new grant or eligibility policy. The canonical
  digest also binds nonce, preview and grant generation. A correct digest
  cannot hide a different stored unit. Missing, malformed or conflicting
  binding returns Unknown and preserves records; no caller-id-only fallback.
  execute applies the same check before its terminal return and again to
  the replay record read under the exclusive lock. Device numbers remain
  observations. Oxide must consume this source-breaking checked API rather
  than parse journals or trust the former caller-id-only result.
  The four states remain —
  found pending (with the Barge intent id, which names the
  `.barge-prune-<intent-id>` quarantine leaf), found terminal, positively
  absent (a complete journal read under the volume's observation lock, no
  record, no attributable leaf), or unknown — so Oxide can settle or keep
  pending without inferring completion from missing bytes.
**`PositivelyAbsent` is Barge-side only.** It answers Barge's journal and
Barge's quarantine leaves and nothing else; the protocol's positive absence
for rollback additionally requires the original root present with the
recorded identity, which is the Oxide caller's own proof, made on its own
durable record. The lookup's answer alone is therefore never rollback
authority, and a truncated journal is unknown there too, never absent.
- **The single-deleter rule.** Only the adapter constructs, executes, resumes
  or settles this family. A generic delete — a host release, a sweep, the
  space prune, a direct library call — refuses an `oxide_outputs` unit before
  any effect, and every generic replay reports such intents pending without
  settling, resuming or rolling them back. A host release holding the area
  grant deletes none of Oxide's roots.
- **The same-call rule.** This call's own `Locked` return, taken before any
  durable step and with no journal record for this caller id (re-read after
  the denial), is the explicit known-no-effect fact. A resumed call under a
  `Locked` volume lock stays pending. Any second call under an Oxide intent id
  resumes or reports the one record, never a second unit.

Unchanged, and refused on the adapter path: `oxide.outputs.root/1` markers,
the legacy fences, an unregistered or non-expendable output volume, a checkout
under a mount parent without a verified marker-recorded checkout UUID, and the R4 and
pending-root rules of the generic release gate. The **device number is never
a bound identity**: the adapter verifies the live volume by UUID and mount
root, takes the device the live volume reports now as the walk's
within-run containment observation, and refuses nothing because the intent
identity's `dev` differs — a remount binds by UUID + inode + nonce + marker
bytes, exactly as the protocol's remount row and Barge's own replay path
(read `replay_unit`) bind. Qualification so far is the
disposable-fixture suite (`effigy rust:test`); the focused adapter native
proof is prepared as the `outputs:native:*` selectors in `effigy.toml` under
the qualified shared supervisor, held for separate planner activation, and it
runs the proof-only synthetic-account build. It does not qualify the
production account lookup, a live drive, or live adoption; no install, real
declaration, grant write or existing-data deletion is authorised by this
slice.


The focused outputs native apparatus retains A1–A12 and adds A13: after an
actual adapter deletion, wrong preview/request and wrong Barge-id lookups
remain unknown for binding, terminal bytes stay unchanged, and the exact
request still recovers. This dimension awaits separately activated diagnostic,
author and retained-reviewer occurrences; Barge051's older evidence does not
qualify it. Synthetic account facts remain a gap.


### Explicit recovery routes and absence-only observation (2026-11-10)

Tom selected the explicit recovery-format upgrade on disposable fixtures in
`60a85553-3e61-4280-9037-0f3965d5ff65` v2, extending `63ad6fc4` v2. Barge's
reader/API implementation supplies the prerequisite for Oxide's caller; it
is not a claim that the Oxide representation writer has shipped. The read-only
source binding is Oxide PR 263 head `661aba1d03e936e51a56e88d9f43fa0787efc0fd`,
`docs/knowledge/contracts/barge-adoption.md`, SHA-256
`f248c984d40482be8b7d86866f31fa2415a8c83bfc02636bea1689be7b9f33a6`.
The predecessor is Barge052 reviewed `9a65089e8114860d498c5721c8f19ca93a07e481`
and published `594494061b18bef4821ab19723f4adfff6119d51`, identical tree
`1b219ce8a760e8403ea123f258022016dc919e7d`.

The exact successor registry is `oxide.outputs.registry/3`, numeric schema 3.
It requires `deletion_routes`, with exactly one entry per retained intent,
including terminal intents, and no extra ids. The serialized entries are:

```json
{"intent_id":"<32 lowercase hex>","deleter":"local"}
{"intent_id":"<32 lowercase hex>","deleter":"barge","request_sha256":"<64 lowercase hex>"}
```

These are exact field sets. Unknown fields/tags, missing, extra or duplicate
routes and malformed hashes make the whole registry Unknown. Routes share the
256-intent and 4 MiB registry caps and existing read deadline. Prune-intent/1
keeps its exact fields, real quarantine path and actual preview hash. Historical
registry /1 and /2 never infer routes and reject the successor field.
`reference::OxideIntent::deletion_route` is None only for historical documents;
otherwise it exposes validated `DeletionRoute::Local` or
`DeletionRoute::Barge { request_sha256 }`.

Pool fence `oxide.outputs/4` joins /3 in the covered predicate without raising
the numeric control-state version. Earlier fence meanings are preserved.
Recognizing /4 gives generic release no new permission: the outputs family
remains exclusively adapter-owned. Old readers refuse the successor formats.

`DelegatedRequest` adds `request_sha256: Option<String>`, the immutable original
sidecar's byte SHA-256. For schema 3 execution and replay it must equal this
intent's validated Barge route; local routes and missing/wrong bindings refuse
before effects, including the Deleted fast path. That fast path also compares
Oxide's stored phase, preview and every durable identity field to the original
claim, after checking the Barge journal. All stored-intent comparisons exclude
device numbers, which are remount observations rather than operation identity.
Historical requests use None.
This hash is separate from `DelegatedRequest::digest`, whose canonical fields
remain unchanged (including original grant generation). Oxide owns strict
sidecar reading and publication; Barge neither reads nor writes the sidecar.
The caller must validate those bytes against the original identity, preview,
generation and canonical request digest. Merely supplying a byte hash is not
permission to replace an original request or borrow a current grant. All052
journal/unit/request/optional-id checks remain in force.

`lookup_absence(state_paths, dirs, facts, mounts, observer, intent)` takes the
original `LiveIntent` with its caller, volume and root identity, but no
`DelegatedRequest`. Its result `AbsenceLookup` is only `PositivelyAbsent` or
`Unknown { reason }`. Under the existing shared volume observation lock,
positive absence requires a completely read bounded journal with no matching
caller record and no attributable or ambiguous quarantine. A matching pending
or terminal record, malformed/truncated state, lock refusal, uncertain volume,
location or exhausted bounds stays Unknown. It never returns a record,
executes, replays, settles, removes a journal or manufactures a request.
Oxide must additionally prove the original root present with its recorded
identity before rolling back tagged unavailable metadata. A new operation
requires a new preview and intent; the unavailable record cannot acquire a
replacement request. Remounts preserve UUID and volume-relative location;
identity or location substitutions remain conservative.

The source regressions and effect-free native apparatus extend A1–A13 with
A14 (schema3 Barge deletion), A15 (local/missing/invalid route refusal) and
A16 (absence-only positive and matching-record refusal). Native/image sessions,
in-guard staging, diagnostic, author and distinct retained-reviewer proofs
remain separately activated and must bind the actual exact head. None is
credited by source controls. The existing 128 MiB image, 300 s / 512 MiB guard,
30 s work stop, supervisor pin, pre-backing custody and detach/reap/whole-backing
closure gates remain. Production account lookup, O2/O3, parent-reader conversion,
installation, real grants/registration, migration and live adoption remain held.

### Delegated checkout absence bound to its volume (2026-11-10)

The deciding adapter check uses `checkout_volume_uuid` from the strictly read,
identity- and digest-verified `oxide.outputs.root/2` marker. Previously it read
that field but dropped it, refusing every checkout under a mount parent. The
strict prune-intent/1 format remains unchanged. A mount-parent checkout now
uses the existing `os::prove_absence` registered-volume rule: locate the recorded
UUID, verify its actual mount and ancestor through descriptors, and walk the
recorded relative components. A remount may change the mount spelling and
device; it cannot change the UUID or ancestor identity. A missing, malformed,
mismatched, unavailable or duplicate UUID, an unreadable/present checkout,
changed ancestor or exhausted bound keeps the root. An unresolved registered
path never falls back to BootData. Non-mount-parent paths retain their actual
BootData identity checks.

The deciding proof is carried in the existing typed `barge.intent/2.absence`
evidence, with no second journal or caller-supplied volume authority. Before
any pending replay, Barge re-establishes the saved checkout path and expected
volume, including the same ancestor inode and UUID/filesystem/kind, with
relative path binding across remounts. This applies before rollback, renamed
partial deletion, marker-last recovery and final settlement. Once marker-last
has removed the marker, the saved evidence still supplies the original
checkout premise. Older pending records without sufficient evidence stay
pending/Unknown; they cannot acquire invented volume evidence. Stored grants,
lock order, exact request/route/preview binding and quarantine meanings remain
unchanged.

The focused apparatus retains A1–A16 and adds A17 for digest-bound mounted
checkout UUID substitutions and actual matching deletion, and A18 for saved
absence refusals, actual same-UUID ancestor replacement and marker-removed
replay after restoring the original ancestor. All native fixture checkout paths
are now absent leaves in the run-owned Stash home, whose UUID is read from its
actual ancestor descriptor; output roots remain on the one owned image.
No BootData replacement or test-only positive evaluator supplies this join.
The apparatus records actual checkout and output UUIDs and the substituted
marker facts. These source controls are not native credit: exact-head source
review, an adequate uncounted diagnostic, counted author and distinct retained
reviewer proofs remain separate activation gates under the unchanged envelope.
Barge051/052/053 evidence retains its accepted limits and is not integrated
credit. Production account lookup, deployment, live adoption and integrated
proof remain unresolved; O2/O3 and production parent-reader conversion stay held.

## One operation journal for delegated recovery (proposed, 2026-11-10)

**Status: proposed design, source-only; architectural reset after three
non-converging exact-head reviews (planner ruling, brief v2).** Tom asked on
2026-11-10 (decision `1d4a7aee-a8e6-4a75-96f0-b142c4176db2` v2) for
delegated-deletion recovery that is dependable and simpler in *both* products.
This section supersedes its own prior revisions and states one coherent
protocol; everything below is PROPOSED and changes no shipped behavior until
implementation briefs land reviewed code. Oxide's owning contract is not
edited from here; its needed reconciliation text is quoted at the end.
Sources: Barge at this repository's HEAD; Oxide read-only at PR 263 head
`39211b4e1cef4abf9ea07f2ef1a4814ce6b0d3a0`.

Today one delegated deletion is coordinated by five durable facts across two
products — Oxide's `prune-intent/1` journal, the registry's schema-3
`deletion_routes`, the `prune-delegation/1` sidecars, the `oxide.outputs/4`
fence upgrade, and Barge's `barge.intent/2` with its caller fields — joined by
a four-state lookup, an absence-only lookup, a first-call-only `Locked` rule
and a twelve-row crash table. The protocol below replaces that join with one
Barge-owned operation journal, typed durable caller hooks, and an explicit
integrity inventory, and it keeps every safety rule the reviews required:
post-effect positive removal evidence, fail-closed journal custody with an
independent identity, complete guarded settlement, and the real admission-lock
contract.

### Failure model

| Guarantee | Today (two journals, PR 263) | Reset (one journal) |
| --- | --- | --- |
| Progress | two phase ladders joined by a four-state lookup | one phase ladder in one store |
| Deletion completion evidence | `removed` recorded after Barge's unlink; absence never resolves | unchanged, preserved verbatim |
| A lost record | detectable only while the *other* journal survives; Barge-side loss of one record is silence | a sequence gap is proven loss: `Unknown`, delegated refusal until operator resolution |
| A recreated/destroyed journal | `read_intents`' empty-on-absent default reads it as no operations | registry-level identity binding plus epoch: recreation is `Unknown`, no auto-recreation |
| Crash-consistency | atomic images + syncs on both sides | unchanged rule, every journal write |
| A historical whole-state restoration | invisible | equally invisible — a stated model limit, not a waiver (below) |
| Exclusion atomicity | caller intent published under publication custody before the Barge call | guarded verdict → begin → guarded re-affirmation by revision, plus fail-fast writer-side consults inside every registry publication's guard |
| Settlement | `complete_intent` + `settle_receipt` under the caller's intent | the handler mirrors the same two revisions and released-receipt branch, with the settlement entry atomic in the removal image, without the intent |
| Crash windows | twelve-row table across four stores | ten windows in one store (traced below) |

### Ownership boundary

Barge's operation journal is the ONE durable authority for a delegated
operation: progress, binding, the owed caller finalization, and whether
conflicting work may enter. Oxide's registry, receipts and tombstones remain
ordinary domain state — what exists and what is selected — reconstructible
from Oxide's own settlement writes; they never decide whether an operation
exists or is owed. The hook acknowledgement is a field transition inside the
record, not a third ledger.

    caller                                             Barge
    ---------------------------------------------     --------------------------------------
    binding lock; guard(admission): verdict;          re-proof, then ONE sequence-numbered
    release; begin; guard: re-affirm by       -->     record: intent > renamed > marker_last
    revision; release; effect runs                    > removed > deleted > complete
    drain(hooks) <--- dispatch, no Barge locks        owed finalize --ack--> complete

### Lock table (actual callers, bound source)

`store::publication_guard` **is** the internal pool's existing admission lock,
made reentrant (`store.rs:6548`–`:6577`) — so no rule below may hold it while
acquiring or waiting for any Barge lock. `LockSet` order is registry → volume
→ root → intent write (`state.rs:19`, `:1246`).

| Actor | Order (serialization primitive first, then consult) |
| --- | --- |
| covered prune | this checkout's clean barrier (`begin_clean`, `Recovering`) **and** the binding lock (`prune.rs:6591`) held across verdict, begin, re-affirmation and effect; guard(admission) for the verdict and again for the re-affirmation, each released before any Barge call; Barge exclusive volume lock for the effect; the handler takes guard(admission) alone |
| `adopt` | this checkout's clean barrier (`begin_clean` `Recovering` — it **waits** on a live holder, `outputs.rs:6864`) → in-lock consult (`outputs.rs:2300`) → publication writes under guard(admission) |
| `bind_for_command` | binding mutex (`store.rs:3439`) → in-lock consult (`outputs.rs:4486`) |
| `reserve` | `lock_binding` (`store.rs:3439`) → in-lock consult (`outputs.rs:6245`) |
| second prune | `prune_one_barrier` clean barrier + binding (`prune.rs:6591`) → in-lock consult |
| `resolve-intent` | binding + in-lock consult + today's prechecks, answered from the one journal |
| Barge-alone replay | Barge shared → exclusive volume lock; takes no Oxide lock |
| drain | Barge shared observation read → dispatch with zero locks → Barge locks for the ack; the handler inside holds guard(admission) only |

**Every shared registry writer consults the journal inside its own guarded
publication (named decision; PROPOSED).** A writer-scope proof alone cannot
close the handoff: `recheck` itself rejects another checkout's live reference
to the same root (`prune_barge.rs:388`–`:392`, "another checkout references
this root"), so cross-checkout live rows referencing this root path can
exist, and per-checkout locks (`begin_clean`, `store.rs:3575`; `lock_binding`,
`:3439`) do not serialize writers of different checkouts with the prune. The
closing rule: **every registry publication consults the journal, fail-fast,
inside the publication guard it already holds** — the guarded write refuses
if a pending operation record binds any `(checkout_id, root)` among the rows
or live root references it is about to write. The consult key is exact per
site: row-scoped writers (`adopt`, `bind_for_command`, `reserve`, the local
prune's intent publication, the settlement revisions) consult with the
`(checkout_id, root)` pairs they are about to write or reference live;
whole-image writers (`compaction`) consult for **any** non-terminal
delegated record. The consult read uses a non-blocking shared observation
mode (`try-shared`, PROPOSED): if the lock cannot be taken immediately the
consult answers `Unknown` and the writer refuses publication — a try-acquire
never blocks, so admission is never made to **wait** on a Barge lock. The
publication sites are enumerated: `adopt`, `bind_for_command`, `reserve`,
the local prune's intent publication, the settlement revisions, and
compaction; each adds one fail-fast consult (a read, not a write — priced in
the accounting). With this, the handoff closes for every writer: a writer
that consulted before the prune's begin and publishes after it still
consults inside its own guard, where the now-existing pending record refuses
it; a writer that published first is seen by the prune's re-affirmation
(unrelated advance → eligibility re-runs; a reference to this root →
pre-effect cancellation to the retained `cancelled` phase); and the
handler's in-guard fresh re-verification at each settlement write catches
anything that changed regardless of writer.

Rules, proven by the table: (R1) every cross-lock direction is caller →
Barge; (R2) **no Oxide admission lock is ever held while blocking-waiting for
any Barge lock** — the joint no-admission-wait rule's availability guarantee
is preserved verbatim, and the re-affirmation is what makes that possible
(begin runs with no Oxide lock held; the guard re-enters only after `begin`
has returned); (R2b, PROPOSED refinement of R2's letter) the writer-side
consults are the single permitted exception to the word "acquiring": a
`try-shared` acquisition attempt under admission gives up rather than waits,
so admission is never delayed by Barge — its worst case is a refused
publication while the journal is momentarily locked, retried by the writer's
ordinary retry. The begin and re-affirmation handoffs satisfy R2 in its
strict original form — no Barge lock is touched while admission is held —
and R2b applies only to the consults; (R3) Barge never takes an Oxide lock;
(R4) the handler runs with zero Barge locks — no Barge →
caller → Barge cycle exists. Re-entry: the guard is per-thread reentrant
(`store.rs:6558`), so the handler's settlement inside an Oxide command that
already holds the guard nests safely; a hook that consults Barge does so with
zero Barge locks held.

### Phases (the shipped evidence semantics, preserved)

`intent` (begin published; no effect; finalize owed) → `renamed` →
`marker_last` → `removed` — **Barge's own final unlink of the verified
captured leaf succeeded; recorded after that unlink, exactly as shipped** —
→ `deleted` (the durable completion fact; finalize still owed) → `complete`
(hook acknowledged; settled); and `cancelled` — a pre-effect cancellation
(verification failed, or the re-affirmation saw a reference to the root)
transitioning the record to a retained terminal phase, never a removal. Every
capture is journalled before its unlink; only the durable `removed` phase
proves the leaf is gone; absence alone never resolves anything, and the
unlink-to-publication window stays conservative: a stop there leaves the
record at `marker_last` with the leaf's capture recorded, the recorded
private name gone reads `Unknown`, and the operation stays pending — never
acknowledged, never settled (stage 1 design, "Durable custody of captured
objects"). **No numbered record is ever removed**: cancellation, like
settlement, is a retained terminal transition, so an authorized cancellation
can never masquerade as a lost record (the gapless rule below stays exact).
This is a stated PROPOSED change to stage 1's shipped rule — "A rolled-back
`intent` phase is the one record removed" — which exists to keep the shipped
journal clean; the successor major retains the record instead because the
gapless inventory requires numbered records to persist. Stage 1's shipped
behavior is unchanged until implementation briefs adopt this, and the owning
design carries both statements without contradiction by marking this one
PROPOSED.
The consult excludes conflicting work for non-terminal phases only
(`intent`/`renamed`/`marker_last`/`removed`/`deleted`); `cancelled` and
`complete` bind nothing, and a fresh attempt uses a fresh caller key. Each
record carries its journal `sequence`; a delegated record carries
`resume: caller`; Barge's own roots keep `resume: barge` (a delegated record
at `intent` is refused to generic replay and to every path except a
caller-authenticated resume, which re-runs the guarded verdict, the
re-affirmation and the full recheck — `prune_barge.rs:336` — before the
effect; post-capture phases are Barge-owned replay that settles only on the
durable `removed` evidence plus positive absence; a generic delete that is
not a record's replay still refuses a delegated unit, `delete.rs:452`,
`:516`). Retained `cancelled` and `complete` records are the counted
retention cost of the gapless inventory.

### Begin

`operation::begin` (PROPOSED) merges `execute`'s steps 2–8
(`oxide_outputs.rs:591`) into one call under the `LockSet` order: stored
declaration and area grant re-read, volume record, exact unit construction and
validation, marker re-proof, checkout absence — all before the record is
published. The caller computes the request digest as `DelegatedRequest::digest`
does today (`oxide_outputs.rs:124`); preview, grant generation and original
identity travel in the typed payload, so no refreshed grant or current root
substitutes for original authority.

**Eligibility** is caller-owned and serialized against every registry writer
without holding admission across a Barge wait: the caller takes the binding
lock and guard(admission), computes the verdict with its own rules — the
complete-group verification `journal_intent_with_route` performs under that
guard (`prune.rs:7118`: complete registry, per-row flags clear, identity
matching) plus the receipt committed and not placing the root and unknown-free
pool custody — records the observed registry revision, and **releases the
guard**. It calls `begin` (no Oxide lock held), then re-takes the guard for a
bounded **re-affirmation**: the live revision equals the record's → proceed;
it advanced → the full guarded eligibility re-runs — still eligible → the
record's bound revision updates under Barge locks and the effect runs; the
root became referenced or selected → pre-effect cancellation (the retained
`cancelled` transition) and refusal. Every registry writer holds the same
guard, so any interleave between verdict and the re-affirmation is observed
there; changes after it are excluded by the pending record and the
writer-side in-guard consults every publication performs, as today's pending
intent and `pruning` flag exclude them. Effects are
strictly ordered after the record's durable publication, so a world without a
record is the pre-effect world: nothing happened, no exclusion owed, a fresh
prune may begin again under a new key. A begin whose publish cannot confirm
its directory sync (`state.rs:787`) reports `Unknown` — never `Began`, never
no-effect — and the caller keeps the root excluded; a later same-key begin
binds one operation. A captured unit can never be re-begun (its original path
is positively gone). A `Locked` begin is a plain retry; no same-call rollback
machinery exists.

### Journal integrity and custody

The journal (`state/intents/`) gains an integrity inventory of three pieces,
counted as machinery in the accounting:

1. **Epoch document** `barge.journal/1` (PROPOSED) inside the journal
   directory: the journal's creation id and its `sequence` watermark. Every
   journal write publishes whole documents by exclusive rename + directory
   sync (`state.rs:787`). **Allocation is crash-consistent**: begin reads the
   watermark `w`, publishes the record numbered `w+1`, then advances the
   watermark to `w+1`. A read accepts sequences `1..=w` exactly once
   (settled and cancelled records retained, so the set is gapless) and **at
   most one** unwatermarked record numbered exactly `w+1` — the allocation
   tail; recovery verifies that record fully and advances the watermark as
   part of any recovery pass, after which the journal is gapless again. Two
   or more unwatermarked records, or any sequence above `w+1`, is broken
   allocation: `Unknown`, refused, never renumbered.
2. **Identity binding**, a static document at the account-state level but
   **outside** the journal directory (`<state>/journal-identity`, PROPOSED):
   the same creation id and creation time. It is written only by explicit
   state initialization — epoch first, binding second, each synced; the pair
   is complete only when both exist with the same id. An empty, unbound
   journal (no records) may be bound by a re-run of explicit initialization;
   anything else is never created, re-created, re-numbered or repaired
   implicitly.
3. **Gapless reads.** Recovery paths — consult, begin, drain, replay — require:
   binding present and readable; journal directory present; epoch present with
   the binding's id; watermark ≥ every record's sequence; every sequence
   `1..=watermark` present exactly once (settled records are retained, so the
   set is gapless); every document within its byte and row caps
   (`contract.rs:47`, `:50`).

The guarantee classes, exactly: **detected** — a sequence gap proves that
record is lost (including a record lost after the final unlink and before the
hook completed: its disappearance is a gap, never silence); recreation or
destruction of the journal directory is detected by the binding (a recreated
directory cannot carry the id the state level expects); unreadable, truncated
or over-cap state is `Unknown`. Every detected class reads `Unknown` →
delegated admission, recovery and drain refuse until explicit operator
resolution (a bounded operator command for a proven gap or a broken binding is
named in the implementation split); no path recreates or renumbers
implicitly. **Crash-consistent by construction** — atomic publishes mean the
journal contains only whole documents; an unconfirmed sync is `Unknown`.
**Not detected, stated as a model limit** — an arbitrary valid historical
restoration is indistinguishable from the journal's own past, in two forms: a
whole-state restoration (binding, journal and records from one consistent
earlier moment), and a narrower one where a valid **older epoch and older
journal are restored together while the identity binding is unchanged** — the
restored watermark and its records are internally gapless, so records
numbered after the restored watermark are lost without a gap. The journal's
guarantees bind its own writes, not the storage beneath them; the shipped
two-journal design has the same limit (restoring yesterday's `prune-intent/1`
image is equally invisible). This is a stated limit of the model, not an
accepted-corruption exception. The caller's own
non-terminal rows for positively absent roots may *name* candidate scopes to
the operator during gap resolution — domain state informing a refusal's
scope, never deciding that an operation exists. An unattributable
`.barge-prune-*` leaf anywhere is a live deletion, never an absence
(`oxide_outputs.rs`, `quarantine_leaf`).

### Conflicting entry points: consult inside the lock

A consult outside the serialization is only a hint. Each conflicting entry
point consults **after acquiring its serialization and before acting**:
`reserve` (`outputs.rs:6245`), `bind_for_command` (`:4486`), `adopt` (inside
the admission barrier, `:2300`), the second prune (`prune.rs:6591`),
`resolve-intent`. The prune's begin publishes under the same binding, so the
handoff is atomic both ways: a command that waited and then acquired sees the
pending record and refuses; if the prune ran first, the waiting command
acquires only after settlement has made the registry and receipt consistent.
Barge-alone replay takes only Barge locks and resumes only records that
exist, so a command either sees the pending record or no replay can start.
The consult reads the one journal under the fail-closed rules above; unknown
refuses.

### The hook contract (PROPOSED)

```rust
pub struct BeginRequest<'a> {
    pub caller: &'a str,             // namespace, e.g. "oxide.outputs"
    pub key: &'a str,                // the caller's own operation key
    pub finalize: FinalizeSpec<'a>,  // hook kind, version, bounded payload
    pub unit: DeletionUnit,          // the exact binding; today's type
    pub request_digest: &'a str,     // the original request's digest
}
pub enum Begun { Began, Already(OperationState) }
pub trait FinalizeHook: Sync {
    fn kind(&self) -> &str;  fn version(&self) -> &str;
    fn finalize(&self, op: &FinalizeOp) -> Result<FinalizeAck>;
}
pub fn begin(...) -> Result<Begun>;  // idempotent per (caller, key)
pub fn drain(paths, hooks: &[&dyn FinalizeHook], bounds) -> DrainReport;
```

Authorization, eligibility policy and domain bookkeeping stay caller-owned;
Barge owns the destructive phases, replay and progress, persists type, version
and bounded payload — never closures or function pointers — and validates
only shape and bounds, never interpreting the payload. No plugin loader,
daemon, workflow platform or callback-registry service: the handler is linked
code registered in-process; one fixed dispatch API is enough. Dispatch runs
outside every Barge lock (R4); double dispatch is safe because handlers are
idempotent and the ack is idempotent; begin refuses a request whose digest
matches a record with an owed finalize. The payload budget is 2.5 KiB inside
the 4 KiB record cap (`contract.rs:47`), reserving phase and capture fields;
oversize or an over-large group refuses at begin, before any durable step.
Settled records are kept (stage 1, "A settled intent is kept"); the read cap
makes an over-full journal `Unknown`, never a prune.

### The finalization transaction

The payload carries: original identity and preview (including `root_kind`
and `volume_uuid`), grant generation, caller key, checkout id and path,
receipt path, group rows as `{member, generation, nonce, root_ino,
marker_digest}` (at most six; larger groups refuse at begin and are not
covered), the receipt's digest at begin (`prior_receipt_sha`), the fixed
`terminal_ms`, and the begin-bound registry revision. The handler's guarded
verification reproduces `recheck`'s own comparison set (`prune_barge.rs:358`–
`:392`) exactly, with one justified exclusion: per-row `selected`, `pending`,
`reservation`, `cancelling` and `pruning` flags, `nonce`, `generation`,
`root_ino`, `marker_digest`, and the row's `checkout` and `receipt` paths
compared to the record's identity — while `root_dev` is the one excluded
fact, because it is the remount observation this design never binds (the
shipped `replay_unit` rule), and a handler that compared it would refuse
every resume after a remount. `recheck`'s cross-checkout scan is reproduced
too: any **live row of another checkout referencing this root** refuses and
stays owed (`prune_barge.rs:388`–`:392`). Every write happens inside
guard(admission) with a completely fresh registry and receipt read; nothing
observed outside the guard authorizes a write:

1. Guard; read the registry completely, fresh; unknown stays owed.
2. **Verify the complete group as an exact set**: the rows matching
   `(checkout_id, root)` must be **exactly** the payload group — every member
   present with matching nonce, generation, inode and marker digest, clear
   selection/pending/reservation/cancelling/pruning flags, no other live root
   reference, **and no extra same-group row beyond the payload members**; an
   unexpected extra row is as much a changed group as a missing one. **Exact
   match** → continue. **Partial** (any member absent or changed while others
   present) → `Unknown`, mark nothing, stay owed — a matching subset never
   authorizes a whole-group transition. **All absent** → settlement only if
   this operation's settlement entry is present (step 5); otherwise anomalous
   → `Unknown`, stay owed — absent rows alone never prove this handler
   removed them, and the bound revision refines the diagnosis only: a fresh
   revision equal to the begin-bound one means the group never existed after
   begin (anomalous); a greater revision means a concurrent actor may have
   removed them; both stay owed. Rows of a recreated root at the same path carry a different
   nonce and make the set inexact: `Unknown`, never matched, marked or
   removed.
3. **Receipt, read-only, under the same guard**, mirroring `recheck`'s
   preimage rule: hash the live receipt; if it differs from the payload's
   `prior_receipt_sha`, the receipt changed since begin — refuse and stay
   owed, exactly `recheck`'s "the original receipt changed before Barge
   settled" (`prune_barge.rs:393`–`:396`); if the receipt is pruning or its
   current selection places this root → refuse, stay owed (the covered begin
   required it not to place the root; "a released root is history in its
   receipt", `prune.rs:7527`) — the handler never rewrites a receipt a
   current selection owns. Otherwise the released-root receipt step is a
   no-op — the handler deliberately does not reuse `publish_terminal_receipt`,
   whose commitment lives in the intent this design removes.
4. **Terminal marking revision** (guard held): clear
   `selected`/`pending`/`pruning`, set `pruned_ms = terminal_ms` on exactly
   the verified rows — `complete_intent`'s marking (`prune.rs:7361`) with the
   recorded fixed timestamp making re-marking idempotent.
5. **Removal revision with the atomic settlement fact** (guard held, fresh
   read): re-run step 2's exact-set verification and step 3's receipt check
   inside the guard; verify the rows are ours, matching, and marked with our
   `terminal_ms`; then remove exactly those rows and record this operation's
   settlement entry **in the same registry image** — a bounded `settled` set
   `{caller, key, checkout_id, root, nonce, outcome, terminal_ms}` (capped
   like the registry's other retained sets; a full cap refuses, never
   evicts). The settlement fact is therefore atomic with the row removal:
   one image, one authority, no window in which rows are gone but the fact is
   not durable. Today's best-effort tombstone (`append_tombstone`,
   `prune.rs:7585`) stays a diagnostic outside this protocol — it was never
   atomic with the removal, and a diagnostic is not promoted to an
   unaccounted recovery ledger.
6. Barge re-takes its locks and records `complete`.

Between-revision and lost-ack traces: a concurrent dispatcher serializes on
the guard and re-runs the same verification — the settlement entry makes the
second pass a no-op ack. A selection change between marking and removal is
caught by step 5's fresh re-verification (refuse, stay owed; marked rows are
non-selected and harmless until removed). A lost acknowledgement re-dispatches
into step 2: members absent **with this operation's settlement entry** → ack;
members absent without it, a partial group, or an exact-set mismatch →
`Unknown`, stay owed. Every interruption converges to settle-then-ack,
because the settlement fact cannot lag the removal it authorizes.

### Crash traces

| Window | Fact after the crash | Next actor |
| --- | --- | --- |
| begin refused pre-publication | nothing anywhere | a later prune re-runs eligibility |
| begin publish sync unverified | `Unknown`; record may or may not exist | caller keeps the root excluded; no effect preceded it; same-key retry or fresh key both safe; a captured unit can never be re-begun |
| begin published, re-affirmation pending | record at `intent` | the caller's re-affirmation decides; conflicting commands refuse at the in-lock consult |
| capture rename → `removed` not yet durable | record at `marker_last`, the leaf's capture recorded; the recorded private name gone reads `Unknown` | pending, exactly as shipped; never acknowledged |
| Barge's unlink done, `removed` durable | positive evidence in the record | the next drain dispatches; the handler settles; ack recorded |
| pre-effect cancellation decided | record transitions to `cancelled`, retained | non-binding; a fresh attempt uses a fresh key; the gapless inventory is unchanged |
| rows removed, settlement entry durable, ack lost | registry settled, record still owed | re-dispatch: absent members + settlement entry → ack |
| handler unregistered / version mismatch | owed, intelligibly named | pending with caller, key, kind, version; never guessed, never executed unloaded |
| sequence gap / binding or epoch broken | `Unknown`, never "no operation" | delegated refusal until operator resolution |
| volume absent | pending; the consult refuses | attach; nothing inferred |

### OLD → NEW removal map

| Today | Reset | Verdict |
| --- | --- | --- |
| schema-3 `deletion_routes` and Barge's route reader (`DeletionRoute`, `reference.rs:525`; `OXIDE_REGISTRY_V3`, `:70`) | routing recorded once in the record | removed, both sides |
| `prune-delegation/1` sidecars; `RequestStore` (225 lines) + `prune_request.rs` (334 lines, `Ready`/`Unavailable`) | the record's typed payload, persisted at begin | removed; the `Unavailable` variant and the absence-only lookup (`lookup_absence`, `oxide_outputs/absence.rs`) go with it |
| covered-path `prune-intent/1`; four-state `lookup` (`oxide_outputs.rs:338`); the same-call `Locked` rule; reply correlation (`prune_barge.rs:154`–`:336`, `:593`) | one idempotent begin/state/drain answer | removed from the covered path; Oxide keeps its journal for local roots |
| eligibility re-read in `execute` (`lookup_oxide_intent`); the guarded revision CAS (`prune.rs:7118`) | guarded verdict at begin + guarded re-affirmation by revision | preserved and moved to begin, against every registry writer, admission never across a Barge wait |
| adapter-only pre-effect replay (`delete.rs:385`) | `resume: caller` two-tier policy | preserved as policy on the record |
| `unsettled_for` consulted before the binding (`outputs.rs:6237`, `:4486`) | one bounded in-lock consult at the same sites | simplified; one authority, atomic handoff |
| paired-pool `/4` upgrade (`store.rs:2702`–`:2865`) and schema-3 validation | one-time adoption fence only | reduced; per-operation upgrade machinery removed |
| Barge caller fields + `validate_caller` (`state.rs:1622`–`:1629`, `:1794`), `CallerCorrelation` (`:1654`), `check_binding` (`oxide_outputs.rs:440`) | generic caller block; the record is the binding | generalized |
| `Outcome`'s six kinds; `BargeLookup`'s four states (`oxide_outputs.rs:223`–`:337`) | `Begun` / `OperationState` / `DrainReport` | replaced |

Retained: Barge's whole safe deleter, replay windows and positive-completion
evidence rule (`delete.rs`, stage 1 design); the `oxide_outputs` marker family
and area grant (`unit.rs:141`, `:326`); settled records and the row and byte
caps; the fence and old-reader refusal; Oxide's registry/receipt/tombstone
history as domain state; Oxide's local prune path with its own journal; on the
caller's side, eligibility policy, the binding serialization and guarded verbs
(`CleanMode`, `store.rs:525`), the guarded verdict, the re-affirmation pass
and the whole-group recheck (`prune.rs:7118`, `prune_barge.rs:336`), the
checkout-absence re-proof at begin and every replay, live and unknown-writer
refusal, grant revocation (a revoked grant never resumes), whole-root group
semantics, no local-delete fallback, and the exact root/volume/request
binding.

### Accounting, on the same basis, added machinery included

**Authorities:** two reconciled journals → one. **Per-operation durable
artifacts:** three (registry intent-and-route image, sidecar, Barge record) →
one; the epoch/watermark document and the identity binding are two static
documents per state directory, not per operation, and are counted here as the
integrity inventory. **Coordination writes per covered operation**
(released-receipt case): before — sidecar, registry intent-and-route image,
`complete_intent` marking, `settle_receipt` removal, five Barge phase records
= **nine**; after — six Barge records (`intent`, `renamed`, `marker_last`,
`removed`, `deleted`, `complete`), the handler's marking revision, the
removal revision **with its atomic settlement entry**, and the begin's
watermark advance = **nine**. Writes are flat on the clean path: the
watermark advance and the settlement entry are the priced machinery of
proven loss detection and a safe repeated ack, and they replace nothing; the
re-affirmation's revision update is one **conditional** write, taken only on
the advanced-revision path, counted here. Retention is the other counted cost:
numbered records are never removed, so `cancelled` records accumulate like
settled ones, and the registry's `settled` set is bounded — a full cap
refuses, never evicts. **Phase states:** eleven across two stores (six Oxide,
five Barge) plus a receipt-settled flag, a route bit and four sidecar wire
states → seven in one store (`intent`, `renamed`, `marker_last`, `removed`,
`deleted`, `cancelled`, `complete`) with no record removal.
**Caller obligations:** sidecar/intent/route publication, first-vs-resumed
`Locked` discrimination, four-state lookup, absence-only lookup, recovery
dispatch, dual resolve prechecks, compaction and upgrade orchestration
(seven) → guarded verdict, begin, re-affirmation, one idempotent handler,
one in-lock consult (**five**). **Recovery boundaries:** the amendment's
twelve → ten traced windows in one store. **Added machinery, named:** the
sequence watermark, the identity binding, the gapless read rule, the
operator gap-resolution command, the re-affirmation pass, the handler's
atomic settlement entry, and the **six writer-side fail-fast consults** (one
per enumerated publication site — reads, not writes) with their refusal path:
a publication that cannot take the try-shared lock immediately, or that sees
a conflicting pending record, refuses and is retried by the writer's ordinary
retry — the availability cost of the consult, priced here. Each named piece
replaces a larger removed piece except the watermark advance and the
settlement entry, which are priced above. The net reduction survives on
authorities, artifacts, states, obligations and removed machinery; it does
not survive on write count.

### The counterexample, and the exit

The brief directs a negative result where the candidate cannot deliver. The
shaping counterexample: a journal cannot detect the loss of an individual
record from its content alone, nor the recreation of its directory from a
document stored inside itself — so the reset carries a watermark, a gapless
read rule, and an identity binding outside the journal. That inventory is
genuinely added machinery, priced above (+1 write on the advanced-revision
path, two static documents, one
operator command, one re-affirmation pass); what it buys is loss and
recreation detection the shipped design does not have, and the removal of the
sidecar store, routes, schema-3 validation, the lookup pair, the same-call
rule and the upgrade orchestration. If the retained review finds that
inventory equal to or greater than the join it replaces, the honest exit is
this section's negative result: the shipped two-journal protocol remains, and
the smallest genuinely simpler alternative does not exist within one journal.
The fallback in every other respect is the shipped execution unchanged.

### Compatibility and staged delivery

Barge051–055 stay historical; PR 56's shared scanner stays an orthogonal
preserved candidate; Oxide PR 263 stays unmerged and preserved — if it
merges, its machinery is this map's OLD column, and the scoped caller work
then removes it. No live schema-3/fence-4 migration is assumed; none has
shipped. An older Barge refuses a successor record major as unknown state;
an older Oxide refuses at the adoption fence; the identity binding is a new
unnamed state file that shipped readers never read; new format numbers are
set by implementation briefs, not guessed here. Pending two-journal
operations drain under the shipped protocol before any successor format
activates — the no-unfinished-predecessor rule, applied once. Implementation
splits into: (1) Barge prerequisites — the operation record major (`resume`
policy, sequence, payload budget, shipped evidence phases); the epoch and
identity-binding documents with the gapless read rule and the bounded
operator resolution command; begin/state/drain with the fail-closed rules
and the `try-shared` observation mode the consults use; the narrow
post-capture replay extension; regressions per trace; (2) Oxide caller work
— the guarded verdict, begin and re-affirmation under the binding, the
writer-side fail-fast consults at all six publication sites (`adopt`,
`bind_for_command`, `reserve`, the local prune's intent publication, the
settlement revisions, compaction), the handler (mirroring `complete_intent`,
`settle_receipt` and its atomic settlement entry), and the removals in the
map; (3) bounded validation — crash-injection fixtures per window including
sequence-gap loss, binding destruction, uncertain publication,
historical-restoration refusal behaviour, the interleaving traces both
directions, concurrency traces, consult refusals under a momentarily locked
journal, oversize, unregistered-handler, mismatch and refusal cases. Named
risks: the journal gates Oxide admission (loss conservatively blocks;
refuse-safe); publications refuse while the journal is momentarily locked or
conflicting (refuse-safe; retried); payload-budget refusals; retention
growth; the operator-resolution dependency for proven gaps. No native
apparatus is authored or activated by this design.

### Owning-contract reconciliation text for Oxide (not edited here)

> In `barge-adoption.md`, the sections "Persistence amendment for independent
> review (oxide#304)" and "Recovery, retention and crash boundaries" are
> superseded for the covered class by Barge's stage 4 design, "One operation
> journal for delegated recovery": the caller keeps eligibility policy —
> computed under its publication guard, released before Barge is called, and
> re-affirmed by registry revision before any effect — its binding
> serialization and guarded verbs, the whole-group recheck before an
> `intent`-phase effect, and one idempotent finalize handler mirroring
> `complete_intent` and `settle_receipt`'s released-root branch with the
> atomic settlement entry as its settlement fact; it moves its exclusion
> consults inside every registry publication's own guard (a fail-fast,
> non-blocking journal read at each publication site, refusing while a
> pending operation binds what it writes), and drops route publication, the
> request sidecar, the covered-path intent journal, the absence-only query,
> the same-call rule and the per-operation upgrade. `publication_guard` is
> the internal pool's admission lock; it is never held while blocking-waiting
> for a Barge lock — the only Barge touch under admission is the
> publications' fail-fast consult, which gives up rather than waits. The
> adoption-time fence and old-reader refusal keep their meanings.

**A second caller does not rebuild the protocol.** A hypothetical Effigy
scratch delegation: declare the area and its grant; take the run-scope
serialization; compute the eligibility verdict under the guard and release;
`begin(caller: "effigy.scratch", key: <run id>, finalize: "conclude-run/1",
payload: the run ids within the budget)`; re-affirm by revision; register one
handler that concludes its run roots with the same guarded complete-group
re-verification and a settlement entry. Exclusion, loss detection, recovery
and finalization come from the same journal, the same gapless rules and the
same drain — no routes, no sidecars, no lookup pair, no Oxide protocol
rebuilt.

**Acceptance recommendation (decisive).** With the cross-checkout handoff
proven by writer scope and serialization, cancellation made a retained
transition, settlement atomic with the removal, and the coordination priced,
the reset protocol is deliverable and this task's acceptance recommendation is
**adopt it** as the target architecture for delegated recovery, through the
implementation split above under separate briefs and review. The negative
exit stays real, not rhetorical: if the retained review finds a concrete
counterexample — an unresolved crash or concurrency window, or an integrity
cost equal to or greater than the join it replaces — the section's negative
result applies and the shipped two-journal protocol remains; a design merge
approves no implementation, migration, native execution or adoption.
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
| F6 | Oxide declares its output areas and switches its prune to Barge's gate; the 91 legacy roots are released with an operator list. Barge slice R2 supplies the release-class policy those areas declare | F1–F5, the [stage 3 space policy](stage3-space-policy.md)'s slices, Oxide's own project, `ff13f2ef`, operator approval |

`F1` and `F2` can run in parallel; `F2` needs nothing from `F1` except a shared
descriptor-walk helper, which now sits beside S3's containment code either way.
`F5` is read-only and could land before `F3`, which is useful because it produces
the evidence for the finding while the gate is still being built. Reserve and
floor pruning is not a slice here: the [stage 3 space
policy](stage3-space-policy.md) owns it, and this contract contributes only the
identity, custody and policy conditions a space prune already supplies. Nothing
depends on a real drive; `F6` needs its own operator gate because it changes what
happens to data on a real volume.

**Release-class policy (Barge slice R2).** `PrunePolicy::ReleaseOnReference` has
landed as an additive `barge.area/1` value named `release_on_reference`, so an
area whose roots may only be released by reference can be declared before the
effect that releases one exists. Two rules make it safe to declare early, and
both are tested with the policy present:

- `maintain` never plans one of its roots. The area is a plan step reported as
  excluded, with the reason and no candidate, so its absence from the deletion
  set is stated rather than silently omitted. A `prune: none` area is not a step
  at all.
- the space path never enumerates or prunes one of its roots, whatever the
  area's `reserve` records. The policy, not the reservation, is what keeps it
  out of the reserve, because only the release gate may release it.

Condition 4 of the [release gate](#releasability-the-one-gate) reads
`release_on_reference` as permission by release only when a release-by-reference
effect exists. That effect is a later slice, so this policy currently releases
nothing: its roots stay, and every report of them says why. An older reader of
`barge.area/1` refuses the value as unknown rather than reading it as `none` or
as an age the area never declared.

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
The verb refuses every form that is not the plan. (Historical, at F3: the effect
needed F4's intent evidence, its replay and its own authorization. F6-R1 added the
release form; see [F6-R1](#f6-r1-landed-surface).)

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
cannot report in full is never a short set. A row's `references` list is
**required**, not defaulted: a row that omits it is not a document this reader
understands, while an explicit empty list is a complete reading that says nothing
names the root. The scan's byte evidence keeps two facts apart: `bytes.logical` is
the sum of **regular files' own sizes only** — a directory or a symbolic link
contributes nothing, and the root directory itself contributes no logical bytes —
while `bytes.allocated` is the allocation the scan counted for everything it
reached, the root directory included.

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

Reference matching is remount-aware, for the same reason root identity is: a
reference recorded before a mount-point change carries the old absolute path, so a
record names this root when its path is the same path **or** when it sits below a
`/Volumes/<mount>` prefix and the components below that prefix are exactly this
root's mount-relative path. The mount-relative path is the identity and the absolute
path is only an observation, so a current Oxide registry row, receipt, prune intent
or Barge placement recorded at the drive's earlier mount name still keeps the root;
matching it is the safe direction, and a path with a different relative path, or one
outside the mount parent, names no root here.

What the reference readers read, and where. Barge's own: every placement record
(the whole census, so a truncated one is `Unknown`), the area declaration the
plan is built under (current when its policy never releases a root), and the
root's hold records, which the caller reads under the root's own lock (a live or
undecidable hold is current; a positively stale one is not). Oxide's, read in place and never written: the outputs registry
`oxide.outputs.registry/3` and its `/2` and `/1` predecessors, every checkout receipt
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
`revision`, `complete` flag, `created_by` and its `rows` array are required — an
explicit `rows: []` is a complete reading that says there are no rows, while a
registry that omits the array is divergent — an incomplete registry
must carry Oxide's own `incomplete` record (and a complete one must not), each
registry row must state Oxide's selection flags (`selected`, `pending`) and pass
Oxide's own `Row::validate` — a lowercase 64-hex checkout id, bounded `member` and
`recipe` text, absolute `root`, `checkout` and `receipt` paths, and a pruning row
never also selected or pending — and every root kind any of these records states — a
registry row's, a prune intent's, a receipt selection's, a history entry's, a
prepared placement's, a reservation's or a migration journal root's — must be one
of the two Oxide writes (`external` or `local`). Each unfinished prune intent must
also carry the root identity Oxide pins, pass Oxide's own intent validation (a
lowercase 32-hex id, a lowercase 64-hex checkout id, absolute receipt and quarantine
paths, a 64-hex plan digest, an empty-or-64-hex preimage and a sorted unique group)
and state one of Oxide's six phases. Every root path a receipt names is a **usable absolute**
path in Oxide's own `safe_absolute` sense — absolute, with no `.` or `..` component —
for selections, history entries, prepared destinations, reservations and journal roots
alike, and the same rule holds for the registry row's and the prune intent's root,
because those are the paths this reader matches. A relative path, or one with a parent
component, can never be the component chain of a root Barge walks, so it is divergent
rather than a reference that names nothing. A document
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
expired bound there — after a candidate's own metadata lookup as much as before it,
and on the absence of a receipts folder as much as on its contents — is unknown,
never "no Oxide records". "There is no control directory" is a complete look's
conclusion: the time each candidate takes is charged to the same bound, and the
empty answer is drawn only while the observation is still inside it. The plan passes
this bound as `ReleaseBounds.reference_wall` (the design's 10 s), so an expired one
leaves every root of the area with an incomplete reference set. More than one distinct consumer path for one root, or two records that
disagree about one path's volume, is also unknown and keeps that root: the plan
proves what it can name exactly once.

The last-activity clock. `OldestIdleAfter` compares `now - last_activity_ms`,
and `last_activity_ms` is now the newest modification time found by a bounded,
descriptor-relative scan of the root's subtree, with the root directory's own
modification time as its floor. ****The floor must exist**, and it is the root directory's own
modification time **read now**, through the descriptor the scan holds: an earlier
observation of the same directory — a listing taken before a hold was released, or
the plan-time reading an effect-time revalidation follows — is never used in its
place, so a root that changed since is not reported as old while a fresh time is
never masked by an older one. A root directory that reports no such time leaves the
age unknown, because its entries' times can be decades old while the directory
itself changed recently, so a child's time is never used in its place. The scan never follows a symbolic link, and a
modification time ahead of this run's clock, an entry that reports no modification
time at all, an unreadable subtree, a name that exceeds one listing's bound, a
depth past 64 levels, or its deadline leaves the age unknown too, which keeps the
root. The deadline is checked before the scan, before every listing and again
before the scan reports a complete observation, so a listing — including one that
answers empty — cannot consume the bound and still leave a complete, old-looking
age behind. The age is measured against **this plan's** clock, and an activity time
ahead of that clock is unknown even when the scan (which compares against the clock
it read, a moment later) did not see it as future: a timestamp that falls between
the plan clock and the scan clock is not a known age of zero. A missing modification time is never read as zero: zero would make a
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

The record's three outcomes stay apart in every row: a record that was **read**, one
that is **positively absent** (no `identity.json` where the id says it should be), and
one whose read **failed** — an unreadable or symlinked file, one over its cap, one that
changed while it was read, and a schema-divergent or non-canonical document. Only the
second is `identity_record_missing`; a failed read is `identity_record_invalid`, as the
[identity section](#where-identity-is-written-and-what-happens-when-it-is-missing)
requires, and both keep the root. Another root of the same area is unaffected by
either: one undecidable record never blocks an unrelated row.

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

### F4 landed surface

`barge.intent/2` gained the two optional documents this contract gives it, and the
reader, the writer seam and the replay know them. (Historical, at F4: it gained no
effect verb, and the slice authorized no delete of any root. F6-R1 added the
release form of `barge prune` and the first writer of those documents; see
[F6-R1](#f6-r1-landed-surface).)

What a record can now carry. `identity` is condition 2's evidence exactly as a plan
row prints it — verdict, optional reason code, inode, both devices, the remount and
mount-moved facts, the marker digest — and `absence` is the `barge.absence/1`
document itself, with its own `proved_ms`. Both live in `evidence`, which the plan
and the intent now share, so one reader decides both documents: a verdict outside
the design's set, a reason outside its table, a `verified` identity that does not
carry its inode, both devices, the remount and mount-moved facts and the marker
digest, an `absent` proof without the ancestor and leaf that prove it, an
`unknown` **absence** without a reason, a `present` walk that carries an ancestor or
leaf, a path that is not absolute with named components only, or a document over
4 KiB is unknown state. An `unknown` **identity** needs no reason, because a plan row
that could not observe a root reports exactly that: it says nothing was decided, and
it keeps the root. `read_prune_plan` reads them through the same validator, which is
a stricter reading of the plan than F3 left and not a change to its field set.

What an intent may state. An intent names a unit only when every condition of its
gate held, so a recorded identity is `verified` and a recorded absence is `absent`; a
record that says otherwise is refused on read. The identity evidence must also
describe this unit — its inode and the exact marker bytes the record carries — and a
unit that carries it is a Barge-marked directory. Both documents are optional:
every `/2` record written before this slice, and every `/1` record, still reads
with neither, and `barge.intent/1` maps to no evidence because it recorded none.
The writer seam is `state::IntentEvidence`; `delete_unit_authorized` publishes it
with the intent before the first effect, and `delete_unit_on_volume` passes none, so
the scratch sweep and the space prune are unchanged. The release effect, when its
own brief authorizes it, supplies the plan row's two documents there and nowhere
else.

Replay under a changed device. A replay never finds its volume through the
registry's mount hint and never compares a durable fact to the device the intent
recorded. It asks the platform which live mounts carry the recorded UUID (F2's
locator): none is `Unknown` — an absent drive is not a deleted root — and more than
one is `duplicate_volume_uuid`, refused for that intent before any mount is chosen,
because a decision between two mounts is never made. The one candidate is verified
as the volume and as a real mount root, and the area folder is walked from *that*
live mount through the same containment walk, so a reconnect that moved the mount
point settles on the UUID and the components below it while the stale hint is only a
stale observation.

Every step of a replay keeps stage 1's mode order: the modes a stopped run
recorded are restored first, before any marker is read — the changed-device proof
reads one — so an intent whose identity cannot be re-established still leaves the
captured object with the mode the record proves, never with the widened permissions
the deletion needed. The object the restore chmods is matched on its recorded inode
and the volume's live device, never on the device a capture recorded before the
remount: that number belongs to the mount the record was written under, so
comparing to it would refuse the restoration exactly when it is needed.

What a changed device is re-proved from depends on the phase, because each phase
acts on a different object and `marker_last` has already removed the marker the
earlier ones compare. `removed` and `deleted` rest on the durable phase — Barge's own
recorded final unlink — and not on an object's identity at all. `intent` acts on
whichever name holds the unit: the unit itself, where the rename never happened and
the intent rolls back, or its capture, where it resumes. `renamed` acts on the
capture with its marker in place, and `marker_last` on the capture whose marker must
be *positively absent*, which is that phase's own proof. In every case the object
must be the recorded kind and inode on the volume this run verified; below
`marker_last` its marker must be the exact recorded bytes (which pin the root id,
nonce and volume); at `marker_last` the marker must be gone and the inode and kind
are the identity. A markerless unit therefore settles on its inode, its kind, the
verified volume and the intent's own 128-bit private capture name — stage 1's own
leaf rule minus the device. An intent that carries identity evidence must also have
its durable record agree: root id, nonce, mount-relative path, inode and kind, and
the generation the record was written at; at `marker_last` the marker's own facts are
gone by design, so the evidence's inode and the acted-on inode are what remain. An
object that is anything else — a different inode, a leaf that cannot be read, a
marker that is not the recorded bytes — keeps stage 1's within-run device, so the
change is `Unknown` and the intent stays pending with the reason. The replayed
intents report how many resumed deletions were re-established this way, and a settled
intent's record keeps its evidence as the diagnostic a later run reads.

The `barge.root/1`-to-`/2` settlement. A pending intent written by an older reader
records a `barge.root/1` root — S3-era, so no durable identity record — and the
marker writer has since moved to `/2`. F4 settles that intent without rewriting
anything: the marker is read by its own version's rule (F2's prefix relaxation
included), the recorded inode and the exact recorded marker bytes are the proof, and
no code writes a v1 marker, demands a v2 one, or invents the identity record O4-002
keeps explicit. The same rule settles a `/2` record that predates the evidence, so
the carried finding's own scenario — a reboot between the intent and its replay —
no longer strands a correct intent inside Barge.

Absence is re-established, never reused. At the `intent` phase, when the rename
already happened and the replay is about to resume the effect, the recorded
`barge.absence/1` proof is proved again from the anchor: a path under a registered
volume is re-prefixed with the live mount located by the recorded UUID (the
components below it unchanged, as the reference matcher already does), the ancestor
is proved on that volume, and only a fresh `absent` lets the deletion resume. A path
that reappeared keeps the intent pending and unlinks nothing; a volume that cannot
be located is unknown, not absent. A phase past `renamed` has already begun the
effect, so the proof is not a gate there and is not consulted.

What the release effect's own brief still owns: the verb, its authorization
(the plan's exact bytes and digest), the effect-time re-establishment of every
predicate in order, and the operator gate that precedes a real run. Nothing in F4
changes what may be deleted, and no CLI verb changed. The documents that did gain
fields are the intent's two optional ones and, additively, the replay report's
`remounted` count inside `barge.scratch.expire/1`, whose human summary names it only
when it is not zero; the CLI's only other edit is the absence seam the replay's
platform object already supplies.

### F6-R1 landed surface

The release effect exists: `barge prune --plan <file> --plan-sha256 <hex> --area
<namespace>/<area> --volume <uuid>` runs it. It is the only path that deletes a
whole owned root for release. Nothing calls it: `maintain` and the space prune
never reach it, and no host job runs it. Oxide's `R1` slice, its adoption and its
operator gates stay in Oxide's own project.

**The run.** `release::execute` takes the presented bytes and digest and does
this, in order. Each batch-wide refusal changes nothing.

1. The bytes must hash to the digest, read through F4's strict reader, and
   re-serialise to the same bytes. The plan's area and volume must be the
   command's.
2. The gate runs again for the same area (F3's `plan`), with the presented plan's
   own named roots, before the volume lock, because a plan takes that lock shared.
   The volume is found by its recorded UUID (`MountLookup::ByUuid`), not by its stored
   mount point: a remount under another name is the same volume, one live mount is
   verified and opened as the descriptor, a stored point that another volume occupies
   is never trusted, zero live mounts is `volume_absent`, and two are `unknown` with
   exit `3`. Custody, the area folder and the deletion itself resolve the same way,
   so the effect re-proves the live volume. A volume that is not attached, a volume
   with an undeclared durability, or an incomplete Oxide reference set refuses the
   whole batch.
3. Each presented eligible row must agree with the fresh row on identity verdict,
   inode, marker digest, references and absence verdict, and the fresh row must
   still be eligible. A row that does not is kept with its reason.
4. The run takes the volume's exclusive lock and replays pending intents on the
   volume (F4), whether or not any row is a candidate, so a later run settles an
   intent an earlier run left. Then each candidate's deletion takes its unit's lock, and before
   the intent is written, `reprove_row` re-reads under that lock: the marker and
   inode the row named, the durable identity record's contents, the policy and its
   age, every reference (holds, placements, Oxide's registry, receipts and marker),
   and any consumer path's absence. Identity is decided again by the plan's own
   rule, against the record's volume, root id, nonce, path, kind and area, so a
   record that names another volume or another root keeps the root. Any change
   keeps that root and writes no intent.
5. The intent is `barge.intent/2` with the fresh identity and absence evidence.
   Its `plan_digest` is the presented digest, which is the permission. The rest is
   S5: custody after publish, the quarantine rename, the whole-unit delete and the
   replay.
6. An effect whose state is uncertain stops the batch. An error after an intent
   could exist, or an intent left pending, stops it and reports the rest as
   `not_attempted`. A refusal the re-proof made came before any intent, so the
   batch goes on.

**Condition 4 and `release_on_reference`.** `release_on_reference` now permits a
release by reference, with no age gate. The area's reference (condition 5) is
current only for `none`. A known last activity is still required, because an
unknown activity keeps a root everywhere in this gate. `oldest-idle` keeps its age
gate, and `none` releases nothing. `maintain` and the space path still never
consider a `release_on_reference` root (slice R2, in the slices section above).

**The named-root waiver.** `barge prune --plan --root <root-id>` (repeatable) names
the roots an operator waives the age gate for. It waives that one condition and
nothing else: custody, durability, identity, references, absence and the policy's
own permission all still apply, and a named root's last activity must still be
known. An `oldest-idle` area is the only one whose age gate it waives; a `none`
area still releases nothing, and a `release_on_reference` area has no age gate to
waive. The plan states the waiver: `named_roots` is the sorted, unique list of root
ids the operator named, and each row the waiver stood in for carries
`age_waived: true`. The digest covers both, and the effect plans with the presented
plan's own named roots, so the same waiver is decided again. A name the area does
not list refuses the plan, and a waived row whose root the plan does not name is a
document the reader refuses. A real run takes no `--root`; the presented plan
states its own. The run never reads a root that a plan did not list, and it never
settles an Oxide intent.

**Output.** `barge.prune.run/1`: `plan_digest`, `fresh_plan_digest`, `run_ms`,
`area`, `volume_uuid`, `replay_pending`, `rows` (`released`, `kept`, `pending`,
`error` or `not_attempted`, each with its reason and intent), `released_count`,
`kept_count` and `stopped`. Exit `3` is a pending or errored row, a stopped batch,
or a replay that left an intent pending.

**Seams.** `release.rs`: `plan` wraps `plan_observed`, which returns each row's
`ObservedRoot` (its unit, path and mount point) beside the report; `decide_root`
keeps the unit and carries `permits_release` into condition 4 and the area
reference; `execute` runs the checks above; `delete_candidates` holds the volume
lock, replays, and calls `delete::delete_unit_under_volume_lock` with `reprove_row`
as its `authorize`; `reprove_row` is the effect-time re-proof. `delete.rs` is
unchanged. `reference.rs` changes one detail string. `barge-cli` `prune_plan`
dispatches bare `--plan` to the preview and `--plan <file>` with a digest to the
run. `area.rs` `MountLookup` and `VolumeRoot::open_with` carry the lookup: the release
gate, its custody and its deletion use `ByUuid`, and every other caller keeps `Hint`,
the stored point, which is unchanged. `identify` still requires the stored point to
match the census, because it is an explicit repair command, not the release path. The
proof build's only seam is `os/macos.rs` `tool_account_home`, which reads
`os/proof_account.rs` under its feature.

**Case table.** Each case is carried by a named regression. The apparatus's
`release:native:test` checks every name against the source.

| Case | Claim | Carried by |
| --- | --- | --- |
| C1 | volume verified and attached; an absent volume plans nothing | `an_absent_volume_plans_nothing_and_a_permanent_one_is_loud` |
| C2 | identity verified and its record present; each one alone keeps | `all_six_conditions_release_a_root_and_each_one_alone_keeps_it` |
| C2e | marker and inode re-proved under the unit lock | `the_unit_lock_re_proof_refuses_each_condition_it_re_reads` |
| C3 | custody safe; a live hold keeps | `the_unit_lock_re_proof_refuses_each_condition_it_re_reads` |
| C4 | policy permits; age passed; `release_on_reference` has no age gate | `a_release_by_reference_policy_releases_a_root_without_an_age_gate` |
| C5 | no current reference from a complete set; a pending or unsettled Oxide row keeps | `an_oxide_reference_that_is_not_settled_keeps_the_root` |
| C5c | an incomplete or divergent reference set keeps the batch | `an_incomplete_or_divergent_reference_source_keeps_the_whole_batch` |
| C6 | consumer path absent; a reappeared path keeps | `an_oxide_reference_that_changes_after_the_plan_keeps_the_root_or_the_batch` |
| X1 | one synthetic root released from the presented bytes; replaying the plan releases nothing | `a_release_run_deletes_one_root_the_presented_plan_allows_and_replaying_it_releases_nothing` |
| X2 | a changed digest, non-canonical bytes or another area refuses | `a_changed_digest_a_non_canonical_plan_or_another_area_refuses_and_changes_nothing` |
| X3 | a foreign or unknown marker kind cannot qualify | `a_marker_with_an_unknown_kind_cannot_qualify_its_root` |
| X4 | each condition that changed after the plan keeps the root and writes no intent | `a_run_keeps_a_root_whose_condition_changed_after_the_plan_was_written` |
| X5 | the intent is written before the rename; a remount replays it | `an_intent_is_written_before_the_rename_and_a_remount_replays_it` |
| X6 | the CLI refuses a digest that is not the presented bytes' | `prune_run_refuses_a_plan_whose_digest_or_bytes_are_not_the_presented_ones` |
| X7 | a named root waives only the age gate; custody and policy still keep, the plan states the waiver, and an unlisted name or an unnamed waiver refuses | `a_named_root_waives_only_the_age_gate_and_the_plan_says_so`, `a_named_root_must_be_listed_and_a_waiver_must_name_its_root` |
| X8 | identity re-decided under the lock against the record's contents | `the_unit_lock_re_proof_refuses_each_condition_it_re_reads` (case `identity record contents`) |

**Proof build.** The first post-merge diagnostic, at PR #38's head `2ab52c7`, was
unqualified: eleven of twelve cases failed. The ordinary CLI resolved Oxide's
directories from the password database's home, so `BARGE_ACCOUNT_HOME` and the
private `HOME` did not move them. It chose the real account control, whose version-1
receipt makes the reference set incomplete. That result is historical and grants no
native credit. The fix is a proof-only build, not a change to the ordinary one.

The `proof-synthetic-account` feature (library and `barge-cli`) is off in every
ordinary build. Under it alone, `os::macos` takes Oxide's state and cache directories
from the synthetic account home, and only for the `oxide` tool. A `proof_account`
module validates the bindings first. All five must be present, and any absent one
refuses rather than falling back to the real home:

- `OXIDE_TASK_SCRATCH_ROOT` and `OXIDE_PROOF_EVIDENCE` are real directories owned by
  this account. The evidence directory need not sit inside the scratch root.
- `BARGE_ACCOUNT_HOME`, `HOME` and `BARGE_STATE_DIR` are real directories owned by
  this account, and each resolves strictly inside the scratch root. A symlink at any
  of them, or an ancestor that resolves outside, refuses.
- `HOME` resolves to the same directory as `BARGE_ACCOUNT_HOME`.
- The synthetic home does not lie inside the password database's home.

Nothing else changes in that build. Release gates, plan parsing, deletion, custody
and the incomplete-reference rule are the ordinary code. The ordinary build ignores
`BARGE_ACCOUNT_HOME` for Oxide's directories, and a unit test pins that.

**Native proof: built, held for owner activation, not run.**
`scripts/native/release-proof.py` carries the bounded native body. It runs under
Oxide's qualified shared supervisor, pinned to the same bytes the S6, S7 and space
proofs name, with a 300 s wall and 512 MiB of scratch. Its fixed command is that
script and its mode: the binary under test is never a caller argument.

**Route and staging.** New proof work runs on `/Volumes/Stash/scratch/barge` under the
verified volume UUID `8A660440-C919-45C6-A3F1-3365B9BD2006`. The outer half first
reads the installed supervisor's bytes against the pin (an unreadable or different
supervisor refuses) and then the scratch route: the mount's UUID, the canonical
path, this account's ownership and an owner-only mode. A drive root, a home folder, a
symlinked path or another volume refuses before anything is staged, and nothing is
probed for a verdict. Historical Builds receipts and the `/Volumes/Builds` scratch
that earlier plans named are unavailable history: no receipt is reconstructed from
them and none is moved.

The guard's own child then stages the proof-feature build. It runs
`oxide build -p barge-cli --locked --offline --features proof-synthetic-account`
with `RUSTUP_TOOLCHAIN=1.97.1`, `RUSTUP_AUTO_INSTALL=0` and
`CARGO_NET_OFFLINE=true`, drops both Rust flag variables and
`DYLD_INSERT_LIBRARIES`, drops the guard's own scratch and evidence hooks so the
build is an ordinary managed Oxide compilation rather than a nested proof
session, points `TMPDIR` at the guard, and copies the built `barge` artifact into
one exclusive directory inside the guard's owned scratch root
(`OXIDE_TASK_SCRATCH_ROOT`). The private fixture run root is created later, inside
that same owned scratch, so the staged copy and the fixture share the guarded root
without the staged copy living in the fixture tree. The compile itself keeps the
real account's `HOME`, `CARGO_HOME` and rustup home, because it is the ordinary
managed Oxide build the brief allows; only the fixture children are bound to the
private home and state, and the compile writes nothing into the fixture. It hashes that
copy, writes its manifest (head, features, toolchain, build command, digest) and
re-checks the manifest and the digest before any effect. Staging and its waits spend the same
300 s command bound and the guard's own clock: the outer half publishes its absolute
monotonic deadline and the child never resets one. There is no unguarded prewarm, no
shared stage directory and no accepted artifact from another run; `release:native`
never reads what `release:native:stage` staged. The ordinary production build
(without the feature) is never the one run.

The inner half re-checks that it is the pinned supervisor's guarded child: the
supervisor bytes still match the pin, the carried scratch route is the pinned one,
`TMPDIR` resolves inside the scratch directory, and the guard's monitored layout is
this occurrence's own. That layout is the pinned supervisor's, not an assumption:
it creates one `oxide-proof.*` root directly under `--root-parent`, then `scratch/`
and, by default, `evidence/` inside that root, so `OXIDE_TASK_SCRATCH_ROOT` and
`OXIDE_PROOF_EVIDENCE` are siblings inside one monitored owned root and evidence is
**not** inside the scratch directory. The check therefore requires both directories
to be absolute, canonical, symlink-free, this account's and private (no group or
other write; the owned root itself owner-only), the owned root to be a direct child
of the verified Stash scratch parent, and the evidence directory to sit inside that
same owned root without aliasing the scratch directory or the owned root. An aliased
or symlinked path, an escape, a foreign or off-parent root and a mismatched occurrence
all refuse. A refusal there returns `125` with no staging and no fixture.

**Custody of the image.** Before the 64 MiB sparse APFS backing exists, the run
publishes the guard's opt-in `image-custody.json` indication (its exact shape:
`version` and one canonical backing inside the owned root or evidence directory).
That is what keeps the root safe when an attach is interrupted: the guard's own
cleanup reads the indication, and a backing that is mounted or attached-but-unmounted
keeps the root, its backing and the evidence instead of being removed.

The attach itself must be positively the owned image: exactly one entity of that backing
mounted at this run's unique `/Volumes/<owned-name>` mount, with the device the attach
child reported, the mounted volume UUID, and `Owners: Enabled`. The census is read as
every system entity of the backing, the container entity plus its mounted volume, the
same shape the guard's own census and the reviewed space pattern read; entity count is
not identity. A second entity of the backing that carries a mount point, a duplicate
mount, a mismatched device, and a failed, partial or unreadable attach are each unknown
custody; nothing is detached, repaired or retried. The same rule decides the pre-detach binding
and R11's remount, and clearance after a detach is zero entity rows for the backing.

**Closure.** Each detach is one ordinary `hdiutil detach` of the run's own mount,
authorized by a fresh rebind census taken immediately before it (the same backing,
mount, device and volume UUID), and accepted only when all of these hold: the child
succeeded and was reaped, the attachment census is readable and names no attachment
for the backing at all — whole-backing absence, not merely an absent mount path —
and the mount path is gone. A non-zero or unreaped child, an unreadable census, or a
missing mount with the backing still attached is a retained, unqualified outcome
with no retry and no force detach.

R11's intermediate detach and the final one are distinct: each re-derives its own
authority, and neither borrows the other's census. R11's remount must verify the same
backing, the same volume UUID and the same recorded relative path before its replay
binds them, and R12 begins only after that remount and its digest-bound registration
succeed. The final closure is the script's own; the guard's cleanup is authorized
separately by the guard's own custody scan, so a counted pass correlates all twelve
case rows, this whole-backing clearance and the supervisor's own receipt, and a
missing or unclear half is a missing proof.

It creates one owned 64 MiB sparse APFS image inside that run root and registers it
as the only volume. Its environment binds `HOME`, `BARGE_ACCOUNT_HOME` and
`BARGE_STATE_DIR` inside the owned scratch, and it checks that each path resolves
there. Each case's receipt records the reference control its previews chose, and a
control outside the owned scratch faults the case. Each of its twelve cases declares
its own area, so one case's roots never reach another case's plan:

| Case | Claim | Counterfactual of |
| --- | --- | --- |
| R1 | one aged, unreferenced root is released from its presented bytes; the replay releases nothing, and the settled intent keeps the identity | the positive case |
| R2 | a missing identity record keeps the root | R1, identity |
| R3 | a live hold keeps the root through custody | R1, custody |
| R4 | a young root is kept; a named root is released with its waiver stated | R1, age |
| R5 | a pending Oxide reference keeps the root | R1, references |
| R6 | a settled row releases only once its consumer path is absent (release by reference) | the release-by-reference policy |
| R7 | an incomplete Oxide set refuses the batch (`unknown_state`) and changes nothing | R1, complete reference set |
| R8 | a marker whose kind is unknown keeps the root | R1, marker kind |
| R9 | a changed digest refuses and changes nothing; the presented bytes then release | the digest |
| R10 | a consumer path that reappears between plan and run keeps the root | the release-by-reference case |
| R11 | a pending intent is written before the rename; after an ordinary detach and reattach, a later run settles it by UUID and the root releases after | the intent and remount |
| R12 | a run releases only the rows its presented plan carries | the plan's scope |

R11 retains the original identity record, marker, inode and fixture content through
replay. It binds the recorded relative path to the reattached mount only after the
UUID is verified, requires a cleared journal and an eligible plan for that identity,
and checks the exact root's release. The mount names and device observations are
separate facts. After R11 completes, R12 uses a fresh private state directory and a
digest-bound `volume register` plan for the same verified image; the old registration
is never rewritten. A failed R11 skips R12. Unknown command, device or custody
outcomes stop collection and retain the image without a repair attempt; receipts
identify skipped and unexecuted cases. The effect-free self-check exercises the
rebind, rollback and dispatch gates; it supplies no native credit.

The receipt, `release-proof-receipt.json` under `OXIDE_PROOF_EVIDENCE`, records the
head, the binary's and the supervisor's digests, the verified scratch route, the
staged build's manifest and digest, the volume UUID and its device before and after
the remount, the image-custody indication, the closure (the ordinary detach's exit and
reap, the whole-backing census, the mount path and the purpose of that detach), the
wall time, each case's status and evidence, the first fault, and the real-state
premise. `qualified` is `true` only for a counted pass with every case passed, a
positive whole-backing clearance and an unchanged real state. The diagnostic is
always `qualified: false`.

A failed oracle names the one predicate that failed and the decisive values the
operation already returned — never a conjunction collapsed into one message. The
predicates keep their original order and short-circuit, so a failure record never runs
an observation the assertion would not have made: a filesystem fact the branch did not
reach is reported as `unobserved`, and the release-run record carries
`released_count`, `kept_count`, `stopped`, `replay_pending`, the expected root id, a
bounded prefix of the returned root ids (at most 32, inside a 400-character budget, with
the number omitted stated) and one entry per row (`root_id`, `outcome`, `reason`) until
the reporting allowance is reached, with the number omitted stated. The two "root kept"
and "root released" conjunctions are separate helpers with fixed polarities, so a release
check cannot inherit keep semantics through a flag. A plan row that does not meet its
expectation likewise records its deciding fields first — `eligible`, the first blocker
and its reason, and each gate's own verdict (`identity`, `custody`, `last_activity_ms`,
`references_complete`, `references_current`, `absence_verdict`) — before the bounded raw
row. A plan row is a
sorted-key document whose `absence` member alone can exceed a short cap, so a truncated
row would not name the gate; the deciding fields are what make a failed case judgeable
from the receipt after the run root is gone. This changes no eligibility decision.

The receipt also carries `qualification_inputs`: the case rows and the image
clearance are its own, and the qualified supervisor's receipt is named as a required
separate input, because the supervisor writes it after the child exits. Correlating
both, with the guard's own custody and detach record, is what turns a counted run into
a final qualification; half of it is not evidence.

The receipt's `limitation` field states what the proof does not establish. It runs a
proof-only build whose Oxide directories come from one synthetic home. It tests the
release logic over synthetic `AccountFacts`. It does not establish that the
production account lookup is equivalent, and the production build is not run. The
case table's regressions in the ordinary suite (`release.rs`, `cli_contract.rs`) carry
the claims. The synthetic incomplete-registry case (R7, and the original incomplete
reference) is carried by `an_incomplete_or_divergent_reference_source_keeps_the_whole_batch`,
which uses a synthetic `FakeAccount`.

What is not yet established: nothing in this repository has run the body against the
proof build. One activated diagnostic at `1e7267c` refused before staging and before
any fixture effect, on a driver premise that wrongly required the guard's evidence
directory to be inside its scratch directory; the guard itself closed that occurrence
cleanly with no image or resource retained beyond its own evidence. The contract above
and its pure controls now encode the guard's real sibling layout. Its self-check (`release:native:test`), its feature declarations and
its pure counterfactuals (`release:native:test`, `rust:test:proof-account`) are
checked. The self-check also drives the readiness correction's own decisions on
canned observations and seams: a wrong scratch route or volume UUID, a carried route
document that names another scratch, a stage parent outside this occurrence (or the
shared scratch, a home folder or a drive root), an inherited Rust flag or unpinned
toolchain, a custody backing outside the owned root, a failed or partial attach, a
missing mount whose backing is still attached, a correct ordinary detach with
whole-backing absence, and an unreadable census all decide the way this contract
says. It also drives the guarded order — refuse, stage, verify, prove — and proves a
staging or stale-build refusal reaches no later effect. The diagnostic pass, the
counted author pass and a distinct reviewer pass each need owner activation at an
exact head. Until those receipts exist, a missing receipt is a missing proof.

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
| Oxide census | strict bounded reading of `oxide.outputs.registry/3`, `/2` and `/1` | a version-1 row, an incomplete registry, a duplicated UUID and a row whose volume is away each report the honest verdict; nothing is written and no row becomes authoritative |
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

The checklist for the one bounded native. The proof's own plan carries the matching
`premise_dispositions` (its nine named dispositions), and its receipt records the
isolation, route, build, custody and closure facts it actually observed.

1. A private `HOME`, `CARGO_HOME` and state directory, so no real account state
   is read or written. The staged proof-feature compile is the exception the brief
   names: it is the ordinary managed Oxide compilation and keeps the real
   account's `HOME`, `CARGO_HOME` and rustup home. It writes nothing into the
   fixture, and no fixture child inherits those paths.
2. The account home used for state resolution is the fixture's, and the real
   account state is untouched — asserted by a before/after digest of the real
   state directory, and by `barge describe` naming the fixture state root.
3. Environment isolation: `BARGE_STATE_DIR`, `TMPDIR`/`TMP`/`TEMP` and every
   inherited scratch point inside the guard's owned scratch; no inherited real
   path, and no inherited `BARGE_*` hook the proof did not set.
4. Explicit adoption only: every fixture root is created or adopted by an
   explicit command with a recorded plan, never by a default.
5. Owned image with ownership enabled (`-owners on`), and ordinary detach only —
   the proof never forces a detach, and an uncertain attachment or detach keeps
   the mount, the backing and the evidence.
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
  F5 lands the census in that form and
  [the F5 note](#f5-output-registry-census) records the concrete readings a slice
  had to fix inside it.
- **O4-007 — Who may prune an Oxide root, and when.** Proposed: nobody but
  Oxide until Oxide declares its areas and switches, so the gate is proven
  against Barge-owned roots first; the 91 legacy roots then need an operator list
  and an adoption decision that `ff13f2ef` already tracks. Nothing in this
  contract authorizes a single Oxide root to be deleted by Barge.

### F5 output-registry census

`barge inspect` reports one further read-only observation: Oxide's output
registry, `<internal pool>/outputs/registry.json`, at `oxide.outputs.registry/2`
and `/1`. The report is `barge.inspect/1`'s optional `oxide_census` field (an
optional field inside the major, so an older document still reads); it holds the
registry's own state, then one row per recorded reference and the drift summary.
Nothing in it becomes authoritative, no marker is read beyond one bounded 64 KiB
read per root, and no row is ever skipped. A row whose recorded volume is away
reports `unknown(volume_unverified)` instead of disappearing.

Choices F5 made where this contract left the mechanics to the slice:

- **Finding the registry.** The candidate internal pools are the ones the
  registration lookup already uses: every pool Oxide's own `pools.json` names,
  then `Library/Caches/oxide/build-pool` and `Library/Caches/oxide/pool`. The
  first candidate whose `outputs/registry.json` is present decides; one that is
  present but unreadable is `unknown_state` and is never looked past. An absent
  registry reports every path it looked at, so "no registry" never reads as "a
  registry with no rows".
- **The pool list must be known, not assumed.** `pools.json` is the census's
  source of sources, so the slice distinguishes its three outcomes. A **reported**
  record gives the pools to search, followed by the defaults. An **absent** record
  is a fact — Oxide recorded no pool, so only its defaults can be in use — and the
  defaults are then the complete list. An **unreadable, oversized or divergent**
  record is not a fact about which pools exist: the census reports
  `unknown_state` naming that record, looks at no registry at all, and so neither
  pronounces the registry absent because a pool Barge cannot see was skipped nor
  reports a default pool's registry as the whole truth. An absent census is only
  ever reached from a complete candidate list.
- **Strict and bounded.** The document is read with Oxide's own caps — 4 MiB and
  4096 rows — and `deny_unknown_fields` at every level it reads. Its
  `version`/`schema` pair must be `/2` with 2 or `/1` with 1; an unknown pair, a
  zero revision, an unusable `created_by`, disagreeing `complete`/`incomplete`,
  an over-cap row count or a divergent field set is `unknown_state` naming the
  file. Its `rows` member is required, because Oxide always writes it even when
  it holds none: a document without `rows` is divergent, never an empty registry,
  and its absence is not evidence that no root is recorded. `intents` and
  `cancelled_overflow` are the members Oxide omits when they are empty, so they
  are accepted as absent and stay presence-only: the census reads rows.
- **Anchoring a recorded root.** The volume is found by the row's recorded UUID
  through the same bounded mount census a root verification uses, so a mount
  that moved still resolves. The recorded path's `/Volumes/<mount>` prefix is
  replaced by the verified live mount and every component below it is walked
  exactly, through held descriptors with no-follow `fstatat`/`openat` pairs, the
  same walk [verifying a root](#verifying-a-root) takes. A recorded UUID that is
  not canonical, and a path the platform has no mount prefix for, are
  `unknown(volume_unverified)` and `unknown(path_changed)`: nothing is repaired
  and nothing is guessed. `path_changed` is the census's name for every way the
  recorded path fails to name the recorded directory — an unusable remainder, a
  missing intermediate component, a component replaced between its reading and
  its opening, or more than 64 components — because all of them mean the same
  thing and none of them is absence.
- **The verdict.** `verified`, `unknown(reason)` or `gone`. A device change is
  never a refusal: it is `device_equal: false` with `remount` carrying both
  numbers, and the verdict stays `verified`. A changed inode is
  `unknown(inode_changed)` with both inodes reported, and its marker is still read
  and compared: the inode decides the verdict first, but changed marker bytes are
  evidence an operator is owed, so the census reports and counts them too. `gone` is reachable only
  from a volume that still verifies and a path that is `ENOENT`, and only after
  the mount census, the volume and the parent descriptor are re-read unchanged.
  The recorded owner marker's own version rules come first, exactly as Oxide
  applies them: an `oxide.outputs.root/2` marker must carry its 128-bit
  lowercase-hex nonce, so one with the member absent or malformed is
  `unknown(marker_schema_divergent)` — even when the row records the identical
  absent or malformed value, because two equal nonces that identify nothing are
  not an identity — while an `oxide.outputs.root/1` marker legitimately has none.
  After that the marker is checked in the same order as Oxide checks it (kind,
  checkout, volume UUID, nonce, then generation), but its digest is reported as
  `marker_digest_equal` and never decides identity — it is a plan fact re-read at
  each step, exactly as [the identity rules](#the-facts-and-the-three-roles-a-fact-may-have)
  require. A row that records no device or inode is
  `unknown(identity_record_missing)` — in an Oxide row that is what is missing,
  and a `/1`-era row is the ordinary case — and a row that records no usable
  volume UUID is `unknown(volume_unverified)`. A value is never defaulted. A row
  missing a recorded identity fact is still walked and its marker still read, and
  each comparison is made whenever its own recorded fact is present: a recorded
  device is compared with the live one when the inode is missing, and the reverse,
  so one fact's absence never suppresses the other's diagnostic result. A
  comparison the row cannot support is absent, never `false`. Changed marker bytes
  are reported and counted like any other, and a recorded path that is absent
  stays `unknown(identity_record_missing)` rather than `gone`, because without a
  recorded inode the recorded directory cannot be tied to that path.
- **Per row, never per batch.** One undecidable row never stops the others: a
  volume that is away, a marker that is unexpected and a row without recorded
  identity facts each keep only their own row. `inode_equal` and `device_equal`
  are absent rather than `false` when the comparison could not be made, so an
  unavailable fact never reads as a disagreement.
- **The drift report.** `oxide_census.drift` counts what the carried finding
  needs as one number: `verified`, of which `device_moved` are roots whose live
  device differs from the recorded one — exactly the roots a recorded-device
  comparison refuses — `inode_changed`, `marker_changed`, `gone`, `unknown`, and
  `unknown_reasons` by code. `inode_changed` and `marker_changed` count the
  differences the census observed, whatever the row's identity verdict: an inode
  that differs is evidence even when a missing recorded fact already keeps the row,
  a marker whose bytes differ is evidence whatever those bytes turned out to be
  (readable divergent, malformed or unknown-version bytes are compared before they
  are parsed, so they count too), and an unknown verdict never hides an observation
  the census did make. Only a comparison the row could not support is `None` and
  never counted: no recorded inode or device, or a marker the census could not read
  at all — absent, unreadable, oversized or another user's. `device_moved` stays
  the verified-rows number it is defined to be. `examined` and `unexamined` bound
  the claim: the census shares one 10 s deadline with the registry read, and rows
  it did not reach are counted, never assumed.
- **Read-only, and no adoption.** The census takes no lock, creates nothing and
  writes nothing. `inspect` remains the only command that uses it, and no row
  becomes a Barge record; Oxide's roots stay Oxide's to prune (O4-007).
