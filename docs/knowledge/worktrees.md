# Worktree pools

Barge supports lease, return, run, status and prune, with key-file validation
for warm dependency folders. Tom approved this design on 2026-11-20.

Barge keeps a warm pool of git worktrees per repository on a registered
external volume and leases them to agents. An agent starts in a checkout that
already has its dependencies installed, instead of cloning and installing from
cold. Worktrees are Barge's domain ([vision](vision.md#direction)); the
reference design is
[Treehouse](https://github.com/kunchenguid/treehouse).

## Model

It reuses the storage core in [architecture](architecture.md); the only new
mechanic is a small git module.

- **A pool is an area** in the `worktrees` class
  ([resources](resources.md#resource-classes)), one per repository, named by
  the repository's canonical identity: its normalised origin URL.
- **A worktree is a root** in that area: a `git worktree add` from one shared
  bare clone of the repository, kept in the pool. Barge owns the clone and
  every worktree.
- **A lease reserves a root** until it is returned. A durable record keeps
  the root assigned between runs, and a pin protects it from eviction.
  A consumer holds the root while it uses it: a Rust caller through the
  library's hold, and any other process through
  [`barge worktree run`](#run). Return requires an exclusive hold before it
  can reset the checkout.
- **Eviction** is the class's LRU rule through `evict` and trash, as for any
  expendable area. A leased root is never evicted.

## Lease and return

The contract is the same for a person at the CLI and for Nucleus. Every call
has `--json` output under the CLI's schema contract.

### Lease

```
barge worktree lease --repo <origin> --base <sha> [--base-name main]
  --branch <name> --request <id> [--label <task id>] --json
```

- `--base` is an exact commit. A branch name can accompany it but never
  replaces it.
- `--request` identifies one allocation, not one task: Nucleus may replace a
  lost checkout or move a task to another host while the task stays open, and
  each allocation gets its own request ID. `--label` carries the task ID as
  metadata only.
- The request is bound to its repository, base and branch. Repeating it with
  the same payload returns the same lease; repeating it with a changed payload
  is refused. A branch name that already exists in the pool's clone is
  refused, not reused. A different request can never get a leased root.
- Barge prefers a free root with a nonempty warm-path record, most recently used
  first, since its dependency folders are likeliest to match the current lockfiles.
  With no recorded root it takes the least recently used free root (or makes
  one if the pool is below its size), then fetches, checks out a new branch
  `<name>` at `<sha>`, and verifies the tree is clean. Eviction and `prune`
  still trim the oldest idle roots first.
- It returns the canonical path, a stable lease ID, the branch and the
  verified head. The lease reports any warm paths it kept or removed as
  `warm_kept` and `warm_removed`. These fields are omitted when empty and
  are available when reading the lease back.
- The lease is recorded durably before the call returns, so a caller that lost
  the reply can read it back:
  `barge worktree lease get --request <id>` or `--lease <id>`.
- The durable lease, not a process, reserves the root. It stays reserved
  between runs, when nothing holds it, until it is returned.
- Nucleus keeps one live lease per allocation through revisions, review and
  restarts, and returns it only at closeout.

### Return

```
barge worktree return --lease <id> [--disposable] --request <id> --json
```

- Return hands the root back to the pool. The caller first clears its own
  side (Nucleus releases its registration and proves participants, tools and
  effects are clear). Barge then fences the return to that lease ID: it is
  refused, and the lease stays, while any hold is taken, and no new hold can
  start on the root once the return begins.
- Barge resets a returned tree only when it can prove nothing is lost:
  - the tree has no uncommitted or untracked changes outside the warm paths
    below; and
  - either the branch has no commits beyond its base, or the caller passes
    `--disposable`, meaning it has its own evidence the work is published,
    merged or abandoned on purpose. Barge does not infer a merge from branch
    ancestry: squash merges break that, and the caller (Nucleus) already holds
    the publication evidence.
- Anything else is refused with the reason, before anything is changed. Only
  this preflight refusal promises the tree is untouched; the lease stays with
  the caller and Barge never discards work.
- On an accepted return the lease moves to `returning`, and the root stays
  unavailable through reset and verification. Barge resets the tree to the
  latest base (fetch, detach at the base, delete the task branch locally,
  remove the cleanup set below), verifies it, then marks the lease `returned`
  and the root free.
- A return cut off part-way leaves the lease `interrupted` and the root out of
  the pool; the next return or `prune` finishes it. `lease get` reports each
  of `leased`, `returning`, `returned` and `interrupted`.
- Return is idempotent by lease ID. Replaying an old return after the root has
  been leased again reports the old result and never touches the root.

## Run

```
barge worktree run --lease <id> [--exclusive] -- <command> [args...]
```

The seam for a process that is not Rust: Nucleus, a shell, a T3 or Paseo
host. It takes a shared hold on the lease's root (exclusive with
`--exclusive`), keeps the descriptor inheritable and replaces itself with the
command, run in the checkout. The hold lives as long as the command and any
child that keeps the descriptor. Barge stays out of the process tree.

- Refused before the command starts when the lease is not `leased` (unknown,
  `returning`, `returned` or `interrupted`), or when a return holds the root.
  A run never waits for a return: the error says so, and nothing runs. Before
  it refuses, the attempt retries for up to 200 ms, because a child that another
  thread of the same process is spawning keeps a copy of the lock descriptor
  until it execs. A real holder still refuses.
- A return started while the command runs is refused until the hold is gone,
  so a return that needs the root retries after the run exits.
- Errors before the command starts use the CLI's error contract under
  `--json`. After it starts, the exit status is the command's own.

On Unix, a Rust service can call `Barge::worktree_hold(lease_id, mode)?`
and pass the resulting hold to `WorktreeHold::spawn(command)`. The service
keeps running, and the child starts in the checkout unless its `Command`
specifies a working directory. Only the spawned child receives the hold
across exec. The parent gives up its copy without recording a release time or
size. Return stays blocked while a child or descendant keeps the descriptor
open. A process that closes inherited descriptors leaves that guarantee.

For Tokio, use `WorktreeHold::spawn_with(command, |command|
tokio::process::Command::from(command).spawn())` and await the returned child's
`wait`. The launcher spawns one child and drops the configured command before
returning. `WorktreeHold::exec` is the library form that replaces the caller.

## Warm paths

What makes a pooled tree worth having is what survives between leases.

- A pool declares its **warm paths**: ignored dependency folders, such as
  `node_modules` or `apps/*/node_modules`, each with the **key files** that
  decide whether it is still valid, such as `bun.lock` or `package-lock.json`.
- After return preflight passes, Barge stores a SHA-256 hash for each declared
  warm path that exists as a real directory in the outgoing checkout. Missing
  paths and symlinks are omitted, dropping any older hash for that path. For a
  recorded path, missing key files contribute to the hash and differ from empty
  files. The snapshot is saved before reset, so it reflects a task's committed
  lockfile changes. A snapshot with no entries counts as cold for lease choice.
- At lease, Barge checks out the requested base and compares fresh hashes
  with the snapshot. Ignored entries with matching hashes stay. Entries with
  changed or missing hashes are removed before handover, including on roots
  created before hashes were stored. Barge never runs an install.
- Rust build output lives in Oxide's own pool. Pairing a warm worktree with
  a warm Oxide slot is a later step.

## Cleanup rules

At return, Barge removes only what it can name:

- tracked changes are already refused by the return preflight;
- warm paths stay on return and are validated at the next lease;
- ignored paths the pool declares disposable, such as `dist` or `.cache`, are
  removed;
- any other ignored file refuses the return: it could be local data, and Barge
  doesn't delete what it can't account for. The operator adds it to the pool's
  disposable list or removes it;
- symlinks are removed as links and never followed.

## Pools

A pool is declared once per repository in `worktree-pools.json`, in the account
state directory:

```json
{ "schema": "barge.worktree-pools/1",
  "pools": [{ "repo": "github.com/inflatable-cookie/barge",
              "origin": "https://github.com/inflatable-cookie/barge.git",
              "warm": 2, "max": 6,
              "warm_paths": [{ "path": "apps/barge-app/node_modules", "keys": ["apps/barge-app/bun.lock"] }],
              "disposable": ["apps/barge-app/dist", ".effigy"] }] }
```

- `warm` trees are kept ready; `max` caps the pool. Beyond `max`, a lease
  waits or fails rather than growing without bound (`--wait <seconds>`).
- `barge worktree status [--repo]` lists pools, roots, leases and their
  holders' request IDs.
- `barge worktree prune [--repo <origin>] [--dry-run] [--json]` finishes
  accepted, interrupted returns when their roots are unheld. It prepares
  detached checkouts at the origin's default commit up to `warm` available
  roots, subject to the total `max`, or evicts the oldest excess free roots
  through the area's trash. Active leases, live holds and pins protect roots
  from eviction. A dry run reports the counts and IDs without making changes
  or contacting the origin.
- Doctor reports declared pools whose bare clones are missing or corrupt.

## Nucleus and Paseo

Barge's Nucleus integration targets the Rust Nucleus engine (Spec 070). The old
engine no longer takes new Barge work. The pool integration is in progress in
the Rust engine's worktree crate; the Barge planner reviewed and approved its
design on 2026-11-24. The pool setting stays off until the engine pins Barge and
has the quiescence census in place. Paseo and T3 register the checkout provided
by Nucleus. Leasing from Barge remains an operator-enabled option, so Nucleus
can continue using its own worktrees without Barge. The return hold ruling is
recorded in [Rulings](#rulings).

Barge's return preflight refuses dirty or unproven trees and leaves them with
Nucleus. `barge worktree run` can keep a checkout held while a host command and
its children run.

## Out of scope

Process scanning or killing on return, journals, a daemon (the CLI is the
whole surface; holds do the coordination), and running installs.

## Build order

1. Built: git module, pools, `lease`, `return`, `lease get`, `status` and `run`,
   with tests against real temporary repositories. See
   [on disk and CLI](#on-disk-and-cli).
2. Built: warm paths with key-file validation, and `prune`.
3. **In progress:** Nucleus pool integration in the Rust engine; see
   [Nucleus and Paseo](#nucleus-and-paseo) for its current state.
4. Later: pairing a leased worktree with a warm Oxide slot.

## On disk and CLI

The code is `crates/barge/src/worktree/`, the CLI is `barge worktree`, and the
tests are `crates/barge/tests/worktrees.rs` and
`crates/barge-cli/tests/worktree_run.rs`, which build their own origin
repositories in temporary folders.

On disk, in the account state directory:

- `worktree-pools.json` (`barge.worktree-pools/1`) declares the pools. Barge
  reads it and never writes it. The operator writes the declaration.
- `worktree-leases/<lease-id>.json` (`barge.worktree-lease/1`) is one durable
  record per lease, published before the call replies.

On the volume, the `worktrees` class placement gives the volume and the path.
Each pool is the area `worktrees/<slug>` at `<path>/<slug>`, with its bare clone
at `<area>/.barge/clone.git`. Each root is `wt-<id>/`, and its checkout is
`wt-<id>/checkout`. The root's `.barge-warm-paths.json` file
(`barge.worktree-warm-paths/1`) stores the outgoing key hashes outside the
checkout, so Git resets leave the snapshot intact.

```
barge worktree lease --repo <origin> --base <sha> --branch <name> --request <id>
  [--base-name <name>] [--label <task>] [--wait <seconds>] [--json]
barge worktree lease get (--request <id> | --lease <id>) [--json]
barge worktree return --lease <id> [--request <id>] [--disposable] [--json]
barge worktree run --lease <id> [--exclusive] -- <command> [args...]
barge worktree status [--repo <origin>] [--json]
barge worktree prune [--repo <origin>] [--dry-run] [--json]
```

Implementation details and differences from the approved design:

- **Lease reservations use a record and a pin.** The allocation must survive
  the CLI call that created it. Holds live as long as a process keeps their
  file descriptors open.
- **The pool declares its origin.** `repo` is the canonical identity, and
  `origin` is the URL the clone fetches from. A local path or an scp-style URL
  cannot be fetched from its identity alone.
- **`--request` on return is optional.** When it is given, it must match the
  lease's request.
- **Pool roots do not count toward the volume reserve**, so reserve pruning does
  not evict them. An operator can still evict a free root by hand. The next lease
  clears the stale git metadata for it.
- **An interrupted return is finished without a second preflight.** The first
  preflight accepted the work. The root stays reserved until return or prune
  finishes the reset.

## Rulings

The Nucleus planner and Barge planner ruled on 2026-11-24 that an inherited
hold is optional, and Nucleus proves quiescence before `worktree return` with a
synchronized T3 thread snapshot plus a working-directory and open-file census
of the checkout.

Tom, 2026-11-20, approving this design:

- Pool sizes default to `warm: 2, max: 6` per active repository.
- Barge's own repository is the first pool; widen to other repositories after
  it works.
