# Resources and bindings

Status: design approved by Tom, 2026-11-18. Steps 1 and 2 are built; later steps remain planned. The lean storage core in [architecture](architecture.md) is the mechanism
this design builds on and does not change.

## The problem

Moving data onto Stash was never one move. Each one changed something that
matters in a different place:

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

The starting set:

| Class | Durability | Contents owned by | Examples |
|---|---|---|---|
| `scratch` | expendable, with expiry | Barge | test runs, agent scratch per project |
| `worktrees` | expendable, LRU | Barge | warm git worktree pools, Paseo and Nucleus workspaces |
| `builds` | expendable, LRU | the consumer, via areas | Oxide's pool and outputs |
| `caches` | expendable, LRU | the tool | Cargo registry and git, npm, bun, pip |
| `evidence` | expendable, with expiry | the tool | Effigy, Praxis and Nucleus run evidence |
| `vms` | kept | the tool | Colima and Parallels disks |
| `archives` | kept | the operator | archives and old projects |

**Durability belongs to the class, not the volume** (Tom, 2026-11-18). Each
class is placed on one registered volume, and different classes can live on
different volumes. One drive can hold both expendable and kept classes, as
Stash does today. A second drive can take some classes over by moving them
(see [Moving a class](#moving-a-class)). Kept classes sit outside every
expendable area (Q-006), so pruning can never reach them.

### Layout

The **layout** places each class on a volume and a folder under its `root_dir`
(or the mount when `root_dir` is empty). Survey lists items at that same base.
It has two copies, with one authority:

- **The account layout** (`layout.json` in account state, `barge.layout/1`) is
  the authority on this machine. It covers every volume, so Barge still knows
  where each class belongs while a drive is absent: `barge path scratch` with
  Stash unplugged applies that class's absent policy rather than guessing
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
    "scratch":   { "volume": "Stash", "path": "scratch" },
    "worktrees": { "volume": "Stash", "path": "worktrees" },
    "builds":    { "volume": "Stash", "path": "builds" },
    "caches":    { "volume": "Stash", "path": "caches" },
    "evidence":  { "volume": "Stash", "path": "evidence" },
    "vms":       { "volume": "Stash", "path": "vms" },
    "archives":  { "volume": "Stash", "path": "archives" }
  }
}
```

A consumer area (`oxide/pool`) sits inside its class folder by default
(`builds/oxide/pool`) and can be overridden to an existing path, as Oxide's two
areas are today ([architecture](architecture.md#layout-on-a-volume)), until a
move puts them under the class folder. The Stash layout lane settles the actual
target layout. The folder names above are a first proposal.

### Bindings

A **binding** records how one consumer is pointed at a class. There are five
kinds:

| Kind | How the consumer learns the location | Barge's part |
|---|---|---|
| `linked` | links the library and asks it | none: always current |
| `queried` | runs `barge path <class>` or `eval "$(barge env)"` | none: always current |
| `setting` | its own configuration file | an adapter writes, reads back and undoes the value |
| `link` | a symlink at the tool's own path | Barge owns the link, records it, and can undo it |
| `workflow` | a managed block in a repository's `AGENTS.md` | `barge init` writes the block from the layout |

`linked` and `queried` are preferred: they can't drift. The other three exist
because most tools can't be changed to ask Barge.

Bindings are per machine, stored in account state (`bindings.json`,
`barge.bindings/1`). Each record holds the consumer, class, kind, target (file
and key, link path, or repository), the value Barge applied, when it applied
it, and what was there before, so undo restores it exactly.

Only the `link` kind is built. Its rules:

- `plan` shows the exact before and after, with the destination already resolved
  as the link will hold it; `apply` takes that plan document back, so the entry
  it observed (including which real directory or file) and the resolved
  destination it reviewed are what it enforces, and it refuses a target or a
  destination that changed since;
- `adopt` records a symlink that already points into a class folder without
  changing it;
- `undo` restores the prior state exactly: nothing, an older link, or a real
  entry Barge preserved. A failed undo leaves the applied link and its record in
  place;
- one target belongs to one consumer: a second consumer is refused, even through
  an equivalent path;
- a real directory or file is only replaced when the plan shows it and the
  caller passes the explicit replace. It is renamed beside the link
  (`<name>.barge-prior`) and undo renames it back; moving data is not part of
  this kind;
- a destination must be an existing folder inside the class folder, and Barge
  never creates it;
- deleting a link removes the link itself and never follows it.

One record names one consumer, so a tool with two links records two consumers
(`cargo-registry`, `cargo-git`).

**Cache and kept placements** are `link` or `setting` bindings for tool-owned
data:

- A **cache placement** moves a tool's cache onto the class folder and points
  the tool there, for example `~/.cargo/registry → <Stash>/caches/cargo/registry`.
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
- **Interactive agents get a generated workflow block.** `barge init` writes a
  managed block into a repository's `AGENTS.md`, the way `oxide init` does: the
  scratch folder for that project, and never to write to a home folder or a
  drive root. `barge init --check` reports a stale block. Today these rules are
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
- each workflow block: current or stale.

`--json` uses the usual schema contract. Exit codes: 0 when everything is ok,
1 when anything drifted. Doctor changes nothing.

As built, `0` requires every volume to be present with an agreeing layout, every
class folder to exist and every binding to be `ok`; an absent or mismatched
volume, a disagreeing copy, a missing class folder, and a `drifted`, `missing`
or `unavailable` binding all exit 1. Doctor writes nothing and works on a
read-only account.

## Moving a class

Changing where a class lives is the operation the Stash tidy needs, so it is
one command with a plan:

```
barge move <class|area> <new-path> --plan    # writes nothing
barge move <class|area> <new-path> --apply   # after operator approval
```

The plan lists:
- the data to move. A move within one APFS volume is a rename: instant,
  atomic, and with no copy.
- every binding that points at the old path, with its exact change;
- the workflow blocks to regenerate;
- the consumers that must be stopped first.

Barge doesn't scan processes, so for tool-owned data the operator confirms the
tool is stopped. For Barge-owned roots, holds already prove it. Apply renames,
then updates each binding, then runs doctor. A failure leaves both the old
binding and the old data in place, or rolls the rename back. The apply step
always needs Tom's explicit approval for live data (AGENTS.md guardrail).

## Survey

`barge survey <volume>` is read-only. It lists every top-level item with its
size and classifies it: inside a declared class, a known consumer path not yet
under a class, or unknown. It's the input to the Stash layout lane's migration
plan.

## Library and CLI additions

```rust
let layout = barge.layout("Stash")?;                // read, or the default proposal
layout.path(Class::Scratch)?;                       // resolved, with the absent policy applied
barge.bindings()?.doctor()?;                        // Report
barge.bind(Binding::setting("paseo", Class::Worktrees))?.plan()?;   // Change; apply is explicit
```

CLI:
- `barge layout show|set`
- `barge path <class> [--project p]`
- `barge env [--shell zsh]`
- `barge bind list|plan|apply|undo`
- `barge doctor`
- `barge init [--check]`
- `barge survey`
- `barge move --plan|--apply`

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
3. `init` with workflow blocks; adopt this repository, then Effigy's.
4. Adapters: Paseo, Nucleus, Effigy, Colima, shell environment.
5. `move --plan|--apply`; the Stash layout lane then uses it item by item.

## Questions

- [Q-002](questions.md#q-002--which-volume-owns-which-data-when-several-are-registered):
  each class is placed on one volume; durability belongs to the class.
- [Q-014](questions.md#q-014--where-does-the-layout-manifest-live): the account
  layout is the authority; the volume record describes the drive.
- [Q-015](questions.md#q-015--how-do-agents-get-their-scratch-and-worktree-locations):
  through each tool's own setting and the standard `TMPDIR`, with no
  injection. See [Reaching tools and agents](#reaching-tools-and-agents-without-coupling).
