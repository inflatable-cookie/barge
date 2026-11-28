# Resources and bindings

Status: design approved by Tom, 2026-11-18. Steps 1, 2, 3 and 5 are built;
step 4 remains planned. The lean storage core in
[architecture](architecture.md) is the mechanism this design builds on and does
not change.

## Volume registration

On Linux, register a filesystem root or a btrfs subvolume mounted at its own
mount point, with a canonical UUID available through `/dev/disk/by-uuid`.
Btrfs subvolumes use the filesystem UUID and subvolume ID as their identity;
the top-level subvolume uses the plain UUID. Subvolumes share the filesystem's
free space. Bazzite's `/var/home` can be registered directly. Barge refuses
subfolder bind mounts and mounts whose identity cannot be resolved. Filesystems
with noncanonical UUIDs, such as short FAT IDs, cannot currently be registered.

## The problem

Moving data to a registered external volume was never one move. Each placement
changed something that matters in a different place:

- Paseo's worktree root in `~/.paseo/config.json`;
- Nucleus's `workRoot` in `~/.nucleus/settings.json`;
- symlinks at `~/.cargo/registry` and `~/.cargo/git`;
- Oxide's pool and outputs;
- Effigy's inherited scratch;
- hand-written scratch rules in several repositories' `AGENTS.md`;
- Colima's profile mounts (Q-009).

Nothing records these links, nothing checks them, and each new tool adds
another. Barge's job is to make this one small policy surface: declare where
each kind of data lives, record how every consumer is pointed at it, and report
when one drifts.

## Model

### Resource classes

A **class** is one kind of heavy developer data. It declares:

- **durability:** `expendable` (Barge may prune it) or `kept` (Barge never
  deletes it);
- **lifecycle:** expiry and pruning for expendable classes; none for kept ones;
- **owner:** whether Barge owns its roots (scratch, worktree pools, consumer
  areas) or another tool owns the contents and Barge only places them (caches,
  VM disks);
- **absent policy:** `Fail` or `Fallback(path)`, as for areas today.

The six resource classes:

| Class | Durability | Contents owned by | Examples |
|---|---|---|---|
| `scratch` | expendable, with expiry | Barge | test runs, agent scratch per project |
| `worktrees` | expendable, LRU | Barge | warm git worktree pools, Paseo and Nucleus workspaces |
| `builds` | expendable, LRU | the consumer, via areas | Oxide's pool and outputs |
| `caches` | expendable, LRU | the tool | Cargo registry and git, npm, bun, pip |
| `vms` | kept | the tool | Colima and Parallels disks |
| `archives` | kept | the operator | archives and old projects |

**Run evidence is scratch** (Tom, 2026-11-18). A tool's run evidence is placed
under the `scratch` class, `scratch/<tool>/…`, with that tool's own expiry;
there is no separate class for it. A stored layout that still names `evidence`
is refused, naming the retired class, rather than misread.

**Durability belongs to the class, not the volume** (Tom, 2026-11-18). The
volume's availability says whether it can currently be used; its stored
`durability` field is descriptive metadata and does not set class policy. A
volume can hold classes of either durability. Each class is placed on one
registered volume, and different classes can live on different volumes. One
drive can hold both expendable and kept classes. A second drive can take some
classes over by moving them (see [Moving a class](#moving-a-class)).
Kept classes sit outside every expendable area (Q-006), so pruning can never
reach them.

### Layout

The **layout** places each class on a volume and a folder under its `root_dir`
(or the mount when `root_dir` is empty). Survey lists items at that same base.
It has two copies, with one authority:

- **The account layout** (`layout.json` in account state, `barge.layout/1`) is
  the authority on this machine. It covers every volume, so Barge still knows
  where each class belongs while a drive is absent: `barge path scratch` with
  its registered volume unplugged applies that class's absent policy rather than
  guessing
  (Tom, 2026-11-18).
- **The volume record** (`<mount>/.barge/layout.json`) lists the classes that
  volume holds and their folders. Barge rewrites it whenever the account layout
  changes. It lets a drive describe itself: another machine, or a fresh
  account, can import it on registration instead of rediscovering the folders.
  `barge doctor` reports any disagreement between the two copies, and the
  operator picks which one wins. Barge never resolves it silently.

```json
{
  "schema": "barge.layout/1",
  "classes": {
    "scratch":   { "volume": "<volume>", "path": "scratch" },
    "worktrees": { "volume": "<volume>", "path": "worktrees" },
    "builds":    { "volume": "<volume>", "path": "builds" },
    "caches":    { "volume": "<volume>", "path": "caches" },
    "vms":       { "volume": "<volume>", "path": "vms" },
    "archives":  { "volume": "<volume>", "path": "archives" }
  }
}
```

A consumer area (`oxide/pool`) sits inside its class folder by default
(`builds/oxide/pool`). Oxide's pool and outputs use `builds/oxide/pool` and
`builds/oxide/outputs` within the registered external volume
([architecture](architecture.md#layout-on-a-volume)). The class folder names
above describe the conventional layout; the account layout names the registered
volume for each class.

### Bindings

A **binding** records how one consumer is pointed at a class. There are five
kinds:

| Kind | How the consumer learns the location | Barge's part |
|---|---|---|
| `linked` | links the library and asks it | none: always current |
| `queried` | runs `barge path <class>` or `eval "$(barge env)"` | none: always current |
| `setting` | its own configuration file | an adapter writes, reads back and undoes the value |
| `link` | a symlink at the tool's own path | Barge owns the link, records it, and can undo it |
| `workflow` | an uncommitted `AGENTS.local.md` in a repository | `barge init` writes the file from the layout and keeps it out of Git |

`linked` and `queried` are preferred: they can't drift. The other three exist
because most tools can't be changed to ask Barge.

Bindings are per machine, stored in account state (`bindings.json`,
`barge.bindings/1`). Each record holds the consumer, class, kind, target (file
and key, link path, or repository), the value Barge applied, when it applied
it, and what was there before, so link undo restores it exactly.

The `link` kind's rules:

- `plan` shows the exact before and after, with the destination already resolved
  as the link will hold it; `apply` takes that plan document back, so the entry
  it observed (including which real directory or file) and the resolved
  destination it reviewed are what it enforces, and it refuses a target or a
  destination that changed since;
- `adopt` records a symlink that already points into a class folder without
  changing it;
- `undo` restores the prior state exactly: nothing, an older link, or a real
  entry Barge preserved. It recognises the link by what it resolves to, as
  doctor does, so a link `adopt` found written with a relative path or through a
  symlinked folder is still the link it removes. A failed undo leaves the
  applied link and its record in place;
- one target belongs to one consumer: a second consumer is refused, even through
  an equivalent path;
- a real directory or file is only replaced when the plan shows it and the
  caller passes the explicit replace. It is renamed beside the link
  (`<name>.barge-prior`) and undo renames it back; moving data is not part of
  this kind;
- a destination is an existing folder or regular file inside the class folder,
  and Barge never creates it. A VM disk is a regular file, so both are
  destinations; the value is resolved first, so a link that leaves the class
  folder is refused rather than followed. See the [VM disk startup warning](machines.md#vm-disks).
- a destination is recorded with its device and inode, so doctor reports one
  that was replaced at the same path, and an apply refuses a destination that
  changed since its plan;
- deleting a link removes the link itself and never follows it.

One record names one consumer, so a tool with two links records two consumers
(`cargo-registry`, `cargo-git`).

The `workflow` kind is built too. Its rules (Tom, 2026-11-21: the scratch
block names paths on one machine, so it is written locally and never committed,
since it would not work for anyone else checking out the code):

- a repository's scratch file is `AGENTS.local.md`, rendered from the account
  layout: the project's scratch folder under the scratch class, how to get a
  fresh scratch root with `barge scratch new`, and the rule never to write
  scratch to a home folder or a drive root. Worktrees are not mentioned until
  that class has a binding. `init` owns the whole file, with no markers, so a
  stale copy is replaced by atomic rename;
- `init` adds `/AGENTS.local.md` to the repository's `info/exclude` once. The
  Git directory comes from `git rev-parse --git-common-dir`, so a linked
  worktree's entry lands in the main repository. Barge never touches
  `.gitignore` and never commits. A folder without `.git` is written and
  reported as not excluded;
- the committed `AGENTS.md` is never written. It needs one line that names
  `AGENTS.local.md`; when it is missing, `init` and `--check` print that line.
  A `barge:workflow` block left there by the earlier design is reported, and the
  operator deletes it by hand. Neither removes it;
- the project defaults to the repository folder's name and can be overridden
  with `--project`. `init` records a `workflow` binding whose consumer is the
  repository path and whose file is `AGENTS.local.md`. A record that names
  `AGENTS.md` predates this and doctor reports it stale, with the fix (rerun
  `barge init`); doctor never rewrites it;
- `barge init --check` exits 0 when the file is current and excluded, and 1
  when it is stale or missing, or not excluded from a repository. A folder that
  is not a repository is a warning, not a failure. The missing pointer and a
  leftover block are warnings in both commands. Check prints a diff, writes
  nothing and works on a read-only account;
- the file names a location, not a mounted path: it renders from the layout
  while the scratch class's volume is away, and the output says the volume is
  absent. `bind undo <repository>` removes the workflow record and leaves the
  file untouched; there is no earlier copy to restore.

**Cache and kept placements** are `link` or `setting` bindings for tool-owned
data:

- A **cache placement** moves a tool's cache onto the class folder and points
  the tool there, for example `~/.cargo/registry → <volume>/caches/cargo/registry`.
  Its contents are expendable: Barge may prune whole cache roots, and the tool
  rebuilds them.
- A **kept placement** does the same for data that must never be pruned, such
  as VM disks. When the volume is absent the data is unavailable. Barge never
  recreates the folder empty and never falls back to the internal drive, so a
  tool can't start writing a fresh empty copy in the wrong place.

### Adapters

An adapter is a small module for one tool's configuration:

```rust
trait Adapter {
    fn observe(&self) -> Result<Observed>;                  // current value, or the shape it can't read
    fn plan(&self, desired: &Path) -> Result<Change>;       // exact before and after
    fn apply(&self, change: &Change) -> Result<Applied>;    // only on an explicit adopt (Q-003)
    fn undo(&self, applied: &Applied) -> Result<()>;
}
```

Rules:
- An adapter refuses a configuration file whose shape it doesn't recognise,
  rather than guessing.
- It changes one key and preserves everything else in the file.
- Its tests run on temporary copies, never on the real file.

First adapters, from today's real bindings:
- **Paseo:** worktree root.
- **Nucleus:** `workRoot`.
- **Effigy:** scratch root.
- **Colima:** profile mounts (Q-009).
- **Shell environment variables for caches:** a managed block in a file Barge
  owns, which the operator sources once.

The Cargo `registry` and `git` symlinks become `link` bindings.

### Reaching tools and agents without coupling

Tom (2026-11-18): don't complicate Nucleus or Effigy, or tie them to Barge.
The rule that keeps this clean: **dependencies point from Barge to the tools,
never back.** No tool needs Barge installed, linked or running.

- **A tool keeps the one location setting it already has.** Nucleus has
  `workRoot`, Paseo its worktree root, Effigy its scratch root in
  `~/.effigy/config.toml`. Barge sets those values through a `setting` binding,
  and doctor checks them. The tool reads its own config as it does today and
  never learns that Barge exists.
- **A process gets its location from whatever started it.** Nucleus already
  gives each run its own `TMPDIR` under `workRoot`, and Effigy gives each
  selector its own scratch. Those values already come from the settings above,
  so Barge's layout reaches every run with no injection and no code change in
  either tool. `TMPDIR` is the standard contract every program already honours.
- **Interactive agents get a generated scratch file.** `barge init` writes
  `AGENTS.local.md` into a repository, uncommitted and excluded from Git, and
  the committed `AGENTS.md` names it. The file gives the scratch folder for
  that project and the rule never to write to a home folder or a drive root.
  `barge init --check` reports a stale file. Today these rules are
  hand-written, and Effigy's already carries notes about a path that moved. A
  repository may run the check in its docs check, but nothing requires it.
- **Voluntary use stays possible.** A tool that wants a fresh scratch root can
  call `barge scratch new` when Barge is installed, falling back to its own
  setting when it isn't. Nothing depends on it.

Rejected:
- Nucleus or Effigy injecting `barge env`: it couples them to Barge's
  installation and output format.
- A new shared locations file that every tool reads: a new cross-tool format
  to version and maintain.

### Doctor

`barge doctor` reports, for every registered volume and recorded binding:

- the volume: present, absent, or a different volume at the mount;
- the layout: every class folder exists, and unknown top-level items are listed;
- each binding: `ok`, `drifted` (with the observed and expected values),
  `missing`, or `unavailable` (volume absent);
- each scratch file (`workflow` binding): current or stale.

`--json` uses the usual schema contract. Exit codes: 0 when everything is ok,
1 when anything drifted. Doctor changes nothing.

As built, `0` requires every volume to be present with an agreeing layout, every
class folder to exist and every binding to be `ok`; an absent or mismatched
volume, a disagreeing copy, a missing class folder, and a `drifted`, `stale`,
`missing` or `unavailable` binding all exit 1. A workflow binding is `ok` when
its file matches what the layout renders, `stale` when it no longer does (or it
records the committed `AGENTS.md`), and `missing` when the file is gone. A stale file is
reported even while the volume is away, because the file names a location
rather than a mounted path. Doctor writes nothing and works on a read-only
account.

## Moving a class

Changing where a class lives is the operation a volume-layout tidy needs, so it
is one command with a plan:

```
barge move <class> <new-path> --plan > plan.json
barge move --item <path> --into <class>/<sub> --plan > plan.json
barge move --apply --plan plan.json --confirm-stopped \
  [--retarget-unrecorded]
```

The reviewed JSON plan records volume and filesystem identities and lists:
- the data to move. A move within one APFS volume is a rename: instant,
  atomic, and with no copy.
- every binding that points at the old path, with its exact change;
- the scratch files to regenerate;
- the consumers that must be stopped first;
- area declarations carried by a moved folder, whose consumers must update their declarations.

Barge doesn't scan processes, so for tool-owned data the operator confirms the
tool is stopped. For Barge-owned roots, holds already prove it. Apply renames,
then updates each binding and any moved area declaration and layout, then runs doctor. An item inside an area cannot be moved out on its own because that would orphan its root. A failure leaves both the old
binding and the old data in place, or rolls the rename back. The apply step
always needs Tom's explicit approval for live data (AGENTS.md guardrail).

A tool that must re-register moved data (Parallels, for a `.pvm` bundle) gets a
manual step from a fixed data list, not from code in the move: a row in
`crates/barge/src/tool_steps.rs`, rendered into the plan's manual steps as the
known-link list is.

The plan also checks the [known link locations](#known-link-locations). A link
there that points inside the moved source and that no binding records is listed
with the exact `barge bind adopt` command that records it afterwards. While the
plan lists any, apply refuses unless the caller passes `--retarget-unrecorded`,
which retargets those links in the same transaction and rolls them back with
everything else. Before retargeting a link, Barge checks that it is still a symlink to the expected target; a writer that changes the path between that check and the rename is outside the cooperative guarantee. Nothing is scanned beyond that fixed list.

A VM's disk link must be retargeted before the VM starts. Lima recreates a
missing disk, so a VM started with a dangling link boots a blank machine instead
of failing; the operator sees a working VM with no data rather than an error.

## Known link locations

`barge move --plan` checks exactly these paths, and never the home folder at
large:

| Location | Path | Consumer an adoption uses |
|---|---|---|
| `colima-disk` | `~/.colima/_lima/*/disk` | `colima-<profile>-disk` |
| `cargo-registry` | `~/.cargo/registry` | `cargo-registry` |
| `cargo-git` | `~/.cargo/git` | `cargo-git` |

The list is data (`crates/barge/src/known_links.rs`): adding a place is adding a
row, not an adapter. A link a tool keeps anywhere else is still the operator's to
find, and the plan's manual steps say so.

Every folder on the way below the home folder must be a real folder: a symlink at
`.colima`, `_lima`, a profile folder or `.cargo` is not followed, so a documented
location can never be reached through a link and apply can only retarget the one
link the pattern ends at. The home folder itself is resolved once, because where
the account's home lives is a platform fact. Apply re-plans first, so a location
that stopped being real between plan and apply is refused as a changed plan.

## Survey

`barge survey <volume>` is read-only and classifies every top-level item:
inside a declared class, a known consumer path not yet under a class, system
metadata, or unknown. By default it does not measure item sizes, so it gives a
quick layout view on a large drive. Pass `--bytes` to measure each non-system
item; it walks all of them and can take minutes on a large drive. Unreadable or
incomplete trees have approximate sizes when measured. `--no-bytes` remains
accepted as a deprecated no-op. The survey is the input to the volume-layout
migration plan.

## Library and CLI

The library example and the CLI's verb list have one owner:
[architecture](architecture.md#library-surface) and
[architecture](architecture.md#cli). This design adds no verb of its own, and
its sections above say what each built step does.

## What stays out

- No daemon for storage, and no watcher that re-applies bindings. Doctor
  reports; the operator applies.
- No process scanning. A stopped consumer is either proved by holds or
  confirmed by the operator.
- No adapter writes a setting without an explicit adopt for that tool.
- No knowledge of a consumer's contents. Barge knows where a tool's cache
  lives, not what is in it.

## Build order

Each step is its own brief, reviewed as usual.

1. **Built:** classes and the layout manifest; `path` and `env`; `survey`.
2. **Built:** binding records, the `link` kind and `doctor`. Adopting the real
   Cargo links is the operator's step after merge.
3. **Built:** `init` with the local scratch file. Adopting this repository, then
   Effigy's, is the operator's step after merge.
4. Adapters: Paseo, Nucleus, Effigy, Colima, shell environment.
5. **Built:** `move --plan|--apply`; the volume-layout migration then uses it item by item.

## Questions

- [Q-002](questions.md#q-002--which-volume-owns-which-data-when-several-are-registered):
  each class is placed on one volume; durability belongs to the class.
- [Q-014](questions.md#q-014--where-does-the-layout-manifest-live): the account
  layout is the authority; the volume record describes the drive.
- [Q-015](questions.md#q-015--how-do-agents-get-their-scratch-and-worktree-locations):
  through each tool's own setting and the standard `TMPDIR`, with no
  injection. See [Reaching tools and agents](#reaching-tools-and-agents-without-coupling).
