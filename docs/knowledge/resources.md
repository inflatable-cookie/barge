# Resources and bindings

Status: design proposal, 2026-11-18, for Tom's sign-off. Nothing here is built
yet. The lean storage core in [architecture](architecture.md) is the mechanism
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

**Durability belongs to the class, not the volume.** Q-002 assumed two drives,
Builds for expendable data and Vault for kept data. There is one, Stash, and it
holds both. Kept classes sit outside every expendable area (Q-006), so pruning
can never reach them.

### Layout manifest

A **layout** maps each class to one folder on a volume. It is stored **on the
volume**, at `<mount>/.barge/layout.json` (`barge.layout/1`), so it travels
with the drive and every machine that mounts it agrees on where things are.
Account state keeps only which volumes are registered.

```json
{
  "schema": "barge.layout/1",
  "volume": "Stash",
  "classes": {
    "scratch":   { "path": "scratch" },
    "worktrees": { "path": "worktrees" },
    "builds":    { "path": "builds" },
    "caches":    { "path": "caches" },
    "evidence":  { "path": "evidence" },
    "vms":       { "path": "vms" },
    "archives":  { "path": "archives" }
  }
}
```

A consumer area (`oxide/pool`) sits inside its class folder by default
(`builds/oxide/pool`) and can be overridden to an existing path, as Oxide's
two areas are today ([architecture](architecture.md#layout-on-a-volume)), until
a move puts them under the class folder. The Stash layout lane settles the actual target layout. The
folder names above are a first proposal.

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
- **Colima:** profile mounts (Q-009).
- **Shell environment variables for caches:** a managed block in a file Barge
  owns, which the operator sources once.

The Cargo `registry` and `git` symlinks become `link` bindings.

### Agent workflows

`barge init` writes a managed block into a repository's `AGENTS.md`, the way
`oxide init` does. The block is generated from the layout: where scratch goes,
how to get a scratch root (`barge scratch new`), and never to write to a home
folder or a drive root. `barge init --check` fails when the block is stale. A
repository adds it to its docs check, so the written workflow cannot drift from
the real layout. This replaces the hand-written Stash rules in Effigy's,
Acowtancy's and other repositories' `AGENTS.md`.

### Doctor

`barge doctor` reports, for every registered volume and recorded binding:

- the volume: present, absent, or a different volume at the mount;
- the layout: every class folder exists, and unknown top-level items are listed;
- each binding: `ok`, `drifted` (with the observed and expected values),
  `missing`, or `unavailable` (volume absent);
- each workflow block: current or stale.

`--json` uses the usual schema contract. Exit codes: 0 when everything is ok,
1 when anything drifted. Doctor changes nothing.

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

1. Classes and the layout manifest; `path` and `env`; `survey`.
2. Binding records, the `link` kind and `doctor`; adopt the Cargo links.
3. `init` with workflow blocks; adopt Effigy and this repository.
4. Adapters: Paseo, Nucleus, Colima, shell environment.
5. `move --plan|--apply`; the Stash layout lane then uses it item by item.

## Open questions

- [Q-014](questions.md#q-014--where-does-the-layout-manifest-live): layout on
  the volume (proposed) or in account state?
- [Q-015](questions.md#q-015--how-do-agents-get-their-scratch-and-worktree-locations):
  should Nucleus and Effigy inject `barge env` into the agents and runs they
  start, so agents never need to read the workflow block?
