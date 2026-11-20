# Worktree pools

Status: design draft for Tom's sign-off, 2026-11-20, revised after the Nucleus
planner's review. Nothing here is built.

Barge keeps a warm pool of git worktrees per repository on Stash and leases
them to agents. An agent starts in a checkout that already has its dependencies
installed, instead of cloning and installing from cold. Worktrees are Barge's
domain ([vision](vision.md#direction)); the reference design is
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
- **A lease is a hold** on that root. The hold is inherited by the agent's
  child processes, so a root is busy for exactly as long as something from the
  lease is still running. Barge never scans or kills processes to find out.
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
- Barge picks a free warm root (or makes one if the pool is below its size),
  fetches, checks out a new branch `<name>` at `<sha>`, and verifies the tree
  is clean.
- It returns the canonical path, a stable lease ID, the branch and the
  verified head.
- The lease is recorded durably before the call returns, so a caller that lost
  the reply can read it back:
  `barge worktree lease get --request <id>` or `--lease <id>`.
- The durable lease, not a process, reserves the root. It stays reserved
  between runs, when nothing holds it, until it is returned.

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

## Warm paths

What makes a pooled tree worth having is what survives between leases.

- A pool declares its **warm paths**: ignored dependency folders, such as
  `node_modules` or `apps/*/node_modules`, each with the **key files** that
  decide whether it is still valid, such as `bun.lock` or `package-lock.json`.
- At lease, after checking out the base, Barge hashes each warm path's key
  files. If the hash matches the one recorded when the folder was last valid,
  the folder stays. If not, Barge removes it before handing the tree over, and
  the agent's own install rebuilds it. Barge never runs an install itself.
- Rust build output is not a warm path: Oxide places it in its own pool, and
  pairing a warm worktree with a warm Oxide slot is a later step.
- Barge also re-hashes the key files of the outgoing checkout at return. If a
  task changed a lockfile and reinstalled, the folder is recorded under the
  new key or removed, so the next task never gets dependencies under an old
  key.

## Cleanup rules

At return, Barge removes only what it can name:

- tracked changes are already refused by the return preflight;
- warm paths stay or go by their key, as above;
- ignored paths the pool declares disposable, such as `dist` or `.cache`, are
  removed;
- any other ignored file refuses the return: it could be local data, and Barge
  doesn't delete what it can't account for. The operator adds it to the pool's
  disposable list or removes it;
- symlinks are removed as links and never followed.

## Pools

A pool is declared once per repository in the account layout:

```json
{ "repo": "github.com/inflatable-cookie/barge",
  "warm": 2, "max": 6,
  "warm_paths": [{ "path": "apps/barge-app/node_modules", "keys": ["apps/barge-app/bun.lock"] }],
  "disposable": ["apps/barge-app/dist", ".effigy"] }
```

- `warm` trees are kept ready; `max` caps the pool. Beyond `max`, a lease
  waits or fails rather than growing without bound (`--wait <seconds>`).
- `barge worktree status [--repo]` lists pools, roots, leases and their
  holders' request IDs.
- `barge worktree prune` returns the pool to `warm` through the class's
  eviction. Doctor reports a pool whose clone is missing or corrupt.

## Nucleus and Paseo

Nucleus creates and owns task worktrees today, under its `workRoot`
(`/Volumes/Stash/queue/workspaces/<hash>/ns-<task>`); Paseo and T3 only
register the directory Nucleus made. The Nucleus planner confirmed
(2026-11-20) there is no seam for an outside provider: a Paseo plugin runs too
late. Taking workspaces from Barge needs a host-independent change to Nucleus's
workspace allocator and retirement. That is Nucleus's change, briefed and owned
by the Nucleus planner, built after Barge's CLI exists.

What Nucleus needs from Barge, and this contract gives it:

- a lease keyed by allocation, with the task ID as a label, bound to the
  repository identity, the exact base commit and the task branch;
- a canonical path, a stable lease ID and the verified branch and head;
- durable recording before the reply, and read-back by request or lease ID for
  a reply that was lost, on both lease and return, with the lease's state
  (`leased`, `returning`, `returned`, `interrupted`);
- a return fenced to its lease ID and against new holds, whose replay never
  touches a root that has been leased again;
- exactly one lease per task, kept through revisions, review, restart and
  unsettled effects. Nucleus holds the lease for the task's whole life and
  returns it only at closeout;
- a return that refuses dirty or unproven work and leaves it with Nucleus,
  with `--disposable` carrying Nucleus's own publication evidence.

Nucleus still proves a clean tree at admission and an idle, tool-free tree at
closeout, and removes only its own registration.

## Out of scope

Process scanning or killing on return, journals, a daemon (the CLI is the
whole surface; holds do the coordination), and running installs.

## Build order

1. Git module, pools, `lease`, `return`, `lease get`, `status`, with tests
   against real temporary repositories.
2. Warm paths with key-file validation, and `prune`.
3. Nucleus integration: the Nucleus planner briefs its allocator change
   against this contract.
4. Later: pairing a leased worktree with a warm Oxide slot.

## Rulings

Tom, 2026-11-20, approving this design:

- Pool sizes default to `warm: 2, max: 6` per active repository.
- Barge's own repository is the first pool; widen to other repositories after
  it works.
