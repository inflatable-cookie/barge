# Architecture

Barge is one Rust library and a thin CLI over it. Oxide links the library and
works with nothing else installed. Other tools use the CLI and its JSON output.
There is no daemon: state lives in small files, guarded by file locks.

This is the lean design adopted on 2026-11-17 ([vision](vision.md#authority)).
It replaces the earlier design, whose files are deleted together with its code.

## Safety model

Everything Barge prunes can be rebuilt. The guarantee is sized to match:

- **Barge deletes only what it owns:** a root it created, whose marker matches
  the area and root ID. Anything else in an area is reported and never deleted.
- **Holds are cooperative.** A consumer holds a root with a file lock while it
  uses it. A root is idle only when its lock can be taken. Barge does not scan
  processes or open files. A process that writes into a root without holding
  it, or that closes an inherited hold descriptor, is outside the guarantee,
  and these docs say so rather than claiming more.
- **Eviction never reuses a path.** Evicting a root renames it into the area's
  trash and deletes it there. A later root always gets a new ID and a new path.
  A straggler that escaped its hold can reach only the trash copy, or recreate
  an unmarked stray folder under the old path; it can never write into a root
  that a new holder owns.
- **Eviction decides atomically.** Eviction takes the root's exclusive lock
  without waiting, then rechecks the pin under the area lock, which pinning
  also takes, before it renames. An earlier "idle and unpinned" reading never
  authorises an eviction by itself.
- **Deletion stays inside what it owns.** Deleting never follows a symlink and
  never descends into another filesystem (a nested mount). A trash entry is
  deleted only when its marker still matches the area; being inside `.trash/`
  is not enough.
- **A crash leaves trash, not ambiguity.** A rename is atomic. A crash before
  the rename leaves the root intact; a crash after it leaves owned trash, which
  the next prune or sweep removes. No journal is needed.
- **Unknown is never safe.** A volume whose identity can't be read, a marker
  that can't be parsed, or a lock that can't be tested all count as "keep".

## Concepts

- **Volume.** A registered external volume: name, UUID, mount point,
  availability (`permanent` or `ephemeral`), durability (`expendable` or
  `kept`), `root_dir`, reserve and floor. Barge checks its identity (UUID at the
  expected mount) on every use, and never uses a different volume found at the
  same mount point.
- **Area.** A folder a consumer declares, named `<namespace>/<area>` (for
  example `oxide/pool`, `oxide/outputs`). Its path defaults to
  `<root_dir>/<namespace>/<area>/` and can be overridden per volume. The
  declaration carries the area's absent-volume policy, an optional soft cap,
  and whether it counts toward the volume reserve. The stored declaration is
  authoritative: a caller that presents a different one is refused, with the
  differing fields named (Q-013).
- **Root.** A directory inside an area that Barge created, named
  `<label>-<id>` with a random `id`. Its marker, `.barge-root.json`, records
  the area, ID, label and creation time. What the root contains is opaque to
  Barge.
- **Hold.** An advisory `flock` on the root's lock file, exclusive or shared.
  A consumer that keeps the descriptor inheritable passes the hold to its child
  processes, so the root stays held until the whole process tree has exited.
  Dropping the `Hold` records the release time.
- **Pin.** A durable record that protects a root from prune and eviction
  whether or not anything holds it. Oxide pins its selected output roots.
- **Trash.** `<area>/.trash/`. Eviction renames a root here before deleting it.

## Layout on a volume

```
<mount>/<root_dir>/
  <namespace>/<area>/          one declared area
    <label>-<id>/              a root, with .barge-root.json
    .trash/                    evicted roots awaiting deletion
    .barge/locks/<id>.lock     hold lock files
    .barge/pins/<id>.json      pins
    .barge/activity/<id>       last hold and release times
  scratch/<project>/           Barge's own scratch area per project
```

Account state lives in `~/Library/Application Support/barge/` on macOS
(`$XDG_STATE_HOME/barge/` on Linux; `BARGE_STATE_DIR` overrides both):
`volumes.json` and `areas/<namespace>.<area>.json`. Writes take the state lock
and publish by atomic rename.

Today's Builds layout (`scratch/`, `caches/`, `oxide-pool/`, `oxide-outputs/`
at the drive root) is expressed as `root_dir = ""` with Oxide's two areas
overridden to their existing folder names, so adoption moves nothing.

## Library surface

The names below are indicative; the implementation may adjust them where the
code reads better, as long as the behaviour holds.

```rust
let barge = Barge::open()?;                       // account state dir
barge.register_volume(VolumeRegistration::new("Builds", mount))?;
let vol = barge.volume("Builds")?;
vol.state()?;                                     // Present | Absent | Mismatch

let area = barge.area(
    AreaSpec::new("oxide", "pool", "Builds")
        .absent(Absent::Fallback(path))           // or Absent::Fail (the default)
        .soft_cap_bytes(Some(200 << 30))
        .counts_toward_reserve(true),
)?;
match area.location()? { Location::Volume(p) | Location::Fallback(p) => .., Location::Unavailable(why) => .. }

let root = area.create_root("slot")?;             // new <label>-<id>
let hold = area.hold(&root.id, Mode::Exclusive)?; // flock; try_hold never waits
let donor = area.hold(&other.id, Mode::Shared)?;  // seeding: hold a donor too
hold.set_inheritable(true)?;                      // the child keeps the hold across exec
area.list()?;                                     // roots, holds, pins, bytes, stragglers
area.pin(&root.id, "selected output")?; area.unpin(&root.id)?;
area.evict(&root.id)?;                            // refuses if held or pinned

barge.space("Builds")?;                           // free, reserve, floor
barge.prune("Builds", false)?;                    // LRU idle unpinned roots to the reserve
barge.admit("Builds")?;                           // prune to the floor; refuse only below it
barge.sweep_trash("Builds")?;

let run = barge.scratch("Builds", "effigy")?.create("test-run", ttl)?;
barge.sweep_scratch("Builds", Some("effigy"))?;   // expired idle roots
```

The stored documents carry their own schema names (`barge.volumes/1`,
`barge.area/1`, `barge.root/1`, `barge.pin/1`, `barge.activity/1`); a reader
that does not know one refuses it rather than guessing.

Warm reuse is the consumer's choice: a consumer that finds an idle root it
wants takes an exclusive hold on it and reuses it, contents and all. Barge
mints a new root only when asked to create one. Barge never decides
compatibility, freshness or what a root contains.

## Space policy

- A volume's **reserve** triggers pruning, it doesn't stop work. Below it,
  Barge evicts the least-recently-used idle, unpinned roots, from areas that
  count toward the reserve, until free space is back above it.
- An area's **soft cap** counts only that area's own bytes; above it, the
  area's own LRU idle roots are evicted.
- Only the **floor** (default 10 GiB) refuses work, and only once nothing idle
  is left to evict.
- Low space never moves work to the internal drive. The drive-absent fallback
  is a separate policy, declared per area.

## Absent volume

Each area declares one of two policies:

- `Fail`: report the volume to attach. Caches and scratch use this.
- `Fallback(path)`: use an explicit internal path and say so in the result
  and in status. Oxide's pool and outputs use this. When the volume returns,
  new roots go back to it; fallback roots stay where they are until they are
  pruned and are never merged back.

A root is found by its ID wherever it lives. `area.location()` decides only
where *new* roots go. Listing, holds, pins and eviction resolve an existing
root on the volume or in the fallback, so a fallback root that a consumer
still uses or wants to reuse stays visible after the volume returns.

Kept data (VMs, archives) is simply unavailable while its volume is away.

## CLI

`barge` wraps the library: `volume register|list|status`, `area list`,
`root list|pin|unpin|evict`, `prune [--dry-run]`, `scratch new|sweep`,
`status`. `--json` prints one document with a top-level `schema` field
(`barge.<kind>/<n>`); consumers check it and refuse unknown versions. Exit
codes: 0 success, 1 refused (nothing changed), 2 usage error, 3 internal error.

## Platforms

macOS on APFS first. The OS layer is deliberately small: volume identity (UUID
at a mount) and free space. Locks, renames and markers are portable, so Linux
support is a new identity and space implementation, not a redesign.

## Testing

Unit and integration tests run on temporary directories, with a fake volume
identity. Required cases:

- a hold blocks eviction and prune; a released hold allows them; a hold or
  pin taken after an idle reading but before the rename stops the eviction;
- deletion and the trash sweep don't follow symlinks or cross into a nested
  mount, and skip trash entries whose marker doesn't match;
- a fallback root stays listed, holdable and pinnable after the volume returns;
- a hold survives `fork` and `exec` when its descriptor is inherited;
- eviction renames to trash first; a crash after the rename leaves trash that
  the next sweep removes; a later root never reuses an evicted path;
- unmarked or foreign folders in an area are reported and never deleted;
- pins survive prune; the floor refuses only when nothing idle remains;
- a different volume at the registered mount is refused;
- a mismatched area declaration is refused with the differing fields named.

There are no native proof ladders or capture harnesses. Before Oxide adopts a
new Barge revision, run `barge volume status` and a `prune --dry-run` against
the real Builds drive by hand.

## Boundary with Oxide

Oxide keeps the Cargo front door, slots and leases, compatibility keys,
freshness, recipes and profiles, output member links, seeding logic, and
`oxide exec`. Barge supplies volumes, areas, roots, holds, pins, pruning,
trash and scratch. Barge knows nothing about Oxide: no Oxide formats, no
Oxide-specific modules, no adapters. Oxide tests that its own child-launch
path keeps the hold descriptor inherited.

## Later

- **Cache and kept placements:** an owned link at a tool's existing path,
  recorded and undoable; a tool's own setting written only on an explicit
  adopt (Q-003). Kept placements live outside every area and are never pruned.
- **VM mounts** such as Colima profiles (Q-009).
- **Resources beyond local storage**: see [vision](vision.md#direction).
