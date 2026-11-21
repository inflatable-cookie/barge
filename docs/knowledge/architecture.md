# Architecture

Barge is a small Rust workspace with one job per crate: `crates/barge` is the
storage library, `crates/barge-cli` the thin `barge` binary over it,
`crates/barge-machine` the machine registry, Wake-on-LAN and `barged` client,
`crates/barged` the machine daemon ([machines](machines.md)), and
`apps/barge-app` the menu-bar app ([app](app.md)). Oxide links the storage
library and works with nothing else installed. Other tools use the
CLI and its JSON output. Storage needs no daemon: state lives in small files,
guarded by file locks.

This is the lean design adopted on 2026-11-17 ([vision](vision.md#authority)).
It replaces the earlier design, whose files are deleted together with its code.

## Safety model

Retargeting checks the expected symlink immediately before replacement; a writer that changes that path between the check and rename is outside the cooperative guarantee.

Everything Barge prunes can be rebuilt. The guarantee is sized to match:

- **Barge deletes only what it owns:** a root whose marker matches the area and
  root ID. Anything else in an area is reported and never deleted.
- **Holds are cooperative.** A consumer holds a root with a file lock while it
  uses it. A root is idle only when its lock can be taken. Barge does not scan
  processes or open files. A process that writes into a root without holding
  it, or that closes an inherited hold descriptor, is outside the guarantee,
  and these docs say so rather than claiming more. Taking a hold compares the
  descriptor it locked with the file the lock path names and retries once, so a
  hold that woke on a lock file an eviction had already unlinked cannot protect
  a later root invisibly.
- **Barge never reuses a path it named itself.** Eviction renames a root into
  the area's trash and deletes it there. A later Barge-created root gets a new
  ID and path. A caller-chosen folder may be adopted again after eviction;
  protection from a straggler at that path then rests on the caller's holds and
  pins.
- **Eviction decides atomically.** Eviction takes the root's exclusive lock
  without waiting, then rechecks the pin under the area lock, which pinning
  also takes, and revalidates the root marker’s area, ID, label and folder
  under both locks immediately before it renames. Marker writes take the area
  lock too. An earlier "idle and unpinned" reading never
  authorises an eviction by itself. Pinning takes the same lock and rechecks
  the marker, so a pin cannot land on a root an eviction has already renamed
  away.
- **Deletion stays inside what it owns.** Deleting never follows a symlink and
  never descends into another filesystem (a nested mount). The walk is
  descriptor-relative — `openat` with `O_NOFOLLOW`, `fstatat`, `unlinkat` — and
  both the marker that proves ownership and the filesystem each folder is on are
  read from the descriptor that was opened, never from a path or an entry seen a
  moment earlier. A folder swapped for a symlink, or a mount swapped for a plain
  folder, therefore cannot redirect the walk; a folder a racing writer puts at
  the path after it is opened is never walked. Measuring a root uses the same
  walk, holds the same boundary and stops at the same depth. A trash entry is
  deleted only when its marker still matches the area; being inside `.trash/`
  is not enough.
- **A crash leaves trash, not ambiguity.** A rename is atomic. A crash before
  the rename leaves the root intact; a crash after it leaves owned trash, which
  the next prune or sweep removes. The area lock is released after the rename,
  before deletion, so holds and other area operations can continue. A sweep does
  the same: under that lock it lists the trash and claims what it will delete,
  renaming the entry to `.barge-claim-<name>` and locking the folder it opened,
  then releases the lock to walk the tree. A claim a crash leaves behind is
  finished by a later sweep; a claim locked by a live deletion is left alone, so
  no tree is deleted or counted twice and a large delete never stalls a hold. No
  journal is needed.
- **Unknown is never safe.** A volume whose identity can't be read, a marker
  that can't be parsed, a lock that can't be tested, or a pin that can't be
  inspected all count as "keep".

## Concepts

- **Volume.** A registered external volume: name, UUID, mount point,
  availability (`permanent` or `ephemeral`), durability (`expendable` or
  `kept`), `root_dir`, reserve and floor. Barge checks its identity (UUID at the
  expected mount root) on every use, and never uses a different volume found at the
  same mount point.
- **Area.** A folder a consumer declares, named `<namespace>/<area>` (for
  example `oxide/pool`, `oxide/outputs`). A namespace never carries a `.`, so the
  account file `<namespace>.<area>.json` names exactly one area. Its path defaults to
  `<root_dir>/<namespace>/<area>/` and can be overridden per volume. The
  declaration carries the area's absent-volume policy, an optional soft cap,
  and whether it counts toward the volume reserve. The stored declaration is
  authoritative: a caller that presents a different one is refused, with the
  differing fields named (Q-013).
- **Root.** A directory inside an area that Barge owns. Barge-created roots are
  named `<label>-<id>` with a random `id`; an existing directory can be adopted
  under its caller-chosen folder name. Its `.barge-root.json` marker records
  the area, ID, label, folder when needed, and creation time. What it contains
  is opaque to Barge.
- **Hold.** An advisory `flock` on the root's lock file, exclusive or shared.
  A consumer that keeps the descriptor inheritable passes the hold to its child
  processes, so the root stays held until the whole process tree has exited.
  Dropping the `Hold` records the release time. `barge worktree run` is the CLI
  form of this for a process that is not Rust ([worktrees](worktrees.md#run)).
- **Pin.** A durable record that protects a root from prune and eviction
  whether or not anything holds it. Oxide pins its selected output roots.
- **Trash.** `<area>/.trash/`. Eviction renames a root here before deleting it.

## Layout on a volume

```
<mount>/.barge/layout.json      the classes this volume holds (resources.md)
<mount>/<root_dir>/
  <namespace>/<area>/          one declared area
    <label>-<id>/              a Barge-created root, with .barge-root.json
    <caller-folder>/           an adopted root, with .barge-root.json
    .trash/                    evicted roots awaiting deletion
    .barge/area.lock           the area lock pinning, eviction and sweeps take
    .barge/locks/<id>.lock     hold lock files (removed with an evicted root)
    .barge/pins/<id>.json      pins
    .barge/activity/<id>.json  last hold and release times
  scratch/<project>/           Barge's own scratch area per project
```

Account state lives in `~/Library/Application Support/barge/` on macOS
(`$XDG_STATE_HOME/barge/` on Linux; `BARGE_STATE_DIR` overrides both):
`volumes.json`, `areas/<namespace>.<area>.json`, `layout.json`, `bindings.json`
and `machines.json` ([machines](machines.md#registry-state)). Writes take the
one `lock` file there and publish by atomic rename. The account layout is the
authority for class placement; each volume also carries `<mount>/.barge/layout.json`,
which lists the classes that volume holds ([resources](resources.md#layout)).

Today's External layout (`scratch/`, `caches/`, `oxide-pool/`, `oxide-outputs/`
at the drive root) is expressed as `root_dir = ""` with Oxide's two areas
overridden to their existing folder names, so adoption moves nothing.

## Library surface

Storage declarations, areas, pins, roots, holds, trash, space policy and scratch
live in separate library modules. Account state and platform operations live in
`state` and `os`; command-line rendering stays in the CLI. Test fixtures (a
temporary directory that removes itself) sit behind the `testing` feature, so
the library a consumer links carries no test helpers. A volume layout
record can be repaired from the authoritative account copy with
`repair_volume_layout` after an interrupted write. Survey marks macOS system
entries separately, so doctor does not treat them as unknown.


The names below are indicative; the implementation may adjust them where the
code reads better, as long as the behaviour holds.

```rust
let barge = Barge::open()?;                       // account state dir
let preview = Barge::open_read_only()?;           // passive account inspection
barge.register_volume(VolumeRegistration::new("Builds", mount))?;
let layout = barge.layout()?;                     // account resource placements
let scratch = barge.resource_path(Class::Scratch, Some("project"))?;
let survey = barge.survey("Builds", true)?;       // read-only inventory
let report = barge.doctor()?;                     // read-only drift report
let file = barge.workflow("project")?;           // the scratch file text, as init writes it
let init = barge.init(repo, None)?;               // write AGENTS.local.md, exclude it, and record it
let vol = barge.volume("Builds")?;
vol.state()?;                                     // Present | Absent | Mismatch

let bindings = barge.bindings();
bindings.list()?;                                 // recorded bindings
let binding = Binding::link("cargo-registry", Class::Caches, link)
    .to(class_folder);                            // the binding, passed explicitly
let plan = bindings.plan(&binding)?;              // exact before and after
bindings.apply(&binding, &plan, ApplyOptions::default())?;
bindings.adopt(&binding)?;                        // record an existing link, unchanged
bindings.undo("cargo-registry")?;                 // restore the prior state

let area = barge.area(
    AreaSpec::new("oxide", "pool", "Builds")
        .absent(Absent::Fallback { path })      // or Absent::Fail (the default)
        .soft_cap_bytes(Some(200 << 30))
        .counts_toward_reserve(true),
)?;
match area.location()? { Location::Volume(p) | Location::Fallback(p) => .., Location::Unavailable(why) => .. }

let root = area.create_root("slot")?;             // new <label>-<id>
let root = area.adopt_root("pool-slot", "slot", "oxide-72ea0e6b")?;
let hold = area.hold(&root.id, Mode::Exclusive)?; // flock; try_hold never waits
let donor = area.hold(&other.id, Mode::Shared)?;  // seeding: hold a donor too
hold.set_inheritable(true)?;                      // the child keeps the hold across exec
area.list_without_bytes()?;                       // roots, holds, pins and stragglers
area.list()?;                                     // same listing, with root byte totals
area.pin(&root.id, "selected output")?; area.unpin(&root.id)?;
area.evict(&root.id)?;                            // refuses if held or pinned

barge.space("Builds")?;                           // free, reserve, floor
barge.prune("Builds", false)?;                    // LRU idle unpinned roots to the reserve
barge.admit("Builds")?;                           // prune to the floor; refuse only below it
barge.sweep_trash("Builds")?;

let run = barge.scratch("Builds", "effigy")?.create("test-run", ttl)?;
barge.sweep_scratch("Builds", Some("effigy"))?;   // expired idle roots
```

`Barge::open_read_only()` and `Barge::open_read_only_with(...)` inspect account
state without creating directories, lock files, markers or activity records.
Missing state is empty. Mutating operations refuse before filesystem access;
`prune(..., true)` remains available as a preview. A missing hold lock is
reported as unheld.

The stored documents carry their own schema names (`barge.volumes/1`,
`barge.layout/1`, `barge.bindings/1`, `barge.area/1`, `barge.root/1` and `/2`,
`barge.pin/1`, `barge.activity/1`
and `/2`); activity schema 2 also records each root's bytes at hold release,
and readers continue to accept schema 1 records.
Root schema 1 keeps the derived `<label>-<id>` folder rule, while schema 2
records an adopted folder. A reader that does not know a schema refuses it
rather than guessing.

Warm reuse is the consumer's choice: a consumer that finds an idle root it
wants takes an exclusive hold on it and reuses it, contents and all. Barge
mints a new root only when asked to create one. Barge never decides
compatibility, freshness or what a root contains.

## Space policy

- A volume's **reserve** triggers pruning, it doesn't stop work. Below it,
  Barge evicts the least-recently-used idle, unpinned roots, from areas that
  count toward the reserve, until free space is back above it. The reserve is
  about one volume's free space, so only roots on that volume count toward it,
  and only bytes freed on that volume are credited to it.
- An area's **soft cap** is a budget on the area, so it counts that area's own
  bytes wherever its roots live: on the volume, or in its declared fallback on
  the internal drive. Above it, the area's own LRU idle roots are evicted,
  fallback roots included. Root sizes are recorded when holds are
  released; roots without a recorded size are measured once on the next
  soft-cap check. `status` reports that recorded total too, measuring only a root
  no release has recorded, so reading status never walks every root. The total is
  approximate between releases. A size a walk could
  not take exactly, because an entry was unreadable, a folder could not be
  enumerated or the tree is deeper than deletion reaches, is reported as
  approximate rather than as exact. Only a complete measurement is recorded, and
  an incomplete release forgets any size recorded earlier, so a lower bound is
  never later read back as an exact size.
- A size is the space an entry occupies, its allocated 512-byte blocks
  (`st_blocks * 512`), not its apparent length, so a sparse file or a
  copy-on-write cloned VM disk neither overstates a soft cap nor overstates the
  bytes an eviction freed. An eviction takes those bytes from the deletion that
  removes the trash copy, or from the size a release recorded, so no separate
  walk runs while the hold lock and the area lock are held.
- A reserve pass counts the measured bytes it evicts and stops when that reaches
  the target. APFS can report freed space late, so free space is read once more
  only at the end, for the report, instead of after every root.
- An eviction that fails for a reason the filesystem gives — a rename or a
  delete that cannot be done — fails the prune with that error. Only a policy
  refusal is reported as a kept root. A trash copy a delete left behind is
  reported with the reason it stayed when deletion skips a boundary. A deletion
  error fails the operation and names the trash path; retained trash is not
  counted as freed.
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

`barge` wraps the library. Every current verb:

- `volume register [--import-layout] | volume list | volume status <name>`;
- `area declare | area list [--volume v]`;
- `root list <namespace>/<area> [--bytes]`, and
  `root pin|unpin|evict <namespace>/<area> <id>`;
- `prune --volume v [--dry-run]`;
- `scratch new | scratch sweep`;
- `worktree lease | lease get | return | run | status` ([worktrees](worktrees.md#step-1-as-built));
- `status`;
- `layout show | layout set`;
- `path <class> [--project p]`; `env [--shell zsh|bash|fish]`;
- `survey <volume> [--no-bytes]`;
- `bind list | plan | apply | adopt | undo`;
- `init [--project p] [--path dir] [--check]`, which writes or reports the
  uncommitted `AGENTS.local.md` scratch file from the layout
  ([resources](resources.md#reaching-tools-and-agents-without-coupling));
- `move --plan | --apply [--retarget-unrecorded]`;
- `doctor`;
- `machine discover | add | list | remove | wake | pair | unpair | clients | revoke | status | sleep | restart | shutdown | cancel`
  ([machines](machines.md)).

`--json` prints one document with a top-level `schema` field
(`barge.<kind>/<n>`); consumers check it and refuse unknown versions. Every
document's fields and their shapes are the
[consumption contract](contracts/consumption.md). `env` refuses `--json`
because its output is shell code. Exit codes: 0 success, 1
refused or not found (nothing changed), 2 usage error, 3 internal error.
`doctor` exits 1 when its read-only report is not ok; `init --check` exits 1 when
the block is stale or missing.

## Platforms

macOS on APFS first. The OS layer is deliberately small: volume identity (UUID
at a mount), free space, and the descriptor-relative open, lock and rename
helpers every other module calls instead of naming a syscall itself. Markers are
portable, so Linux support is a new identity and space implementation, not a
redesign.

`barged` targets Linux: its power backend is logind over D-Bus, so sleep,
restart and shutdown exist only there. The daemon and its tests still build and
run on macOS so the workspace checks cover them; state and its HTTP protocol do
not depend on the platform.

## Testing

Unit and integration tests run on temporary directories, with a fake volume
identity. Required cases:

- a hold blocks eviction and prune; a released hold allows them; a hold or
  pin taken after an idle reading but before the rename stops the eviction;
- deletion and the trash sweep don't follow symlinks or cross into a nested
  mount, and skip trash entries whose marker doesn't match;
- a sweep releases the area lock before deleting, so a hold is taken while a
  sweep's deletion is in progress; two sweeps never delete the same tree;
- a fallback root stays listed, holdable and pinnable after the volume returns;
- a hold survives `fork` and `exec` when its descriptor is inherited;
- eviction renames to trash first; a crash after the rename leaves trash that
  the next sweep removes; a later root never reuses an evicted path;
- unmarked or foreign folders in an area are reported and never deleted;
- pins survive prune; the floor refuses only when nothing idle remains;
- a different volume at the registered mount is refused;
- a mismatched area declaration is refused with the differing fields named;
- plan, apply, adopt and undo of a link on temporary paths, including a target
  that changed after the plan, a preserved directory, an adopted link whose text
  is relative or runs through a symlinked folder, and deletion that never
  follows a symlink;
- doctor reports ok, drifted, stale, missing and unavailable, and writes nothing
  on a read-only account;
- `init` writes, replaces and leaves alone `AGENTS.local.md`, adds its exclude
  entry once (also in a linked worktree's common Git directory), never edits
  the committed `AGENTS.md`, renders while the volume is away, and reports a
  leftover block or a missing pointer; `--check` and doctor write nothing.

There are no native proof ladders or capture harnesses. Before Oxide adopts a
new Barge revision, run `barge volume status` and a `prune --dry-run` against
the real External drive by hand.

Two test affordances exist and no others. `barge::testing` is behind the
`testing` feature, which only dev-dependencies enable, so it is not in the
library a consumer links. `BARGE_TEST_VOLUME_UUID` is the one hook in the shipped
library: it replaces the real volume identity, and only for a path inside the
process's own temporary directory, so the CLI contract tests can register a
fixture folder on a platform whose identity Barge cannot read yet.

## Boundary with Oxide

Oxide keeps the Cargo front door, slots and leases, compatibility keys,
freshness, recipes and profiles, output member links, seeding logic, and
`oxide exec`. Barge supplies volumes, areas, roots, holds, pins, pruning,
trash and scratch. Barge knows nothing about Oxide: no Oxide formats, no
Oxide-specific modules, no adapters. Oxide tests that its own child-launch
path keeps the hold descriptor inherited.

## Later

Designed separately, not yet built:

- **Tool adapters** for cache and kept placements ([resources](resources.md#adapters)).
  Link and workflow bindings, `init`, `move` and `doctor` are built; the
  `setting` adapters are not.
- **Worktree pools:** areas of git worktrees, leased through durable records
  and pins; step 1 is built, see [worktrees](worktrees.md).
- **The machine daemon's later steps:** the remote-control replacement and
  Tailscale sign-ins ([machines](machines.md#build-order)). Machine control
  steps 1-3 are built.
