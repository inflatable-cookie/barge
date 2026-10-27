# Stage 3 design: registration, space policy, reserve pruning and the floor

This contract is the reviewed design for Barge's third stage: registering a
volume's space policy, measuring free space, pruning idle owned roots when the
registered reserve is breached, and refusing work only at the hard floor. It
also owns the meaning of an area's `reserve` participation, the concurrency
rules that let pruning run while consumers work, and the formats, locks, bounds,
failure matrix, test plan, bounded native design and implementation slices
those need.

The measurement and registration slices are implemented. This contract alone
does not authorize a native run, a real drive registration, a mount or host
setting, a data move, or an Oxide change. [Architecture](../architecture.md) owns the concepts and the
boundary; the [stage 1 design](stage1-design.md) owns the crate and CLI shape
this stage extends. Where this file and architecture differ, architecture wins
and this file is corrected.

<details>
<summary>Which sections answer the design review's pass gate</summary>

- `operator_rulings_carried`: [Carried operator rulings](#carried-operator-rulings),
  [No low-space internal fallback](#no-low-space-internal-fallback),
  [The area soft-cap boundary](#the-area-soft-cap-boundary).
- `prune_trigger_and_floor_semantics`:
  [The reserve is a trigger and the floor is the stop](#the-reserve-is-a-trigger-and-the-floor-is-the-stop),
  [Registration: reserve, floor and participation](#registration-reserve-floor-and-participation),
  [Measurement](#measurement).
- `concurrent_prune_custody_and_lock_order`:
  [Idleness, activity and least-recently-used order](#idleness-activity-and-least-recently-used-order),
  [Pruning under the reserve](#pruning-under-the-reserve),
  [Concurrent pruning while consumers work](#concurrent-pruning-while-consumers-work),
  [Lock order with the stage 1 locks](#lock-order-with-the-stage-1-locks).
- `staged_validation_and_handoff`:
  [Unit and counterfactual tests](#unit-and-counterfactual-tests),
  [One bounded native design](#one-bounded-native-design),
  [Validation staging and the handoff to stage 4](#validation-staging-and-the-handoff-to-stage-4),
  [Implementation slices and dependencies](#implementation-slices-and-dependencies).
</details>

## Scope and authority

Frozen inputs, recorded before this design and unchanged by it:

- **Tom's rulings**, as carried into Barge by
  [architecture](../architecture.md#space-policy-carried-from-oxide-tom-2026-10-25-and-2026-10-26)
  and [vision](../vision.md#what-it-does): the reserve is a prune trigger, only a
  hard floor (default 10 GiB, a registration field) stops work, a floor above the
  reserve wins, an area's soft cap counts that area's own bytes only, low space
  never redirects work to the internal drive, and idleness comes only from
  Barge's own hold and release. Oxide answered the underlying decisions
  `c2cd939e` and `ad46853e`; Barge's stage 3 is where that policy is built once.
- **The stage 1 design**, frozen at `8b36b06`
  ([contract](stage1-design.md)): account state and its fixed lock order, the
  volume registry, area declarations, owned roots and markers, holds, writer
  custody, whole-root deletion with a quarantine and intent journal, the
  plan/preview discipline, bounds, and the JSON and error contract.
- **The merged code** at Barge `main` `07273764ddabdc1627bd82885fc5d8b0dbb5f762`:
  stage 1 slices S1 (`ad6373e`, the workspace, OS layer, volume verification and
  `barge describe --json`) and S2 (`efa4744`, state, locks, `barge.registry/1`,
  Oxide observations and `barge inspect`). No S3 (areas, roots, holds) commit is
  on `main` at that base, so the seam this design needs is taken from the frozen
  stage 1 design, and every stage 3 slice below depends on stage 1 S3 and S5
  landing first. Nothing here presumes otherwise.
- **Oxide's current machinery, read only**, at Oxide `main`
  `5ce34c5526dd4945ced71ca9a6eabadcdcad206e` (2026-10-26T14:17:11Z): the
  removable registration and its selection rule (`src/bin/oxide/removable.rs`),
  the volume binding, measurement and whole-volume flush (`src/volume.rs`), the
  explicit owned-output prune (`src/bin/oxide/prune.rs`), and its knowledge
  (`docs/knowledge/vision.md`, "Low space on the Builds drive";
  `docs/knowledge/contracts/local-lease.md`, the reserve/floor route). Oxide is a
  reference, never a dependency and never edited here.
- **Oxide's qualified shared proof supervisor**, read only at the same commit:
  `scripts/bounded-test.py` and `scripts/bounded_runner.py`, with the invocation
  Oxide already uses (`bounded-test.py --limit 300 --scratch-budget-mib 512`) and
  its `hdiutil create -size 128m -fs APFS -type SPARSE` /
  `hdiutil attach -nobrowse -owners on` fixture pattern.

In scope for this design: the volume record's space policy and its registration
verbs; the space measurement and its accuracy limits; the reserve trigger, the
prune target and the hard floor; idle detection and the least-recently-used
order across areas; the prune that runs at admission and while work runs, its
custody, flush and reclaim check; the concurrency and lock rules; the formats,
bounds, failure matrix, tests, one bounded native design and the slices.

Out of scope, each needing its own brief and gates: implementation of any of it;
Oxide declaring its pool and outputs as areas (stage 4, Oxide's own Queue
project); Effigy's scratch move; replacing the host job; pruning a cache
placement's whole root (deferred, see
[alternatives](#alternatives-considered-and-rejected)); any real-drive
qualification; Linux.

No material scope question is open. The decisions this design had to take where
the rulings stop are listed in [review questions](#review-questions) with their
alternatives, so a reviewer can rule on them rather than infer them.

## Carried operator rulings

Each ruling, and the mechanism in this design that carries it. A later section
that contradicts a row is a defect against this table.

| Ruling | Carried by |
| --- | --- |
| The registered reserve is a **prune trigger**, not a stop. | [The reserve is a trigger and the floor is the stop](#the-reserve-is-a-trigger-and-the-floor-is-the-stop): at or below the reserve Barge prunes, then admits if nothing idle is left. |
| **Only the hard floor stops work**, default 10 GiB, a registration field; a floor above the reserve wins. | The same section's decision table; the stop line is the floor alone, and the prune `target` is above `max(reserve, floor)`. |
| Below the floor with nothing idle left, a **new command refuses** and a **running one stops**, with a message naming free space, floor, reserve and whether non-Barge data is the cause. | `barge.space.guard/1`'s completed stop and `barge.error/1`'s `space_below_floor` no-effect refusal (both carry the same message and detail), and [the exit-code rules](#the-stop-the-refusal-and-the-exit-code); [rechecking while work runs](#rechecking-while-work-runs). |
| Low free space on a **present** volume never silently redirects work to the internal SSD. The drive-absent fallback is a separate, declared per-area policy. | [No low-space internal fallback](#no-low-space-internal-fallback), including the type-level absence of a fallback decision. |
| An area's **soft cap counts that area's own bytes only**; caches, scratch and other data never count against it. | [The area soft-cap boundary](#the-area-soft-cap-boundary): Barge reports area bytes by root identity and never enforces a cap. |
| Prune the **least-recently-used idle owned roots across areas**, by each area's declared policy. | [Idleness, activity and least-recently-used order](#idleness-activity-and-least-recently-used-order) and [pruning under the reserve](#pruning-under-the-reserve). |
| **Idleness comes only from Barge's own hold and release**, never from a consumer's internals. **Unknown is never idle.** | The same idleness section: the only source is the recorded release, and every undecidable input makes a root not idle. |
| Concurrent pruning while consumers run must be **safe**: never a held root or one with an open writer; lock ordering with consumer holds; flush on removable volumes; positive custody. | [Concurrent pruning while consumers work](#concurrent-pruning-while-consumers-work), [lock order](#lock-order-with-the-stage-1-locks) and [pruning under the reserve](#pruning-under-the-reserve). |
| Barge stays **consumer-agnostic**: it learns nothing from a consumer's internals. | Everywhere: the only shared vocabulary is a root, its marker, a hold, an activity record and a policy. |
| **No daemon**; commands and scheduled jobs only. | [Rechecking while work runs](#rechecking-while-work-runs) and `maintain`. |

## Measurement

Space policy needs one fact read from the kernel, not inferred from Barge's own
directories: how many bytes a writer can still put on the volume.

### The measured fact

| Field | Source | Rule |
| --- | --- | --- |
| `free_bytes` | `fstatfs` on a descriptor opened on the verified volume's root: `f_bavail * f_bsize`, checked multiplication | The bytes an unprivileged writer can use, which is what a consumer about to write needs. `f_bfree` (including root-reserved blocks) is reported beside it as `free_blocks_total_bytes` and is never the decision input. |
| `total_bytes` | `f_blocks * f_bsize`, checked | Reported; never a decision input. |
| `source` | a fixed string, `"fstatfs.f_bavail*bsize"` | A document always names where its numbers came from. |
| `observed_ms` | the call's completion time | Every decision's measurement is distinguishable from a later one. |
| `purgeable` | `"unknown"` | APFS may include purgeable bytes in `f_bavail` that only appear under pressure. Barge cannot tell, so it says so. |

Rules:

- A measurement is always **descriptor-relative on a reverified volume**: the
  volume UUID, mount-root proof and APFS check of
  [`os::verify_volume`](stage1-design.md#read-only-inspection-versus-authoritative-writes)
  run first, and the `fstatfs` is taken on that same opened descriptor. A
  different volume at the mount point is `volume_mismatch`, never a measurement.
- An unreadable, absent or overflowing field makes `free_bytes` **unknown**. It
  is never zero and never "plenty": a decision that needs it refuses
  `unknown_state`. Zero is a real value with a real meaning and is not overloaded
  for "no answer".
- Nothing above `os` sums directory sizes to decide space. Summing Barge's own
  roots answers a different question (area usage, attribution) and cannot see
  another tool's data.
- A measurement taken after Barge deleted something is taken **after a flush**
  (below), because APFS frees space in a transaction and an immediate
  re-measurement can still report the pre-delete figure.

### Flush

Deletion is only half of reclaiming space; the filesystem must commit it, and a
measurement used for a decision must not be a stale one.

- The OS layer gains one flush surface, `Flush::flush_volume(&VerifiedVolume)`,
  implemented on macOS with `fsync_volume_np(fd, SYNC_VOLUME_FULLFSYNC |
  SYNC_VOLUME_WAIT)` on the volume's own descriptor, reverified before and after
  the call. That is the same whole-volume flush Oxide's `Binding::flush` uses
  today, reused rather than reinvented.
- A prune batch flushes the volume once after its deletions and before its
  post-batch measurement. A decision never admits on an unflushed measurement it
  caused itself; a measurement nothing changed needs no flush.
- A flush failure, timeout (30 s) or volume change during the flush makes the
  post-prune measurement unknown: the guard reports `unknown_state` at exit `3`,
  reports the
  roots it did delete, and leaves no pending intent unless the delete itself left
  one. It never treats "the flush failed" as "the space is there".
- There is no environment-variable fault-injection hook in product code (Oxide
  has a debug-only one; Barge does not inherit it): tests inject a failing
  `Flush` implementation, and the native design gets a real failure by detaching
  the volume mid-prune.

## Registration: reserve, floor and participation

Stage 1 designed the registry and its strict reader and left registration writes
to this stage. Registration is the only place a space policy is created or
changed, and it is an explicit operator declaration.

### The record

`barge.registry/1` gains one **optional** field per volume record:

```json
{
  "space": {
    "reserve_bytes": 21474836480,
    "floor_bytes": 10737418240,
    "declared_by": "operator",
    "declared_ms": 0
  }
}
```

| Field | Rule |
| --- | --- |
| `reserve_bytes` | The prune trigger. `u64`, above zero; a volume registered for expendable work must declare it. |
| `floor_bytes` | The stop line. `u64`, above zero; when the registration command omits it the default `10 GiB` (`10 737 418 240`) is written explicitly, never left implicit. |
| `declared_by` | Who declared it (the same free-text rule as `scratch_grants`). |
| `declared_ms` | When. |

Rules:

- **Absent is `undeclared`, never "unlimited".** A record without `space` (every
  imported `observed_oxide` record, and any record written before this stage) can
  be measured and reported, but a **space-gated** decision on it refuses
  `unknown_state` naming the missing policy. This mirrors `durability:
  undeclared`, which is likewise never inferred: a stop line is a safety
  threshold, and an absent threshold is not "no threshold".
- **The policy is optional in the format and required in effect, and it is a
  forward-only addition.** `barge.registry/1` permits an optional field inside a
  major, but the reader is strict (`deny_unknown_fields`), so a build that does
  not know `space` **refuses** the record as `unknown_state` rather than reading
  it and ignoring the policy. No build therefore ever reads a registry that
  carries a declared stop line while acting as if it had none, which is the
  direction a threshold must fail in. Writing `space` needs the build that owns
  this contract; the alternative (a new `barge.registry/2` major) buys nothing,
  because no consumer reads a registry and the old major would have to stay
  readable for at least one slice carrying no policy at all.
- **Oxide's `reserve_bytes` is never imported.** Oxide's field means the
  opposite thing — its selection refuses a volume at or below it and falls back
  to the internal pool. Importing it as Barge's reserve would turn a stop into a
  trigger silently. An imported record therefore has no `space` policy at all,
  and the observation keeps Oxide's number labelled as Oxide's.
- Registration **refuses** `floor_bytes == 0` (a volume with no stop line is not
  a policy Barge writes), `reserve_bytes == 0`, and either value above the
  volume's measured `total_bytes` at registration (a threshold the volume can
  never satisfy would refuse or prune forever). A floor **below** the default is
  permitted and reported as `floor_below_default` in `describe`: it is an
  operator's own margin, and Barge does not invent a minimum.
- A floor **above** the reserve is legal and ordinary; it simply means the stop
  line is the floor (see the decision table). No combination of the two is
  "normalised" at registration: what the operator declared is what is recorded.
- A policy change is one atomic registry write under the exclusive registry lock,
  increasing the record's `generation`. A decision that read an older generation
  re-reads and never applies a superseded policy to an effect (see
  [concurrency](#concurrent-pruning-while-consumers-work)).

### Registration verbs

```text
barge volume register --mount /Volumes/Builds --reserve 20GiB --floor 10GiB \
    --availability permanent --durability expendable --root-dir barge --json
barge volume update  <uuid> --reserve 24GiB --json
barge volume describe <uuid> --json
```

- Parsing accepts an explicit byte count with a binary suffix (`KiB`, `MiB`,
  `GiB`, `TiB`), not a bare float; a bare number is bytes. An inexact or
  out-of-range value is `usage` and nothing is written.
- `register` verifies the volume exactly as every other use does (UUID, mount
  root, APFS, not boot) and never mounts, mounts elsewhere, or creates a mount
  point. An absent volume is `volume_absent` and registers nothing: a policy for
  a volume nobody has seen is a guess.
- Both verbs plan first and write once. `--plan` prints the exact record bytes
  and their SHA-256; a real run re-verifies the volume and identity and then
  publishes. There is no "register everything found": discovery is not
  registration.

### What an area declares, and what it does not

The stage 1 area declaration already carries `reserve: ReserveParticipation`.
This contract is what gives that field its meaning, because a space policy is
what needs it:

| `reserve` | Meaning |
| --- | --- |
| `counts` | The area's owned roots are **eligible to yield under the reserve trigger**: idle roots in this area are prune candidates while the volume is at or below its reserve, whatever their age. |
| `excluded` | The area's roots are **never pruned for space**. They are still subject to their own `prune` policy in `maintain`, and they are reported in the stop's attribution as data the reserve does not cover. |

- `reserve` and `prune` are independent. `prune: none` with `reserve: counts`
  means "yield under pressure, but never on a schedule"; `prune:
  oldest_idle_after{7d}` with `reserve: excluded` means "age out normally, never
  evict for space". Conflating them would either block reserve recovery behind an
  age nobody can meet or silently make young data expirable.
- The declaration is where a **consumer consents** to pressure pruning. The
  absence of a declaration is not consent: an undeclared folder is not an area
  and its contents are never Barge's to prune.
- Barge's own `scratch/` participates by construction: its roots are Barge's own
  expendable data and whole-root deletion is safe for them. Pressure pruning of a
  scratch root uses the [stage 1 expiry
  predicate](stage1-design.md#expiry) with the age requirement relaxed to "idle
  now" — positively concluded, `nested_scope: exclusive`, `kept_reason:
  evidence`, no live hold. An unconcluded root (a crashed run, or evidence kept
  deliberately) is **never** pressure-pruned; it is reported as a floor cause.
- Barge's `caches/` never participates in stage 3 (see
  [alternatives](#alternatives-considered-and-rejected)): a cache root is a link
  target, and deleting it whole would leave the consumer's original path pointing
  at nothing. Reclaiming a cache is a placement-level operation for a later
  slice; it is reported as excluded data.

## The reserve is a trigger and the floor is the stop

Definitions, exactly as the code must name them:

- `free` (`F`): the measured `free_bytes`, or unknown.
- `reserve` (`R`): the volume's `reserve_bytes`.
- `floor` (`L`): the volume's `floor_bytes`.
- `target`: the prune target, `F' > max(R, L)` after a flush.
- `trigger`: `F <= R`.
- `stop`: `F <= L`.

The decision table, applied at admission and again on every recheck:

| State | Rule |
| --- | --- |
| `F` unknown | Refuse `unknown_state`. Nothing is pruned, nothing is deleted, nothing is admitted on a guess. |
| No `space` policy on the record | Refuse `unknown_state` naming the missing policy (`space_undeclared` detail). A report still measures and prints the numbers. |
| Volume absent | Refuse `volume_absent` naming the drive; the area's declared **absent** policy decides what the consumer does next. The space policy never applies to an absent volume. |
| Volume `durability` is `kept` or `undeclared` | Refuse `refused_boundary` (kept) or `unknown_state` (undeclared). Expendable work is not admitted onto a volume whose data Barge may not expend. |
| `F > max(R, L)` | **Admit**, nothing pruned. |
| `L < F <= R` (so `R > L`) | **Prune** in least-recently-used order until `F' > max(R, L)` or nothing eligible is left. If a batch reaches `F' > max(R, L)`, **admit**. If nothing eligible remains and `F' > L`, **admit below the reserve** with a warning naming `F'`, `R` and `L`. |
| `F <= L` | **Prune** in the same order until `F' > max(R, L)` or nothing eligible is left; then **admit** if `F' > L`, or **stop** once nothing eligible remains and `F' <= L` — a completed stop when the pass deleted something, a no-effect refusal when it deleted nothing. A running command **stops** on the same test. |

Stated without the table, because this is the sentence a reviewer must be able to
check against the ruling:

- The reserve **never** refuses work. It triggers pruning. If pruning recovers
  nothing, work proceeds — with a warning naming the free, reserve and floor
  figures — as long as the floor is satisfied.
- The floor **always** stops work, and only after pruning has been tried. There
  is no "prune and then admit below the floor" case and no "force" path. A stop
  that follows deletions is a **completed command** whose report says what it
  did, not a refusal: see
  [the exit-code rules](#the-stop-the-refusal-and-the-exit-code).
- `max(R, L)` is why "a floor above the reserve wins": with `L > R`, being just
  above `R` is not enough to admit, because the volume is still at or below the
  floor. The floor is evaluated independently of the trigger, so a low reserve
  never lowers the stop line.
- Both are evaluated on the **same measurement**, taken after the last flush, and
  every decision document reports which measurement decided it.

### The stop, the refusal and the exit code

A low-space outcome has two shapes — a stop and a refusal — plus the unsettled
case, and which stop shape applies is decided by whether the pass changed
anything. Stage 1's exit codes already say it; this stage must not bend them:

| Outcome | Envelope | Exit | When |
| --- | --- | --- | --- |
| **Completed stop** | `barge.space.guard/1`, `data.decision: "stop_below_floor"` | `0` | the pass deleted at least one root and the floor is still breached. Stage 1's `0` is exactly "the command completed and its report says what it did". |
| **No-effect refusal** | `barge.error/1`, `kind: "space_below_floor"` | `2` | the pass deleted nothing at all — nothing was eligible, or every candidate failed its re-verification — and the floor is breached. Nothing changed, so "refused before any effect" is literally true. |
| **Unsettled state** | `barge.error/1`, `kind: "unknown_state"` | `3` | the pass left a pending intent, or the measurement, flush, policy or registry became unknown. `detail` names the pending intent id or the unknown fact, and `detail.pruned_root_ids` reports anything already deleted. |

Rules:

- The two shapes are **different documents**, not one document with two
  meanings: effects are always a `barge.space.guard/1` success, and refusals are
  always stage 1's `barge.error/1`. There is no `ok: false`
  `barge.space.guard/1` document, and no `barge.error/1` document carries a
  `decision` or a `pruned` row set.
- `space_below_floor` is the one new value in `barge.error/1`'s closed `kind`
  set, and it is used **only** for the no-effect refusal. A guard that pruned and
  then stopped must not claim "refused before any effect" while its own report
  lists the roots it deleted.
- `pending_intent` is not a kind: stage 1's closed set has no such value. An
  unsettled delete is `unknown_state` at exit `3`, which is what stage 1 already
  means by "a pending intent or unknown state that needs a human".
- The stop signal a consumer reads is `decision: "stop_below_floor"` in a
  `barge.space.guard/1` success document, or `kind: "space_below_floor"` or
  `kind: "unknown_state"` in a `barge.error/1` document. A consumer must never
  infer a stop from the exit code alone, because `0` also carries a completed
  stop.
- The message and the attribution detail are identical in both shapes, so a
  consumer's message never depends on which one it received.

The message and the `detail` object both carry the four facts the ruling
requires:

```text
barge: /Volumes/Builds (A6AC3ED7…) is below its 10.0 GiB floor: 3.2 GiB free,
10.0 GiB floor, 20.0 GiB reserve. Nothing idle was left to prune: 41.7 GiB of it
is Barge-owned expendable data, all of it held or not positively idle; 12.9 GiB
is not Barge-owned (estimated). No work was sent to the internal drive.
```

| Detail field | Meaning |
| --- | --- |
| `free_bytes`, `floor_bytes`, `reserve_bytes` | The measured facts and the declared policy that decided. |
| `pruned_root_ids`, `reclaimed_bytes` | What the pass deleted before the stop or the refusal; empty and zero for a no-effect refusal. |
| `owned_prunable_bytes` | Idle, eligible, Barge-owned bytes still present: zero once the pass has taken everything eligible, which is what a stop means. |
| `held_or_unknown_bytes` | Barge-owned bytes in eligible areas that were **not** idle: held roots, unknown markers, unknown holds, unconcluded scratch. |
| `excluded_bytes` | Barge-owned bytes in areas whose `reserve` is `excluded`, and in unparticipating folders. |
| `other_estimate_bytes` | `total - free - owned - excluded`, labelled `estimate`: everything else on the volume, including APFS metadata, kept data and other tools. It is a lower bound on "not Barge's", never authority, and it exists so an operator can see that a full drive is somebody else's data. |
| `cause` | `held_owned_roots`, `excluded_or_non_barge`, `unknown`, or `none`. `unknown` when the estimate cannot be formed; it is never presented as `none`. |

The message names the internal drive only to say no work went there. Its wording
names what could not be freed, so the sentence differs by cause while the fields
above are identical in both shapes. It is a statement of fact from the stop or
refusal path, not a promised fallback.

## Idleness, activity and least-recently-used order

### The only source of idleness

A root is **idle** when, and only when:

1. its marker verifies under the [stage 1 marker
   rules](stage1-design.md#owned-roots-markers-and-holds) against the verified
   volume and its walked path;
2. its activity record verifies and records a **release** (below) — the last hold
   was released, and Barge recorded it;
3. every hold file for it is positively absent or positively stale under the
   [stage 1 hold rules](stage1-design.md#owned-roots-markers-and-holds); and
4. its area's declaration says it may yield here (`reserve: counts`, or Barge's
   own scratch under the relaxed expiry predicate).

Anything else is **not idle**. In particular: a marker that will not verify, a
missing or invalid activity record, a hold file that cannot be decided, a live
pid, a future-dated timestamp, an unconcluded scratch root, a root on another
device, a marker family Barge does not own, and a root whose area declaration is
absent or whose policy excludes it. Unknown is never idle, and there is no
"probably done" widening.

Filesystem mtimes, entry counts and consumer ledgers are **not** inputs. They are
consumer state or metadata, they move without a hold, and the ruling is explicit
that idleness comes from Barge's own primitive. A root whose tree was written
after its last recorded release is a consumer that broke the hold protocol; Barge
cannot see that, says so as a custody limit, and does not guess.

### Activity records

Holds are released by deleting the hold file, so the release itself must leave a
durable, positive fact. Barge writes one small record per root:

```json
{
  "schema": "barge.activity/1",
  "root_id": "<32 hex>",
  "volume_uuid": "<uuid>",
  "last_release_ms": 0,
  "first_created_ms": 0,
  "releases": 3,
  "generation": 7,
  "recorded_ms": 0
}
```

- Path: `state/activity/<root-id>.json`, under the account state directory of
  [stage 1](stage1-design.md#account-wide-state-directory). Written atomically
  (private temp, sync, rename, sync directory), at most 4 KiB, strict schema, no
  unknown fields.
- Written on root creation (`last_release_ms = 0`, `releases = 0`) and on every
  release (`last_release_ms = now`, `releases += 1`, `generation += 1`), while
  holding the root lock (rank 3).
- **Missing is not idle.** A root with no activity record — an older root from
  before this stage, or a state directory that was lost — is reported as
  `activity_unknown` and never pressure-pruned. Losing account state must not
  turn every root into a candidate. The repair is an explicit operator command,
  not an inference: `barge area idle-baseline <root-id> --json` re-verifies the
  marker, refuses if any hold exists or is undecidable, takes custody, and writes
  a fresh record asserting "idle as of now". Without it the root stays reported
  and kept, forever if need be.
- A release never writes into the root's own marker: the marker is written once
  and never rewritten (stage 1), and an activity record beside it in account
  state keeps that rule intact.

### The order

Least-recently-used, across all participating areas, in one deterministic total
order:

1. older `last_release_ms` first;
2. then older `first_created_ms`;
3. then `root_id` ascending (a stable, arbitrary tie-break).

The order is computed across areas, not per area: a single volume-wide list means
the oldest idle data yields first whichever area holds it, which is what "across
areas" requires. Each candidate is still gated by **its own** area's policy, so
an `excluded` area's roots never enter the list at all.

## Pruning under the reserve

### What a space prune is

A space prune is the stage 1 whole-root delete, driven by the reserve instead of
by age, in one bounded pass:

```text
barge space guard  --volume <uuid> --area oxide/pool --json
barge space recheck --guard <token> --json        (a running consumer's checkpoint)
barge space prune  --volume <uuid> --plan --json  (operator preview)
barge space prune  --volume <uuid> --plan <file> --plan-sha256 <hex> --json
barge space          --volume <uuid> --json        (a report; no effect)
```

A **guard call** is the admission step a consumer makes before it writes. It:

1. resolves the volume from the area's own declaration on the verified volume —
   never from a caller's claim — then reads the policy snapshot (registry,
   bounded read) and the volume's identity, and refuses without a measurement if
   the policy is missing, the durability forbids expendable work or the volume is
   absent;
2. measures (flush-free: nothing has changed yet). A measurement needs no lock:
   it is a read, and a concurrent prune can only free space;
3. if that measurement is at or below `max(R, L)` — the trigger, or already the
   stop — takes the volume lock **exclusively** and **holds it from this first
   authoritative measurement to the final decision**, re-measures (another
   process may have pruned in between) and plans. If the lock cannot be taken
   within the lock wait, the guard does not prune: it re-measures and applies the
   decision table to that measurement as if nothing could be pruned, reporting
   `decided_after_wait`;
4. deletes candidates in order, one root at a time, each under its root lock
   (volume → root, strictly ascending) with the stage 1 custody, intent and
   quarantine protocol, up to its bounds;
5. flushes the volume, re-measures, and repeats a batch if still at or below
   `max(R, L)` and eligible candidates remain, still holding the volume lock;
6. decides, releases the volume lock, and reports `barge.space.guard/1` — admit,
   admit below the reserve, or stop — with the measurement that decided, the
   roots it deleted, the bytes it reclaimed and the attribution.

`barge space prune --plan` is the operator's preview: the same computation with
no effect, printing the ordered candidates with their identity digests and the
plan's SHA-256. A real run takes exactly those bytes and re-verifies every
predicate before each effect, as stage 1's `maintain` does. A guard's own prune is
a **live** prune and cannot round-trip a preview — admission is one call — so it
re-verifies each candidate at its effect step instead, and reports the plan it
executed as evidence afterwards. That difference is deliberate and is recorded in
[alternatives](#alternatives-considered-and-rejected).

### Custody, and why a held root is never touched

A candidate is deleted only when the stage 1 `Safe` custody verdict holds for it,
which requires, in this stage's terms:

- the root lock is held exclusively for the whole plan → intent → rename →
  delete sequence, so **a consumer cannot take a hold in the middle**: `hold`
  takes the same root lock, and a hold attempt during a prune either waits and
  then fails with `locked`, or is recorded before the prune reads the holds;
- every hold record read under that lock is positively released or positively
  stale;
- the same-uid open-descriptor scan is complete, empty for this root, and within
  its bound, so a writer that never took a hold is still seen if it is ours to
  see;
- the root's device, inode and marker digest still equal the observation the
  candidate was built from;
- the area's declaration still says `counts`, and the policy generation still
  matches the snapshot;
- when a root also carries Barge's durable identity record (`barge.root.identity/1`,
  [stage 4](stage4-design.md#root-identity)), that record verifies and agrees
  with this observation; a disagreement keeps the root rather than being resolved
  in either direction; and
- there is no pending intent for the root.

A held root — one with a live hold, one whose holder is alive, one the scan
cannot complete, one with an unreadable hold file — is skipped and reported, never
"attempted and failed". Skipping is the safe outcome; it also means the stop's
attribution can say exactly why bytes were not freed.

The residual limit is stage 1's and is not papered over: a process of another
uid, or one that dropped its hold and kept writing, is outside what macOS lets an
unprivileged Barge prove. `describe.custody.capability` states it, the intent and
quarantine make a wrong delete recoverable up to the marker unlink, and no force
path exists.

### Reclaim, and when pruning stops

After each batch: flush, then measure. Then:

- if `F' > max(R, L)`, stop pruning and decide on that measurement;
- if nothing eligible is left, stop and decide: admit if `F' > L`, and once
  `F' <= L` report the stop in the shape
  [the exit-code rules](#the-stop-the-refusal-and-the-exit-code) require — a
  completed stop if this pass deleted anything, a no-effect refusal if it did
  not;
- if `reclaimed_bytes` for a batch is zero *and* the flush succeeded, stop
  pruning and report `space_not_reclaimed` (detail: the roots deleted, their
  recorded sizes, the two measurements). That report is a completed document,
  because something was deleted. Unlinked-but-open files of a foreign writer,
  purgeable space that will not materialise, or a filesystem that has not
  committed are all causes Barge cannot distinguish; it stops rather than
  deleting more data for no effect, and the outcome is the floor's, not a gamble;
- the batch, round and per-root budgets in
  [bounds](#bounded-resources) end the pass regardless. A pass that ends on a
  budget still decides from its last flushed measurement, reports
  `budget_exhausted` with what it did not reach, and takes the same stop or
  refusal shape as any other outcome.

### What a space prune never does

- It never deletes a root whose area is `excluded`, undeclared, or a foreign
  marker family.
- It never deletes a cache placement's root, a `kept` placement or its data, a
  root on another device, a stale mount-point directory, or anything outside the
  verified volume.
- It never deletes a root with an unconcluded scratch marker, or an
  `adopted_existing` root that its area's policy does not cover.
- It never widens a delete to "make space": the walk's unknown-entry refusal,
  Finder tolerance, `ENOTEMPTY` rule and marker-last rule are stage 1's exactly.
- It never touches the internal drive, a home folder, or a drive root as scratch:
  the only writes are the stage 1 intent and journal records in account state.

## Concurrent pruning while consumers work

Consumers run while pruning runs. Four mechanisms make that safe, and each has a
test.

### 1. A hold is taken before a write, and prune respects it

`hold(root)` takes the root lock, writes the hold record, releases the lock.
`release(hold)` takes the root lock, removes the hold record, writes the activity
record, releases the lock. A prune takes the root lock of each candidate and
holds it across that candidate's plan → intent → rename → delete sequence. There
are exactly two orderings, and both are safe:

1. **The hold is recorded before the prune takes that root's lock.** The prune
   reads the hold file under the lock, finds a live hold, skips the root and
   reports it as `held`. The consumer's hold succeeded and it writes normally; it
   waited on nothing.
2. **The prune already holds the root lock.** The consumer's `hold` waits for the
   lock up to its own deadline and then fails `locked`, so it never starts
   writing into a root that is about to be renamed or deleted. The prune completes
   its verified delete. This is a wait on a **lock**, not on a hold.

There is no third ordering, because both sides take the same root lock and the
prune re-reads the holds under it. The window in which a consumer could start
writing into a root a prune has chosen is closed by that lock, not by timing.

`hold` and `release` are tiny (one lock, one small atomic file, one unlink). They
never take the volume lock, so a long prune pass cannot starve them beyond the
single candidate whose root lock it currently holds.

### 2. Custody is evidence, not a lock, so it creates no order

Hold files are read as evidence under the root lock. A prune never blocks on a
hold — a hold is a file it reads, not a lock it takes — and a consumer never
waits on a prune except for the root lock described above. There is therefore no
lock-order edge between consumer holds and Barge's locks: holds are files, and
the only ordered locks are the stage 1 ranks below. A consumer that holds a root
and then tries a space decision would acquire rank 2 under a held rank 3 —
`LockSet`'s rank check refuses that as `usage`, which is the documented rule:
**take the decision first, hold second, and release before the next decision.**

### 3. One prune per volume, and other decisions keep measuring

A prune holds the volume lock exclusively from its first authoritative
measurement to its final decision; a read-only report holds it shared, and only
when the lock file already exists; a measurement by a guard that is only deciding
needs no lock; a hold/release takes no volume lock. The volume lock therefore
serializes **pruning**, not reporting or admission, and two prunes on one volume
cannot overlap at all.

A second guard that finds itself needing to prune waits up to the lock wait (5 s).
Because it cannot prune, it then re-measures and applies the decision table to
that fresh measurement as if nothing could be pruned: `admit` when
`F > max(R, L)`, `admit_below_reserve` when `L < F <= R`, and the no-effect
refusal when `F <= L`. It reports `decided_after_wait` naming the volume lock it
waited on. This is the common case — one guard pruned, the next one needs
nothing — and it never spins and never breaks a lock whose owner is unknown.
Because that guard deleted nothing, a below-floor outcome in that position is
always the no-effect refusal, never a completed stop.

Two guards therefore cannot both delete the same root, and neither can hold the
volume lock for longer than its own pass bounds allow. Two guards on *different*
volumes prune concurrently and independently — the volume lock is per volume, so
a full Builds drive cannot block a Vault report.

`locked` is not a guard outcome: an admission decision needs no lock, so refusing
`locked` would be a worse answer than the measurement the guard can already take.
`locked` remains the answer for the operations that must hold the lock in order to
act — an operator's `space prune --plan` real run and a registration write.

### 4. The policy is a versioned snapshot, and a change stops an effect

The decision reads the registry once, bounded, recording the record's
`generation`. Before each effect it re-reads the registry (bounded, lock-free —
stage 1's atomic publish means a reader sees old or new bytes, never a torn
document) and compares:

- generation unchanged: proceed;
- generation changed: re-evaluate the candidate against the new policy. A
  candidate that no longer qualifies (its area withdrew `counts`, or the reserve
  moved) is skipped and reported `policy_changed`; the pass continues from the
  fresh measurement, because the new policy's thresholds may be different;
- registry unreadable or invalid: stop, delete nothing further, and report
  `unknown_state` (exit `3`), naming the file, with anything already deleted in
  `detail`.

A running consumer's `recheck` reads the policy afresh every call, so a changed
reserve or floor applies to work already running at its next checkpoint. That is
the intended behaviour: the operator lowering the floor admits more work, and
raising it prunes harder.

### The stale-measurement rule

No decision uses a measurement older than the state it describes. Concretely: a
measurement taken before a prune is not used to admit after it (a fresh, flushed
measurement is), and a `recheck` never reuses the previous call's numbers. A
measurement is a fact with a time, and the document always prints that time.

One thing follows that the design states rather than hides: an admission is a
decision on a measurement, not a reservation of room. Another writer can consume
the free space a moment later, and Barge cannot see that without a daemon. The
floor is therefore enforced at admission **and at every recheck**; a consumer
that writes for a long time without a checkpoint is outside the contract. What
Barge does promise exactly is that no fact it uses is stale by its own action,
and that every decision names the measurement that produced it.

## Rechecking while work runs

No daemon, so "while work runs" is the consumer's own checkpoint calling Barge:

```text
barge space recheck --guard <token> --json     # or space::recheck(&guard)
```

- `SpaceGuard` is a value, not a lease: volume UUID, area identity, the policy
  generation it last saw, the caller identity for the report, and its own
  deadline. It holds no lock, reserves no bytes and expires into nothing. The
  CLI's `--guard <token>` is that value in text form
  (`<uuid>:<namespace>/<area>:<generation>:<observed_ms>`, bounded and parsed
  strictly); it is **not** authority — a forged or stale token gains nothing,
  because every call re-reads the policy, re-verifies the volume and
  re-measures. A token whose volume UUID differs from the volume the area
  resolves on is `volume_mismatch`.
- A `recheck` on a volume that has become absent returns `volume_absent`; a
  running consumer treats that as a stop, because its writes are failing anyway.
- Each call measures, prunes if needed, flushes, and reports the same
  `barge.space.guard/1` document as admission: `decision: continue` when work may
  proceed (`admit` or `admit_below_reserve`), and a stop when `F' <= L` after
  pruning with nothing eligible left. A stop takes the shape
  [the exit-code rules](#the-stop-the-refusal-and-the-exit-code) require: a
  completed stop (`ok: true`, `data.decision: "stop_below_floor"`, exit `0`) when
  the recheck's prune deleted something, a no-effect refusal in stage 1's
  `barge.error/1` envelope (`space_below_floor`, exit `2`) when it deleted nothing.
  It carries the same
  message and attribution detail as the admission stop.
- The contract on the consumer is one sentence: **a consumer that writes bulk
  data calls `recheck` at its own checkpoints and stops on `stop`.** The
  recommended cadence is at least once per minute of continuous writing and after
  each new root, and stage 4's Oxide adoption binds it to Oxide's existing
  watchdog points. Barge cannot enforce a cadence without a daemon and does not
  pretend to; a consumer that never calls it is outside the contract, and its
  writes are its own.
- Rechecking is cheap in the common case (one reverify and one `fstatfs`, no
  prune, no flush) and bounded in the uncommon one (a prune pass with the same
  bounds as admission, default budget 30 s, at most 120 s when the caller asks
  for it).
- A recheck never touches a root the caller holds: the caller's roots are held,
  held roots are not candidates, and the recheck's own scan sees them as `held`,
  not as unknown.

## No low-space internal fallback

This is the ruling most easily broken by accident, so it is carried four ways.

1. **The decision type has no fallback.** `SpaceDecision` is `admit`,
   `admit_below_reserve`, or a stop (`stop_below_floor`) — a completed stop or
   the no-effect refusal of
   [the exit-code rules](#the-stop-the-refusal-and-the-exit-code). There is no
   `fallback_local` variant and no `Option<PathBuf>` for one, so a caller cannot
   receive a low-space redirect even by mistake. Stage 1's `AbsentPolicy::
   FallbackLocal` exists on the *area declaration* and is reachable only from an
   **absent** volume path, which requires a verified absence (`volume_absent`), not
   a low measurement.
2. **A low-space stop never names an internal path**, in either shape. The message
   names the free, floor and reserve figures and states that no work went to the
   internal drive. A completed stop is a success document with `decision`
   `stop_below_floor`; a no-effect refusal is a `barge.error/1` document with
   `kind: space_below_floor` at exit `2`.
   Neither is allowed to carry a path the consumer could write to.
3. **A no-effect refusal changes nothing first** — the name is accurate: no
   directory, no marker, no state file, no link, no mount point. A pass that did
   delete something reports it as the completed stop instead, so the "changed
   nothing" claim is never made by a command that changed something. In both
   shapes the only writes are the stage 1 intents and journal records of
   deletions that actually completed.
4. **A fallback is visible and attributed.** `barge space` and
   `barge describe --json` list every fallback root in use with the reason it
   exists, and the only reason they can name is `volume_absent`. A fallback root
   present while its volume is present is reported as a contract violation
   (`fallback_while_present`), which is exactly what a low-space redirect would
   look like.

For Oxide, this design records a source tension rather than resolving it here:
Oxide's current `removable::verified` refuses the volume when `available <=
reserve_bytes` and its selection then falls back to the internal pool, and
Oxide's `local-lease.md` still records that below-reserve selection fallback as
current behaviour. Barge implements no such fallback; retiring Oxide's is part of
Oxide's stage 4 adoption, in Oxide's own Queue project, with its own gates. Until
then the two behaviours coexist and this contract does not claim Oxide's has
changed.

## The area soft-cap boundary

The ruling "an area's soft cap counts that area's own bytes only" is carried by
separating three things that are easy to conflate:

| Thing | Owner | Barge's role |
| --- | --- | --- |
| A **cap** (a consumer's admission budget, e.g. Oxide's pool cap) | the consumer | Barge **never** enforces one. `barge space` reports `area_bytes` so a consumer can feed its own cap. |
| The **reserve and floor** | the volume registration | Barge measures free space and applies the decision table. |
| **Pruning** | the area's declaration plus this policy | Barge deletes whole idle owned roots only. |

- `area_bytes` is summed over **that area's owned roots only**, by root identity:
  each root's whole-tree allocated bytes from its listing, never by folder,
  never by volume used bytes, never by walking a neighbour. Scratch, caches,
  kept data and another tool's data are not in the number, so they cannot push an
  area over its own cap.
- Barge's report shows `area_bytes` beside the volume's free and total, so a
  reader can see that the volume is full of something the area is not
  responsible for — which is the fact the floor message needs — without either
  number being used as the other.
- Barge never sums areas into a cap and never divides the volume between them.
  A consumer that wants to share a volume with another consumer does that
  arithmetic itself against facts Barge measures.

## Formats and versions

All documents follow stage 1's JSON and error contract
([versioned JSON](stage1-design.md#versioned-json-and-error-contract)): one
document on stdout, `barge.<command>/<major>`, exact field sets, capped sizes and
rows, `unknown` never read as absent or default.

| Format | Kind | Change in stage 3 |
| --- | --- | --- |
| `barge.registry/1` | authoritative, account state | **optional** `space` object on a volume record (above); absent means undeclared. A reader that predates the field is strict and refuses the record as `unknown_state`. |
| `barge.activity/1` | authoritative, account state | **new**: one per root, `state/activity/<root-id>.json`. |
| `barge.area/1` | authoritative, account state | unchanged shape; this contract defines what `reserve` means. |
| `barge.space/1` | report, read-only | **new**: per-volume measurement, policy, state, attribution and area bytes. |
| `barge.space.guard/1` | decision document | **new**: the admission and recheck result, with measurement, pruned roots, reclaimed bytes, cause, message and a `decision` of `admit`, `admit_below_reserve`, `continue` or `stop_below_floor`. A stop that deleted something is this success document; every refusal is `barge.error/1`. |
| `barge.space.prune/1` | plan and report | **new**: ordered candidates with identity digests, the plan's SHA-256, and the executed result. |
| `barge.error/1` | report | the only envelope for a refusal, including this command's: one new `kind`, `space_below_floor`, used only for a no-effect refusal; an unsettled delete is `unknown_state` at exit `3`. |
| `barge.describe/1` | report | optional additions: a `space` row set and the new format names in `formats`; absent in an older document is valid. |
| `barge.root/1`, `barge.hold/1`, `barge.intent/1` | authoritative | unchanged. This stage adds no field to a marker, a hold or an intent. |

### `barge.space/1`

```json
{
  "schema": "barge.space/1",
  "command": "space",
  "ok": true,
  "data": {
    "observed_ms": 0,
    "volumes": {
      "rows": [{
        "volume_uuid": "A6AC3ED7-33EA-46B4-8082-7254CF291ED2",
        "mount_point": "/Volumes/Builds",
        "measurement": {
          "free_bytes": 34359738368,
          "free_blocks_total_bytes": 36507222016,
          "total_bytes": 512110190592,
          "source": "fstatfs.f_bavail*bsize",
          "purgeable": "unknown",
          "observed_ms": 0
        },
        "policy": {"reserve_bytes": 21474836480, "floor_bytes": 10737418240,
                   "declared_by": "operator", "declared_ms": 0},
        "state": "below_reserve",
        "areas": {"rows": [{"namespace": "oxide", "area": "pool",
                            "reserve": "counts", "bytes": 118111600640,
                            "idle_roots": 3, "held_roots": 1,
                            "unknown_roots": 0}],
                   "count": 1, "total": 1, "truncated": false},
        "scratch": {"bytes": 10737418240, "idle_eligible_roots": 2,
                    "held_roots": 1, "unconcluded_roots": 1},
        "caches": {"bytes": 21474836480, "reserve": "excluded"},
        "attribution": {"owned_bytes": 139586437120, "excluded_bytes": 21474836480,
                        "held_or_unknown_bytes": 0, "other_estimate_bytes": 0,
                        "cause": "none"}
      }],
      "count": 1,
      "total": 1,
      "truncated": false
    }
  }
}
```

For T1, `areas`, `scratch`, `caches` and `attribution` are required keys
whose value is `null` while the later slices have not computed them. Null means
unavailable, not zero or an empty set. Readers require these keys and accept the
populated shapes above as later slices provide them (Tom, 2026-10-27, decision
`ceaa90b1`). A missing policy remains undeclared.

`state` is one of `above_reserve`, `below_reserve`, `below_floor`,
`policy_undeclared`, `volume_absent`, `space_unknown`, `durability_forbids`.
A report never prunes and never writes, and it takes only the shared volume lock,
only when the lock file already exists.

### `barge.space.guard/1`

A completed decision is a success document, including a stop that deleted
something:

```json
{
  "schema": "barge.space.guard/1",
  "command": "space guard",
  "ok": true,
  "data": {
    "decision": "stop_below_floor",
    "volume_uuid": "…", "mount_hint": "/Volumes/Builds",
    "area": {"namespace": "oxide", "area": "pool"},
    "before": {"free_bytes": 3435973836, "observed_ms": 0},
    "after": {"free_bytes": 4724464024, "observed_ms": 0},
    "policy": {"reserve_bytes": 21474836480, "floor_bytes": 10737418240},
    "pruned": {"rows": [{"root_id": "…", "area": "oxide/pool",
                        "bytes": 1288490188, "last_release_ms": 0}],
               "count": 1, "total": 1, "truncated": false},
    "reclaimed_bytes": 1288490188,
    "rounds": 1,
    "decided_after_wait": false,
    "message": "…",
    "detail": {
      "free_bytes": 4724464024, "floor_bytes": 10737418240,
      "reserve_bytes": 21474836480,
      "pruned_root_ids": ["…"], "reclaimed_bytes": 1288490188,
      "owned_prunable_bytes": 0, "held_or_unknown_bytes": 43528138752,
      "excluded_bytes": 0, "other_estimate_bytes": 13851269529,
      "cause": "held_owned_roots",
      "measurement_source": "fstatfs.f_bavail*bsize",
      "observed_ms": 0
    }
  }
}
```

A **no-effect** refusal is not a guard document: it is stage 1's ordinary error
envelope, `barge.error/1`, naming the command and carrying the same `message` and
`detail` fields, with no `decision`, no `pruned` rows and no `barge.space.guard/1`
schema:

```json
{"schema": "barge.error/1", "command": "space guard", "ok": false,
 "error": {"kind": "space_below_floor", "message": "…", "detail": {"…": "…"}}}
```

`decision` is `admit`, `admit_below_reserve`, `continue` (a recheck that may
proceed) or `stop_below_floor`. A recheck reports `continue` in place of
`admit`/`admit_below_reserve` and is otherwise the same document. `pruned` lists
the roots this call deleted, in delete order;
`reclaimed_bytes` is the measured difference across the pass, not a sum of
recorded root sizes. `decided_after_wait` is true when another prune held the
volume lock past the lock wait, so this call decided on a fresh measurement
without pruning. An unsettled delete is a `barge.error/1` document with
`kind: "unknown_state"` at exit `3`, `detail.pending_intent` naming the intent id
and `detail.pruned_root_ids` naming anything already deleted.

Everything this command completes is a `barge.space.guard/1` success document,
including a stop that deleted something. Everything it refuses or cannot decide
is a `barge.error/1` document, whose closed `kind` set this stage extends by
`space_below_floor` and no more. A consumer reads one schema for effects and the
other for refusals, and never the same schema with two meanings.

### `barge.space.prune/1`

The plan is canonical: one candidate per row, in the exact delete order, with
`root_id`, `area`, `bytes`, `last_release_ms`, `first_created_ms`, and an
identity digest over device, inode and marker digest. The plan's SHA-256 covers
the plan bytes only; the measurement is beside it, not inside it, so a
re-measurement does not invalidate a plan whose candidates are still valid. A
real run requires the plan bytes and digest, and re-verifies each candidate's
identity, holds, custody, policy generation and volume before its intent.

## Lock order with the stage 1 locks

Stage 1 fixes the order and enforces it as a checked rank
([state](stage1-design.md#account-wide-state-directory)). This stage adds no
rank; it uses the existing ones, in the same order, and adds rules about which
locks are held for which step.

| Rank | Lock | Held by stage 3 for | Never held across |
| --- | --- | --- | --- |
| 1 | `registry.lock` | the policy snapshot read (shared), and a registration write (exclusive) | a measurement, a volume walk, a deletion |
| 2 | `volume-<uuid>.lock` | a prune pass (exclusive, held from its first authoritative measurement to its final decision); a report (shared, only if the file exists); an operator prune's real run; a registration write | a registry lock write |
| 3 | `root-<id>.lock` | `hold`, `release` and the activity write; one candidate's plan → intent → rename → delete | another root's effect |
| 4 | intent write (no lock file) | the intent publish before an effect | — |

Rules a reviewer can check:

1. **Strictly ascending, never descending.** `LockSet` refuses a rank at or
   below one already held. Stage 3 therefore reads the registry **before** taking
   a volume lock, and re-reads it lock-free (stage 1's atomic publish) before an
   effect rather than re-taking rank 1.
2. **The registry lock is never held across a walk or a delete.** A long prune
   holds the volume and one root lock, and re-reads the registry bytes it needs
   (bounded, 4 MiB, 10 s) before publishing its intent.
3. **The volume lock is held for the whole prune pass; only the root lock is
   released between candidates.** A pass takes the volume lock before its first
   authoritative measurement and releases it after its final decision, so two
   prunes on one volume cannot overlap and no deadlock is possible. Each
   candidate's effect takes its root lock after the volume lock, one root at a
   time, and releases it before the next candidate, so a consumer's `hold` waits
   for at most one deletion, never for a whole batch.
4. **No consumer ever holds a root and asks for a volume lock.** That would be a
   descending acquisition (`usage` refusal); the API documents the two phases —
   decide, then hold — and the guard's own code holds no root lock.
5. **Holds are not locks.** Hold files are read as evidence under the root lock;
   a prune never blocks on a hold, because a hold is a file, not a lock. A
   consumer's `hold` may wait for the root lock a prune holds and then fail
   `locked`, which is waiting on a **lock**, not on a hold; that is the only
   wait either side can do for the other.
6. **The intent is last.** It takes no lock file, and it is durable before the
   first destructive step, exactly as stage 1 defines.

A lock that cannot be taken within the deadline that the operation *needs* — an
operator prune's real run, a registration write — is `locked`, names the lock
file, changes nothing, and is never broken. A guard never treats the volume lock
that way: it measures without it and decides (`decided_after_wait`), because an
admission decision needs no lock to act.

## Failure, race and cancellation matrix

Every row is a rule the implementation must carry a test for. "Keeps" means the
data, marker and record are unchanged and reported.

| Trigger | Detection | Action |
| --- | --- | --- |
| Drive pulled before a decision | mount root will not open; UUID or device mismatch; `EIO`/`ENOENT` on the volume descriptor | refuse `volume_absent`, naming the drive; nothing measured, pruned or created |
| Drive pulled mid-prune | a descriptor-relative call fails after the quarantine rename | stop; keep the pending intent and the quarantine leaf; exit `3`; replay only after the volume reverifies and the recorded identity matches |
| Drive pulled during the flush | flush or the reverify around it fails | post-prune measurement unknown; `unknown_state` at exit `3`, reporting the roots already deleted; no admission |
| Measurement unknown or overflowing | absent field, `None`, checked-multiply failure | `unknown_state`; never treat as zero or as plenty |
| Purgeable space not materialised | a batch reclaimed nothing and the flush succeeded | stop pruning; report `space_not_reclaimed` with both measurements; the outcome is the floor's (completed stop when something was deleted) |
| Unlinked-but-open file by a foreign writer | same as above | same; the cause is named as undecidable |
| Free space fell again between flush and decision | the deciding measurement itself | the decision uses the deciding measurement; a recheck re-measures; no cached number is reused |
| Two guards at once on one volume | the volume lock, held by the pass | the second waits ≤ 5 s, re-measures without the lock and decides (`decided_after_wait`); it never prunes, so the two can never delete the same root |
| A guard needs to prune and cannot take the volume lock | the lock wait expires with a live owner | it prunes nothing, re-measures without the lock and applies the decision table to that measurement: `admit`/`admit_below_reserve` on it, or the no-effect `space_below_floor` refusal when `F <= L`; `decided_after_wait` names the wait; it never deletes and never returns `locked` |
| Two guards on two volumes | per-volume lock | both proceed; no global lock |
| A consumer's `hold` while the prune already holds that root's lock | root lock held by the prune | the hold waits for the lock up to its own deadline, then fails `locked`; nothing is written into the root and the prune completes its verified delete |
| A consumer's `hold` recorded before the prune takes that root's lock | the hold file read under the root lock at the effect step | skip the root and report it as `held`; the hold stays valid and the consumer writes normally; no partial delete |
| Consumer releases a hold mid-pass | the activity record and hold scan at the effect step | the root becomes eligible; it is considered in the next batch, never retroactively in the current plan |
| Hold file unreadable, foreign uid, or pid alive with no start identity | custody evidence | `Unknown`; skip and report `held_or_unknown_bytes`; never idle |
| Holder pid reused | pid + start identity + boot identity all disagree | stale only when all three positively disagree; otherwise unknown |
| Activity record missing or invalid | activity read | `activity_unknown`; skip and report; the repair is an explicit `area idle-baseline` |
| Activity timestamp in the future | clock skew | not idle; report `unknown_state` |
| Marker unverifiable, foreign family, or another device | stage 1 marker rules | skip and report; never a candidate |
| Root becomes absent or renamed between plan and effect | identity re-verification | skip with `identity_changed`; no delete |
| Registry changed between snapshot and effect | generation compare on a bounded re-read | re-evaluate that candidate against the new policy; skip if it no longer qualifies (`policy_changed`); continue on a fresh measurement |
| Registry unreadable or invalid | bounded re-read | stop; delete nothing further; `unknown_state` at exit `3` naming the file, with anything already deleted in `detail` |
| Area declaration withdrawn mid-pass | declaration re-read with the registry snapshot | the root stops being a candidate; skip and report |
| Floor or reserve edited while work runs | the recheck's fresh policy read | the new thresholds apply at that checkpoint; documented, not a race |
| Volume's durability changed to `kept` mid-pass | registry generation compare | stop; delete nothing further; report `unknown_state`/`refused_boundary` at exit `3`, naming what was already deleted |
| Pending intent from an earlier prune | intent directory | that root is skipped and reported; the guard `unknown_state` (exit `3`) with `detail.pending_intent`, rather than admitting over an unsettled delete |
| `ENOSPC` on the state volume | intent or activity write | refuse before the effect; nothing deleted; report the state volume |
| Signal during a batch (SIGINT/SIGTERM) | signal | finish the current durable step, leave the intent (never an unrecorded effect), release locks, exit non-zero |
| Prune budget exhausted mid-batch | per-root and per-pass deadlines | stop; decide from the last flushed measurement; report `budget_exhausted` and what was not reached, in the completed-stop or no-effect-refusal shape that matches whether anything was deleted |
| Finder recreates `.DS_Store` during the delete | `ENOTEMPTY`/`EEXIST` | stage 1's three attempts, 1 s apart, then pending intent and exit `3`; never a wider delete |
| Unknown entry inside a chosen root | the walk | stop, keep the root, report the name; the candidate is skipped for the rest of the pass |
| Two registration writes race | exclusive registry lock and `generation` | one wins; the loser re-reads and reports the newer generation; no lost update |
| Registration sees an absent volume | verification | refuse `volume_absent`; nothing written |
| A floor above the measured `total_bytes` at registration | registration validation | refuse `usage`; nothing written |
| A memory or file bound is reached | bounds | report the unfinished part with counts; never report it as done |

## Bounded resources

Stage 1's bounds stand; this stage adds and reuses these.

| Bound | Value |
| --- | --- |
| Registry read (policy snapshot and re-read) | 4 MiB, 64 records, 10 s |
| Activity record / activity rows read | 4 KiB each / 65 536 rows / 10 s |
| Measurement per decision | 1 `fstatfs`; at most 4 per guard call |
| Volume flush per batch | 1; 30 s deadline |
| Root deletions per guard call | at most 128 (at most 4 batches of 32) |
| Guard call wall clock | 30 s default, caller may ask up to 120 s |
| Per-root delete deadline inside a guard | `min(120 s, remaining budget − 5 s)` |
| Candidate rows per plan and per area listing | 4096 roots, 10 s |
| Space documents (`space`, `guard`, `prune`) | 4 MiB, 4096 rows, `truncated` with counts |
| Registration plan and write | 64 KiB, one atomic publish |
| `maintain`'s space stage | inside stage 1's 600 s run, stopping 30 s before its deadline |

A bound that is reached reports what was not reached, with counts; it never
reports the unfinished part as done, and it never turns into a longer deadline.

## Alternatives considered and rejected

| Alternative | Why rejected |
| --- | --- |
| Keep the reserve as a stop and fall back to the internal pool at or below it (Oxide's current selection) | It is the exact behaviour Tom's ruling retires: low space must never silently move work onto the internal SSD. The reserve triggers pruning; the floor stops. |
| Treat the reserve as a hard stop with no floor | Then a volume with no idle data refuses work while free space is still usable, which loses the drive to an arbitrary number. The floor is the stop because it is the line where writing stops being safe. |
| Enforce the reserve also below the floor ("prune until above the reserve or refuse forever") | With nothing left to prune, the reserve would refuse work the floor permits, turning the trigger into a stop by the back door. |
| Prune a fixed number of roots, or a percentage of the volume | A fixed count frees an arbitrary amount and a percentage ignores the actual thresholds; Oxide's own decision already rejected a fixed eviction count. Prune until the measurement satisfies the policy, bounded by time and roots. |
| Order candidates by mtime, size or the consumer's ledger | mtime and ledgers are consumer state that moves without a hold, and the ruling says idleness comes only from Barge's own primitive. LRU over recorded releases is the only order Barge can defend. |
| Order candidates per area (round-robin or fair share) | It would keep a young root in one area while deleting an older one in another; "least-recently-used across areas" is what the ruling says, and the per-area policy already gates eligibility. |
| Treat a root with no activity record as idle because it has no hold | A lost state directory or a pre-stage-3 root would become a deletion candidate; unknown is never idle. The repair is explicit. |
| Delete a cache placement's whole root to reclaim space | The root is a link target: deleting it leaves the consumer's original path dangling at nothing. Reclaiming caches is a placement-level operation with its own slice and gates. |
| Prune a consumer's data under an age policy when the reserve is breached | Age asks "has this expired?"; pressure asks "may this yield now?". Conflating them either blocks recovery until a week has passed or expires fresh data silently. Hence the separate `reserve` participation. |
| Enforce a per-area cap or quota in Barge (APFS quota per area) | A cap is the consumer's accounting, quotas need root and apply to a filesystem not a folder, and neither produces the cross-area LRU the reserve needs. Barge reports area bytes and enforces nothing. |
| Import Oxide's `reserve_bytes` as Barge's reserve | Its meaning is the opposite (a selection stop). Importing it would silently convert a stop into a trigger with a number nobody chose for that purpose. |
| Reservations or leases for admitted bytes | Stage 3 has no way to size a command (that is the consumer's accounting), a reservation needs orphan recovery and a second durable lifecycle, and the floor plus the consumer's own cap already produce the required behaviour. Named as an open issue instead. |
| A background watchdog that prunes on a timer | The vision excludes a daemon; `maintain` already gives a scheduled pass, and consumers recheck at their own checkpoints. |
| Preview-then-real for the guard's prune | Admission is one call on a live path; requiring a plan round-trip would mean measuring, returning, and being admitted later on stale facts. The guard re-verifies each candidate at its effect step, and reports the plan it executed; the operator's `space prune` keeps the strict preview discipline. |
| `lsof` or a process scan as the "no writer" proof | Stage 1's limit stands: external, unbounded, and its silence is not a kernel guarantee. Custody stays cooperative plus same-uid descriptor observation. |
| Delete more when a flush shows no reclaim (widen the batch) | It would delete data for no measured effect, and the cause (purgeable space, a foreign open file) is undecidable. Stop, report `space_not_reclaimed`, and let the floor stop work. |
| Report a stop that pruned as a refusal at exit `2` | Stage 1's exit `2` means "refused before any effect", so a document listing deleted roots in the same envelope would be false about its own behaviour, and a consumer could not tell a stop it can act on from a refusal that changed nothing. A stop that changed something is a completed command at exit `0`; only a pass that deleted nothing is a refusal. |
| Rewrite the root marker to record activity | The marker is written once and is never rewritten to add a fact another process could have raced; activity lives in account state beside it. |
| A `--force` pruning or admission escape hatch | Any force path lets an unknown become safe, which stage 1 rejected for deletion; the floor is the honest stop. |

## Unit and counterfactual tests

Unit tests carry the decision tables; counterfactual tests construct the state
each rule must refuse, so a refusal is proven real rather than asserted. All of
these are proposed for the slices below, not run by this design task.

| Area | Unit | Counterfactual |
| --- | --- | --- |
| Measurement | `fstatfs` mapping to free/total with checked multiply; `source` and `observed_ms` always present; purgeable reported as unknown | an absent field, an overflow and a `None` all yield `unknown` and a refusal, never zero; `f_bfree` never decides; a different volume at the mount point is `volume_mismatch` |
| Policy record | optional `space` read/write; default floor written explicitly; generation increases; `--plan` digest | an absent `space` refuses a space-gated decision and still reports; a strict reader that predates the field refuses a registry carrying `space` as `unknown_state` rather than reading it without the policy; `floor_bytes == 0`, `reserve_bytes == 0` and a floor above `total_bytes` refuse at registration with nothing written; a floor below the default is reported, not silently raised |
| Decision table | every cell of the table, with `R < L`, `R == L` and `R > L`; and the envelope each outcome takes | `F` just above `R` with `L > R` stops (the floor wins); `F <= R` with nothing eligible admits with the below-reserve warning; `F <= L` with a candidate that reclaims nothing yields a **completed stop** (`ok: true`, `decision: stop_below_floor`, exit `0`, `space_not_reclaimed`) because it deleted something, while the same floor with nothing eligible yields the `space_below_floor` no-effect refusal (exit `2`); a completed stop is never encoded as a `space_below_floor` error, and a no-effect refusal never lists a pruned root |
| Idleness | the eligible/ineligible matrix over marker, activity, hold and participation | missing activity, unreadable hold, live pid with mismatched start identity, future timestamp, foreign marker, other device, unconcluded scratch and `excluded` area roots each stay kept and reported |
| LRU order | release, then created, then id ordering; stable across areas | two roots in different areas: the older release goes first; equal releases fall to `first_created_ms` and then `root_id`; a held older root in front is skipped, not deferred silently |
| Prune custody | plan → intent → rename → delete phases; marker last; Finder tolerance; `ENOTEMPTY` | an unknown entry stops the delete; an injected device change stops before the first unlink; a symlink inside the quarantine stops it |
| Reclaim | flush after the batch, then measure; zero-reclaim detection | a flush failure yields `unknown_state` at exit `3` with the deleted roots reported; an injected zero reclaim stops the pass instead of deleting more |
| Concurrency | two guards on one volume: the second waits, never prunes, re-measures without the lock and decides `decided_after_wait`; two volumes proceed; an operator prune's real run needs the lock | an injected held volume lock yields `decided_after_wait` for a guard and `locked` (nothing changed) for an operator prune's real run; a guard never returns `locked`; a consumer `hold` while the prune holds that root lock fails `locked` and the root is untouched; a hold recorded before the prune takes the root lock makes it skip that root and report `held` |
| Policy snapshot | generation compare before each effect | a registry changed to withdraw the area skips the candidate; a changed reserve re-evaluates; an invalid registry stops the pass with nothing further deleted |
| Registration | register/update/describe documents and errors | an absent volume, an unverified UUID and a malformed size each refuse with nothing written; a concurrent update loses nothing (generation) |
| Fallback exclusion | `SpaceDecision` has no fallback variant; a present volume's area resolution never returns a local path | a low-space decision on a present volume never yields a path; a fallback root present while its volume is present is reported `fallback_while_present` |
| Soft cap | `area_bytes` sums that area's roots only | two areas on one volume: adding scratch, cache and a second area's bytes never changes the first area's `area_bytes` |
| Contacts | every document matches its schema; unknown major refuses; optional addition accepted | a `barge.space/1` document with a removed field or changed type is refused by its own reader; an older `describe` without `space` is valid; a no-effect refusal is a `barge.error/1` document and a stop that pruned is a `barge.space.guard/1` success, and each reader refuses the other's schema |

## One bounded native design

One focused native design, proposed for the registration, measurement, prune and
guard slices together. It is a design, not a claim: this task runs nothing
native, and a missing receipt from the slice that runs it is a missing proof.

### Apparatus

- **One owned APFS sparse image**, `hdiutil create -size 128m -fs APFS
  -type SPARSE` at a unique path inside the guard's scratch, attached with
  `hdiutil attach -nobrowse -owners on -plist` at a unique `/Volumes/<name>`,
  ordinary detach only. 128 MiB is stage 1's size and enough for three reachable
  thresholds: `reserve = 48 MiB`, `floor = 24 MiB`, and roots large enough that
  one deletion visibly moves the measurement.
- **One fake internal directory pool** under the same scratch — the only thing a
  fallback could ever write to — so "no work went to the internal drive" is a
  checkable fact (`walk the pool; expect nothing`) rather than an assertion.
- **Oxide's qualified shared supervisor**, at the pinned bytes
  (`scripts/bounded-test.py --limit 300 --scratch-budget-mib 512`), owning the
  deadline, the 20 GiB host reserve watchdog, owned-byte accounting, positive
  closure and cleanup. Barge adds no second supervisor, session or accounting
  harness.
- **A private account home and state directory** for the whole session
  (`BARGE_STATE_DIR` plus the fixture account home), a private `TMPDIR`, and a
  before/after digest of the real state directory to prove nothing touched it.

### Cases

| # | Case | What it proves |
| --- | --- | --- |
| 1 | Register the image with `reserve 48 MiB`, `floor 24 MiB`; fill two areas with roots so free is above the reserve; guard | admission with no prune, no flush, nothing deleted |
| 2 | Fill below the reserve with idle roots in two areas of different ages; guard | LRU across areas: the older-release root from the second area goes first; admission once free is back above the reserve |
| 3 | Hold one root and keep writing to it in a second process while a guard prunes the rest | the held root's inode and bytes are unchanged, the writer's data is intact, the pass reports it as held |
| 4 | Fill below the floor with every eligible root held | no-effect refusal `space_below_floor` at exit `2` (the pass deleted nothing); the message and detail carry free, floor, reserve, held bytes and cause; the fake internal pool is empty afterwards |
| 4b | Fill below the floor again, this time with one large eligible idle root | the pass deletes it and the floor is still breached: a **completed stop** (`ok: true`, `decision: stop_below_floor`, exit `0`) listing the deleted root, and the fake internal pool is still empty |
| 5 | One root with an unreadable hold file and one with no activity record, both large | both survive, both are reported, and the pass never treats them as idle |
| 6 | Register a second fixture volume with `reserve 16 MiB`, `floor 32 MiB`; fill so free sits between the two (above the reserve, below the floor), with nothing eligible left | the floor wins: the guard prunes, then stops at 32 MiB although free is above the 16 MiB reserve, in the completed-stop shape if its prune deleted anything |
| 7 | Delete a chosen root whose file is still open by a same-uid process, so free does not rise | `space_not_reclaimed` after the flush, no further deletion, and after the writer exits a recheck sees the space |
| 8 | Detach the image (ordinary, after the writers finish) mid-pass, then re-attach | pending intent with the quarantine leaf kept, exit `3`, replay completes after re-attach; no stale mount point written |
| 9 | A stale `/Volumes/<name>` directory left after detach | never written to, never recreated; the decision refuses |
| 10 | Registration refusals on the image: absent, unverified UUID, malformed size, floor above total | each refuses with nothing written and no mount created |

Explicitly **not** claimed: a real USB or Thunderbolt surprise removal, a real
drive's flush latency, the internal SSD write saving, APFS purgeable behaviour
under real pressure (a 128 MiB image has none), or any qualification of Oxide's
stage 4 adoption.

### Nine-premise HIGH checklist

Recorded in the proof's own plan and receipt:

1. A private `HOME`, `CARGO_HOME` and state directory, so no real account state
   is read or written.
2. The account home used for state resolution is the fixture's, and the real
   account state is untouched — asserted by a before/after digest of the real
   state directory.
3. Environment isolation: `BARGE_STATE_DIR`, `TMPDIR`/`TMP`/`TEMP` and any
   inherited scratch point inside the guard's owned scratch; no inherited real
   path.
4. Explicit adoption only: every fixture volume, area and placement is registered
   or declared with an explicit command and a recorded plan, never by a default
   or a discovery sweep; and no fixture ever names a real volume UUID.
5. The owned image is attached with ownership enabled (`-owners on`) and detached
   ordinarily; a forced detach would target only the owned image's own whole-disk
   device, and no real drive is ever attached, detached or written.
6. Every flag, environment variable and fault-injection seam used is listed with
   its purpose. There is no product debug bypass: failing `Flush`, failing
   `fstatfs` and failing measurement are injected through the OS trait in unit
   tests, never through an environment variable in shipped code.
7. Temporary storage: everything inside the guard's one owned root; no home, no
   drive root, no fixed `/tmp/<name>`, no fixture parent directory.
8. `mktemp -d` under that root for every fixture directory, never a shared name,
   never a link to a real checkout.
9. Oxide's qualified runner bytes and selector are named and hash-pinned, and
   Barge adds no second supervisor, session or accounting harness.

### Elevation

Stage 3 deletes storage and writes to removable media, so its implementation is
**Elevated**. The path is Oxide's, reused rather than reinvented: a HIGH premise
record, then an uncounted diagnostic run (keep-going, latched first fault,
dependent-skip, no promotion of unknown to pass), then a separate counted author
run and a separate independent reviewer run, each separately activated and each
at an exact head. A missing native receipt is a missing proof, never a pass. This
design task produces none and claims none.

## Validation staging and the handoff to stage 4

### Staging

Each slice lands with tests and a documentation update, in this order, and no
slice skips a stage:

1. **Unit and counterfactual** (rows above) in the slice that adds the mechanic.
   No slice is "done" without the refusals its rules require.
2. **`effigy docs:check`** once per changed documentation input, with the fixed
   120 s wall clock and 64 MiB scratch in a fresh
   `/Volumes/Builds/scratch/barge/run.*`, removed afterwards.
3. **`effigy rust:compile`, `rust:test`, `rust:lint`, `rust:format`** through
   `oxide` for the slices that add Rust, per the repository's `AGENTS.md`.
4. **The bounded native design above**, under the qualified shared supervisor,
   before any real-drive action is proposed.
5. **A real-drive qualification** and the host-job change, each operator-gated
   and each its own brief. `maintain`'s space stage is only enabled after the
   qualification, and the host job keeps running Oxide's prune entry until stage
   4.

No native receipt from this design task exists or is claimed. A slice that cannot
run its native proof reports it missing rather than claiming the slice qualified.

### Handoff to stage 4

What Oxide adopts, in what order, and what Barge guarantees it. This is the
contract's half of the handover; Oxide's own Queue project owns its half, and
nothing here edits Oxide.

1. **Registration first.** Oxide registers Builds through `barge volume
   register` with the reserve and floor the operator chooses, and stops reading
   its own `reserve_bytes` as a selection stop. Its `removable.json` stays
   readable by Barge as an observation until both sides agree to retire it.
2. **Then areas.** Oxide declares `oxide/pool` and `oxide/outputs` with
   `reserve: counts` (or `excluded`, its choice) and its own `prune` policy, and
   keeps the meaning of what lives inside a root: slots, leases, ledgers,
   receipts and member links stay Oxide's.
3. **Then the primitives.** Oxide replaces its own slot-root deletion and output
   prune with Barge's hold, release, list and prune, in that order, one verb at a
   time, each with its own proof. Barge never imports an Oxide ledger, receipt or
   marker meaning, and never interprets a Cargo layout.
4. **Then the fallback retires.** Oxide's below-reserve selection fallback and
   any low-space internal fallback are removed in Oxide's own change, after
   Barge's guard is in use. Until then the two behaviours coexist and this
   contract claims only its own.
5. **Then the host job.** Oxide's prune entry leaves the host job when stage 4's
   area adoption is qualified; the job's own file, label and schedule remain
   operator-gated host changes.

Barge's guarantee to a stage 4 consumer, in one paragraph: a `space::guard`
before work and a `space::recheck` at checkpoints give an admission or a stop on
measured facts, a declared policy and Barge's own custody; a stop means the floor
with the free, reserve and floor figures and the cause; no decision ever names an
internal path; and a held root is never pruned. Everything else — what a slot is,
what a ledger says, how much a build will write — stays the consumer's.

## Implementation slices and dependencies

Finite, in order, each landing with its tests and its documentation update. No
slice authorizes a native run, a real drive or a host change on its own, and none
starts before stage 1's S3 (areas, roots, holds) and S5 (custody, delete) are on
`main`.

| Slice | Content | Depends on |
| --- | --- | --- |
| T1 | `os::SpaceFacts` (`fstatfs` free/total with checked arithmetic and provenance) and `os::Flush`; `barge.space/1` and `barge space --json`; measurement and flush unit tests | stage 1 S1 |
| T2 | The `space` field on `barge.registry/1`; `barge volume register/update/describe` with plan and digest, validation and generation; `describe` space rows and the new format names | T1, stage 1 S2 |
| T3 | `barge.activity/1`; `hold`/`release` taking the root lock; `area idle-baseline`; the idleness matrix and the LRU order | T2, stage 1 S3 |
| T4 | Candidate planning for space (`barge.space.prune/1`), the area `reserve` participation rules, scratch's relaxed expiry predicate | T3, stage 1 S3 |
| T5 | The space prune: per-root delete under the stage 1 custody, intent and quarantine protocol; flush and reclaim; the batch, round and deadline bounds; operator `space prune --plan` and replay | T4, stage 1 S5 |
| T6 | The guard and recheck: admission decision table, `barge.space.guard/1`, attribution, the completed-stop and no-effect-refusal shapes with their exit codes, concurrency (`decided_after_wait`), policy-generation re-evaluation | T5 |
| T7 | `maintain`'s space stage, `describe` completion, and the single-command contract for the host job (the plist change itself stays operator-gated) | T6, stage 1 S7 |
| T8 | The bounded native proof above, as its own brief with the Elevated gates | T1–T7 |
| T9 | A real-drive qualification and the host-job change | T8, operator gate |
| T10 | A cache-placement reclaim slice (whole-root deletion with link handling), only if stage 3 and stage 4 show it is needed | T7, stage 1 S6 |

Dependencies that are not slices: Oxide's registration, area and primitive
adoption (stage 4, Oxide's project); Effigy's scratch move; Linux.

## Open design issues

Named, with the proposed resolution and what the implementation brief must do.
None is a qualification claim, and none contradicts a frozen ruling.

- **O-010 — Measurement accuracy on APFS under pressure.** `f_bavail` may include
  purgeable bytes that only materialise under pressure, and a small image has
  none. Proposed: keep `fstatfs` as the only decision input, report `purgeable:
  unknown` in every document, prune to `max(R, L) + margin` by making the reserve
  the operator's margin, and never claim the floor is exact to the byte. The
  implementation brief must fix the `f_bsize`/`f_bavail` mapping and its overflow
  refusal, and the native proof must state that purgeable behaviour is unmeasured
  there.
- **O-011 — A volume with no declared space policy.** Proposed: absent means
  undeclared, and space-gated work refuses until an operator registers a policy;
  reports still measure. The implementation brief must confirm that no path
  defaults a missing reserve or floor, and that `describe`/`space` make the
  missing policy obvious rather than looking like "no limit".
- **O-012 — Reservations for admitted work.** Proposed: none in stage 3; a
  consumer's own cap owns sizing, and Barge's job is the volume's thresholds. The
  implementation brief must not add a lease, and stage 4's Oxide adoption must
  keep its own cap arithmetic. If a second consumer ever shares one volume's
  reserve, this becomes a real design question with concrete alternatives.
- **O-013 — Activity record loss.** Proposed: a missing or invalid activity
  record is `activity_unknown`, never idle, and the repair is the explicit
  operator `area idle-baseline`. The implementation brief must fix that verb's
  refusals (a hold present, an undecidable hold, an unverified marker) and
  document that losing account state makes roots kept until an operator acts,
  rather than silently prunable.
- **O-014 — A writer that ignores the hold protocol.** The stage 1 capability
  limit stands: a same-uid scan can miss a process that starts after the scan, and
  a foreign uid is invisible. Proposed: keep the hold requirement, keep the
  intent and quarantine recovery, and report the limit. The implementation brief
  must pick the concrete observation primitive (the stage 1 O-001 decision) and
  test that an open descriptor of this uid, inside a chosen root, refuses the
  delete.
- **O-015 — The scratch pressure predicate.** Proposed: a scratch root is
  pressure-eligible only when it also satisfies the expiry predicate (positive
  closure, exclusive nesting, expirable reason) with the age requirement relaxed
  to "idle now"; an unconcluded root is never pressure-pruned. The alternative is
  to treat a no-holder unconcluded root as reclaimable, which risks deleting a
  crashed run's evidence. The implementation brief must confirm the choice with
  the owner of the scratch data if it wants the alternative.
- **O-016 — Source tension in Oxide's recorded behaviour.** Oxide's
  `removable::verified` refuses the volume at or below its reserve and selects
  the internal pool, and `local-lease.md` still records that selection fallback
  as current. Barge implements none of it; the retirement is Oxide's stage 4
  change. Named so no reader concludes that this contract already changed Oxide,
  and so the reviewer can check the carried ruling rather than the source.

## Review questions

Each is a decision this design took where the rulings stop, with the alternative
a reviewer may prefer. None blocks the design from being reviewed as a whole.

1. **Is `space` participation the right home for "may yield under pressure"?**
   Alternative: add a separate `under_reserve` field to the area declaration and
   leave `reserve` meaning only cap accounting. The chosen reading gives the
   frozen field one job and keeps the age policy independent; the alternative
   separates two axes but adds a field to a frozen format.
2. **Should the guard's live prune really skip the plan round-trip?** Alternative:
   require `--plan` for every prune, including admission, which makes admission
   two commands and admits on facts measured a call earlier. The chosen reading
   re-verifies each candidate at its effect step and reports the executed plan.
3. **Is `128 MiB` with `48/24 MiB` thresholds a fair native design?** Alternative:
   a larger image with larger numbers, at more scratch budget and a longer
   deadline. The chosen numbers keep stage 1's cap and make all three thresholds
   crossing cheaply and deterministically.
4. **Should caches be excluded from pressure pruning in stage 3?** Alternative:
   delete a cache root whole and repair its link, which reclaims the largest
   bytes but leaves a consumer path momentarily dangling and needs a link-repair
   mechanism this stage does not have.
5. **Is the completed-stop split the right envelope?** A stop that deleted
   something is `ok: true` at exit `0` with `decision: stop_below_floor`, and only
   a pass that deleted nothing is `space_below_floor` at exit `2`. Alternative:
   always report the floor as an error, reusing `resource_bound` or
   `refused_boundary`, which would put a report of deleted roots inside an
   envelope that promises no effect and lose the distinction between a policy
   stop and a bound.
6. **Should a floor below the 10 GiB default be allowed?** Alternative: refuse
   anything under the default, which makes the default a minimum rather than a
   default and takes an operator's own margin away.
7. **Is the `activity` record the right place for last-use?** Alternative:
   rewrite the root marker, which the frozen marker contract forbids, or rely on
   hold-file mtimes, which are not durable and disappear on release.

### Implemented measurement and registration surface

The library's `os::SpaceFacts` reports checked `fstatfs` block counts with
source, completion time and device provenance; `os::Flush` exposes the macOS
whole-volume flush. `barge volume register` and `update` preview a record in a
plan with its SHA-256; publication requires the saved plan and its exact digest,
rechecks the registry generation and volume, and writes the registry atomically.
`barge volume describe` reads the registered record. Their document formats are
`barge.volume.register/1`, `barge.volume.update/1`, and
`barge.volume.describe/1`.

`barge space` reads the registry and measures present registered volumes without
creating files. Its rows and the space rows in `barge describe` report the
declared policy, or `policy_undeclared` when absent, and identify a floor below
the default with `floor_below_default`. An absent registered volume is reported
as absent. Area attribution and policy decisions belong to their later slices.
