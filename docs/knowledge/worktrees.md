# Worktree pools

Status: design draft for Tom's sign-off, 2026-11-20. Nothing here is built.

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
  --branch <name> --request <id> --json
```

- `--base` is an exact commit. A branch name can accompany it but never
  replaces it.
- `--request` is the caller's idempotency key (for Nucleus, the task ID).
  Leasing the same request twice returns the same lease; a different request
  can never get a leased root.
- Barge picks a free warm root (or makes one if the pool is below its size),
  fetches, checks out a new branch `<name>` at `<sha>`, and verifies the tree
  is clean.
- It returns the canonical path, a stable lease ID, the branch and the
  verified head.
- The lease is recorded durably before the call returns, so a caller that lost
  the reply can read it back:
  `barge worktree lease get --request <id>` or `--lease <id>`.

### Return

```
barge worktree return --lease <id> [--disposable] --request <id> --json
```

- Return hands the root back to the pool. It is refused, and the lease stays,
  while the hold is still taken (something is still running in the tree).
- Barge resets a returned tree only when it can prove nothing is lost:
  - the tree has no uncommitted or untracked changes outside the warm paths
    below; and
  - either the branch has no commits beyond its base, or the caller passes
    `--disposable`, meaning it has its own evidence the work is published,
    merged or abandoned on purpose. Barge does not infer a merge from branch
    ancestry: squash merges break that, and the caller (Nucleus) already holds
    the publication evidence.
- Anything else is refused with the reason. The tree stays exactly as it is
  and the lease stays with the caller; Barge never discards work.
- On an accepted return Barge resets the tree to the latest base (fetch,
  detach at the base, delete the task branch locally, `git clean` everything
  except the warm paths) and marks the root free.
- Return is idempotent by lease ID; a repeated return of a returned lease
  reports the earlier outcome.

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
- Everything else ignored is removed at return.

## Pools

A pool is declared once per repository in the account layout:

```json
{ "repo": "github.com/inflatable-cookie/barge",
  "warm": 2, "max": 6,
  "warm_paths": [{ "path": "apps/barge-app/node_modules", "keys": ["apps/barge-app/bun.lock"] }] }
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

- a lease keyed by the task ID, with the repository identity, the exact base
  commit and the task branch;
- a canonical path, a stable lease ID and the verified branch and head;
- durable recording before the reply, and read-back by request or lease ID for
  a reply that was lost, on both lease and return;
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

## Questions for Tom

- Pool sizes: `warm: 2, max: 6` per active repository as the default?
- Warm paths are opt-in per pool. Start with Barge's own repository as the
  first pool and widen after it works?
