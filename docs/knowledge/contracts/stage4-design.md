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
custody, whole-root deletion and the plan/intent mechanics this contract
extends. Where they differ, they win and this file is corrected.

No part of this contract authorizes a native run, a real drive, a mount or host
setting change, a data move, a deletion, or any change to Oxide. It is design
only.

## Scope and authority

### Frozen inputs

- Tom's scope ruling and the authority list in [vision](../vision.md).
- The invariants in [architecture](../architecture.md#invariants), including the
  one this contract implements: owned-root identity survives remounts, as
  volume UUID plus a stable per-root identity, never the raw device number
  alone.
- The [stage 1 design](stage1-design.md) as frozen at `8b36b06`, and the code
  merged on `main` at `0727376` (slices S1 and S2). S3's `area` module is
  specified by that design and is not in that commit, so this contract is
  written against S3's frozen interface and says so where it matters.
- Oxide's `oxide outputs prune` as the implementation whose behaviour this
  replaces, and Oxide's absence rule as the compatibility target, both read at
  the pinned commit in [sources](#sources-and-version-identity).
- Decisions: `88cc123e` delegates design freezes to the Oxide Supervisor;
  `ff13f2ef` holds the 91 legacy Oxide output roots until Oxide's own absence
  rule (its task `40010b40`, which stays in Oxide) lands and this identity work
  is adopted; Q-004 fixes reading Oxide's records
  in place; `765f84c6` part (b) moved owned-root identity out of Oxide into
  Barge so it is designed once, here.
- The stage 1 freeze's own deferrals: reserve and floor pruning, and the
  retirement of any foreign format, each need their own slice.

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
proof for an absent consumer path; the single gate that authorizes pruning one
whole owned root; the plan and intent additions that carry identity and absence
evidence; the read-only diagnostic that makes the carried finding decidable; the
Oxide compatibility reading; the implementation slices; the tests; one bounded
native design.

### Out of scope

Any live run, real drive or data change. Registering volumes, changing a
volume's mode, reserve or floor, or measuring space (stage 1 `O-008`, later).
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
| `inflatable-cookie/barge` `main` | `07273764ddabdc1627bd82885fc5d8b0dbb5f762` (2026-10-26T13:53:36Z) | the stage 1 contract at `8b36b06` (`8b36b06dbabab3b4d67a91c3bd0dae813adba1d0`), the merged S1 and S2 code (`crates/barge/src/os`, `state`, `registry`, `inspect`, `describe`, `contract`), AGENTS.md and `effigy.toml` |
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
  candidate rule, the operator list and the host automation that runs
  preview-then-real.

Barge's own `os` layer at `0727376` already reads `ATTR_VOL_UUID` through an
opened descriptor, refuses a non-APFS filesystem, refuses the boot volume, and
requires a managed volume to be a real directory directly under `/Volumes`
(`crates/barge/src/os/macos.rs`, `os/mod.rs::verify_volume`). It has no way to
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
`scratch_run`/`cache_root`/`area_root`, `origin` in `created`/`adopted`/
`identified`, `inode` and `device_at_record` non-zero, at most 4 KiB, a regular
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

`barge.root/1` markers stay readable for at least one implementation slice, with
one bounded relaxation: its absolute `resolved_path` is compared to the live
path after replacing only its mount prefix with the verified live mount of the
recorded UUID, and every component below that prefix must still match exactly.
A root at a different relative path, or on a different volume, is still unknown.
No stage-4 code writes a v1 marker. If S3 lands a v1 writer first, the relaxation
is the bridge, and it is [open design issue O4-003](#open-design-issues).

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
with `origin: adopted`.

A root whose creation crashed between steps 3 and 4 has a valid marker and no
identity record. It is `Unknown(identity_record_missing)`: never prunable,
reported by every plan, and never given an identity automatically — deletion
authority must never appear by observation. How, and whether, a human may give
it one is [open design issue O4-002](#open-design-issues).

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
`state::LockSet`'s checked rank. Stage 4 adds no rank; it adds four rules:

| Step | Locks held | Rule |
| --- | --- | --- |
| Mount enumeration, absence proof | none | proves facts about the world outside Barge's state; it takes no lock and is never cached across a run |
| Identity verification | volume shared, then root shared, per root | released before the next root; two roots are never held at once |
| Plan build (one area) | registry shared, volume shared, one root shared at a time | the registry lock is never held across a directory walk or a delete |
| Identity write (creation, adoption, an operator repair) | volume shared, root exclusive | it is a root-lock-protected write and takes no other rank |
| Effect (quarantine, delete, replay) | volume shared, root exclusive, then the intent write last | the intent stays the last rank, so a pending intent is always the last durable record |

Two concurrent prunes of the same area therefore take turns per root instead of
one waiting on the other's whole plan, and neither can deadlock: the only
exclusive lock a step takes is one root's, always after the volume's, always
before its intent.

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
   `O_DIRECTORY | O_NOFOLLOW`; every component above the leaf exists, is a real
   directory of this user, and is on the anchor device. A symlinked component, a
   component on another device, an unreadable directory (`EACCES`), a depth-bound
   or deadline hit is `Unknown`.
3. **Only the leaf may be ENOENT.** A missing leaf gives `Absent`. A missing
   non-leaf component gives `Unknown(ancestor_missing)`: a renamed or deleted
   parent means the path may have moved, and a moved consumer path is not a
   deleted one.
4. **A verified nearest existing ancestor.** The leaf's parent — which condition
   2 has already proved to exist — must be on a local, verified volume:
   [the ancestor volume proof](#the-ancestor-volume-proof).

Anything else is `Unknown`, and `Unknown` is never absence, never idle, never
empty.

### Walk rules

| Situation | Verdict |
| --- | --- |
| The leaf is `ENOENT`; every ancestor existed and is a real directory | `Absent` |
| A non-leaf component is `ENOENT` | `Unknown(ancestor_missing)` |
| A component is a symlink | `Unknown(symlink_component)` |
| A component is a regular file or other non-directory | `Unknown(not_a_directory)` |
| A component is on another device than the anchor | `Unknown(cross_device)` |
| A component is owned by another user, or `EACCES` | `Unknown(not_readable)` |
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
| `--operator-list` legacy exception: nearest existing ancestor on the boot Data volume, any missing ancestor tolerated | ancestor on `boot_data`; **a missing non-leaf ancestor is still `Unknown`** | Barge is stricter here on purpose: the operator list is a one-off waiver with a human in the loop, and a general primitive must not inherit it |
| only the leaf may be `ENOENT` | the same | identical |
| `Absent` requires the root's own identity, custody and no current selection | the same, as the [gate](#releasability-the-one-gate) | identical |
| `diskutil`/`statfs` text | never identity; kernel facts through descriptors | stage 1's rule |

Rules that keep the handover safe:

- While Oxide prunes, **Barge deletes no Oxide root.** The primitive is
  implemented and proven against Barge-owned roots first; Oxide's adoption is
  its own slice with its own brief (stage 1 S8, `T6` below).
- Barge never writes an Oxide marker, receipt, registry row or tombstone. It
  reads `oxide.outputs.root/2` markers, `oxide.outputs.registry/2` rows and the
  receipts strictly, bounded, and reports them.
- Where Oxide's own rule is stricter than Barge's (the operator list, receipt
  completeness), Oxide's rule wins while Oxide owns the prune.
- Where Barge's rule is stricter (missing ancestor), Barge keeps more, which is
  the safe direction for a handover in progress.
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
one thing in this contract that the operator may want to see decided before T2.

## Releasability: the one gate

A whole owned root is deleted only when all six conditions hold, each re-proved
at the effect step, each with `Unknown` meaning keep:

| # | Condition | Owned by | `Unknown` means |
| --- | --- | --- | --- |
| 1 | The volume is verified: APFS, not boot, not duplicated, mounted once | stage 1, [finding the volume](#finding-the-volume-after-a-remount) | keep |
| 2 | The root's [identity verdict](#verifying-a-root) is `Verified` | this contract | keep |
| 3 | Custody is `Safe` (holds released or positively stale; same-uid scan bounded and empty) | stage 1, "Custody protocol" | keep |
| 4 | The area's declared `PrunePolicy` allows it and the volume's `durability` is `expendable`; age comes from [last activity](#last-activity) | stage 1, "Area declaration API" | keep |
| 5 | Every reference naming the root is not current, from a complete reference set | this contract, [the consumer reference](#the-consumer-reference-what-makes-a-root-releasable) | keep the batch |
| 6 | Every reference whose own rule requires absence has an `Absent` proof, re-established now | this contract | keep |

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

`barge.intent/1` gains two optional objects, `identity` and `absence`, holding
what was proved at plan time and what was re-proved at effect time, so a replay
after a reboot has the evidence it must re-establish rather than a permission it
must trust.

## Formats and versions

| Document | Version | Where | Stage 4 change |
| --- | --- | --- | --- |
| `barge.root` marker | `/2` written, `/1` read | `.barge-root.json` in each owned root | path pair becomes mount-relative plus a mount hint; `generation` defined |
| `barge.root.identity` | `/1` | `state/roots/<root-id>/identity.json` | new |
| `barge.absence` | `/1` | in a plan row and an intent | new |
| `barge.prune.plan` | `/1` | `barge prune --plan` | new: one area's plan with identity, custody, idle, reference and absence evidence |
| `barge.maintain.plan` | `/1`, fields completed | `barge maintain --plan` | adds the `prune` array and per-area plan digests |
| `barge.intent` | `/1`, optional fields added | `state/intents/<intent-id>.json` | adds `identity` and `absence` |
| `barge.describe` | `/1`, optional fields added | `barge describe` | adds `identity.capability` (what may be compared across a remount) and `volumes[].mounts` (how many live mounts carry each registered UUID) |
| `barge.inspect` | `/1`, optional fields added | `barge inspect` | adds the Oxide registry observation and the per-row identity census |

The versioning rules are stage 1's: adding an optional field stays inside a
major; removing one, changing a type or a meaning is a new major. `barge.prune.plan/1`
and the completed `barge.maintain.plan/1` are defined here before any landed
code writes either — S5 and S7 are not in `0727376` — so their field sets are
completed rather than extended, and the reader's "exactly this field set" rule
holds from the first write.

## Reserve and floor are not in this contract

Stage 1 defers reserve and floor pruning to "after a qualified space
measurement" (`O-008`). This contract owns the half that slice needs — identity,
absence, the reference check and the gate — and not the trigger: the space
measurement, the reserve/floor arithmetic, the "low space prunes the least
recently used idle root first" ordering and the admission-time re-check. Those
are a later brief that consumes `barge.prune.plan/1`; nothing here may be read
as authorizing a reserve or floor value.

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
| A consumer path's ancestor is a symlink, a foreign mount, a network or autofs mount, or a not-APFS volume | walk or volume proof | `Unknown`; keep |
| A consumer path's ancestor volume is a removable volume that is away | no candidate mount | `Unknown(volume_unverified)`; keep |
| A reference set is incomplete | an unreadable, oversized or schema-divergent receipt or registry | keep the whole batch that names those roots |
| A hold is live, stale-but-undecidable, or foreign-uid | stage 1 custody rules | `Unknown`; keep |
| A hold's boot identity differs from this boot | all three of pid, start identity and boot identity disagree | the hold is positively stale; custody may be `Safe` |
| Clock skew, NTP correction or a restored snapshot puts activity in the future | `last_activity_ms` ahead of now | not eligible; `Unknown`; keep |
| An identity record is missing | marker valid, no `state/roots/<root-id>/identity.json` | `Unknown(identity_record_missing)`; keep forever; report on every plan |
| An identity record is unreadable, oversized or schema-divergent | strict bounded read | `Unknown(identity_record_invalid)`; keep |
| The state directory is on an unavailable volume | state resolution fails | refuse every mutating command; `describe` still reports (stage 1 `O-002`) |
| Two prunes run at once | one root lock at a time, intent last | the second sees a pending intent for that root and skips it; a stale reader's identity check fails and it changes nothing |
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
| Treat a missing ancestor as absence (the reaper's implicit behaviour, and Oxide's `--operator-list` waiver) | A renamed or deleted parent means the consumer path may have *moved*, and its outputs are still live there. A general primitive must not inherit a one-off human waiver. |
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
| T1 | `barge.root/2` marker, `barge.root.identity/1`, the identity write at creation and adoption, [verification](#verifying-a-root) with its verdicts and reason codes, `identity.capability` and the mount census in `describe` | S3 (area, markers, holds) |
| T2 | `os::VolumeLocator` (`getfsstat` + `ATTR_VOL_UUID`), [finding the volume](#finding-the-volume-after-a-remount) with its duplicate refusal, `barge.root/1` prefix-rule reading, the `barge.absence/1` primitive and its ancestor volume proof | S1 (`os`), and T1 only for the shared walk helper |
| T3 | The [gate](#releasability-the-one-gate): the reference readers (Barge's own records, and Oxide's receipts, registry rows and markers read in place), `last_activity_ms`, the `barge prune --plan` verb, and `barge.prune.plan/1` with its digest and evidence rows | T1, T2, S5 (custody and delete) |
| T4 | `barge.intent/1` with identity and absence evidence, replay under a changed device, and the `barge.root/1`-to-`/2` settlement for a pending intent written by an older reader | T3, S5 |
| T5 | `barge inspect`'s Oxide registry census and the drift report; the read-only operation that answers the carried finding without adopting anything | T1, T2, S2 (`inspect`) |
| T6 | Reserve and floor: the qualified space measurement, the trigger, ordering and admission re-check, `barge maintain --plan` real runs | T3, T4, S7, and its own brief |
| T7 | Oxide declares its output areas and switches its prune to Barge's gate; the 91 legacy roots are released with an operator list | T1–T6, Oxide's own project, `ff13f2ef`, operator approval |

`T1` and `T2` can run in parallel; `T2` needs nothing from `T1` except a shared
descriptor-walk helper, which lives in the `os`-adjacent containment code either
way. `T5` is read-only and could land before `T3`, which is useful because it
produces the evidence for the finding while the gate is still being built.
Nothing depends on a real drive; `T6` and `T7` each need their own operator gate
because they change what happens to data on a real volume.

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
| Identity write | the write order; the identity record written under the root lock | a crash between marker and identity record leaves `identity_record_missing`, never prunable, never adopted automatically; a second identity write is impossible by type |
| Absence walk | leaf `ENOENT` gives `Absent`; every other outcome is enumerated | a missing non-leaf ancestor, a symlinked component, a cross-device component, `EACCES`, a 65-component path and a depth-bound hit each give `Unknown` with its code; a relative path and a `..` component are refused before the walk |
| Ancestor volume | `boot_data` and `registered(uuid)` proofs | an ancestor on a not-APFS volume, on a foreign mount, behind a stale mount point, on an unmounted removable volume, or on a volume whose UUID differs gives `Unknown` |
| The gate | each of the six conditions alone blocks a delete | six counterfactuals, one per condition, each proving the root survives with its reason; plus one proving all six together authorize it |
| References | receipts, registry flags and Barge's own records decide "current" | an incomplete reference set keeps the whole batch; a selected, reserved, pruning or cancelling row keeps its root |
| Plan and intent | canonical plan, digest, evidence on every row | a changed digest, a changed marker digest, a changed identity and a reappeared consumer path each stop the affected row only; a replay after a device change settles from the recorded identity |
| Oxide census | strict bounded reading of `oxide.outputs.registry/2` and `/1` | a version-1 row, an incomplete registry, a duplicated UUID and a row whose volume is away each report the honest verdict; nothing is written and no row becomes authoritative |
| Caches and scratch | the identity rule reaches every root kind | a `scratch_run` root concluded under generation 2 verifies; an adopted `held-root` with `origin: adopted` verifies; a `cache_root` with a changed inode is kept |

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
     non-leaf ancestor and a symlinked ancestor are `Unknown`;
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
- **O4-002 — Repairing a root that has no identity record.** A crash between the
  marker and the identity write, or a root created by an S3 build that predates
  T1, leaves a valid owned root with no identity. Proposed: never automatic; one
  explicit operator command that takes the root lock, verifies the marker and
  the volume, requires custody `Safe`, writes a record with
  `origin: identified`, and reports exactly what it decided. The alternative is
  that such a root can only ever be kept and deleted by hand. The operator's
  choice is needed before T1 lands, because it decides whether a repair verb
  exists at all.
- **O4-003 — The `barge.root/1` prefix relaxation.** Stage 1's v1 marker records
  an absolute `resolved_path`. Stage 4 reads it with the mount prefix replaced
  by a verified mount of the recorded UUID and every component below it compared
  exactly, and writes v2. If S3 lands a v1 writer before T1, the relaxation is
  live from the first day; otherwise v1 has no writer and the rule is only a
  reader test. The architect (design freeze) should confirm the relaxation, and
  either way the alternative — refusing every v1 marker after a mount-point
  change — is rejected because it would make S3's roots unprunable forever.
- **O4-004 — The mount enumeration primitive.** Proposed: `getfsstat(2)` for the
  candidate list plus `ATTR_VOL_UUID` read through each candidate's descriptor,
  with a 256-entry bound and at most 64 candidates for one UUID; `diskutil` is
  never parsed. The implementation brief must record the concrete refusal path
  for a truncated or partial mount table (proposed: `unknown_state`, never "no
  mount").
- **O4-005 — Last activity for an area root.** An `area_root` has no conclusion,
  so stage 1's idleness has no clock for it. Proposed: the newest modification
  time in a bounded scan, with the marker directory's mtime as the floor, and a
  future time, an unreadable subtree or a bound hit as `Unknown`. A consumer that
  wants a different clock declares one in its area. This decides the age of
  Oxide's roots when it adopts, so it should be confirmed with the same review
  that confirms O4-006.
- **O4-006 — Reading Oxide's prune registry in place.** Stage 1 reads Oxide's
  registry "presence and version only". The census in
  [reading the census](#reading-the-census-without-adopting-anything) reads its
  rows to answer the carried finding. Proposed: read-only, strict, bounded, never
  authoritative, used by `barge inspect` alone, and retired when Oxide adopts its
  area. This widens a read boundary that stage 1 set deliberately, so the
  architect should confirm it before T5; the alternative is to answer the finding
  only after Oxide's adoption, which delays the 91 held roots further.
- **O4-007 — Who may prune an Oxide root, and when.** Proposed: nobody but
  Oxide until Oxide declares its areas and switches, so the gate is proven
  against Barge-owned roots first; the 91 legacy roots then need an operator list
  and an adoption decision that `ff13f2ef` already tracks. Nothing in this
  contract authorizes a single Oxide root to be deleted by Barge.
