# Questions

Questions that block or shape work. Reference them by ID from the plan and from
briefs. An answered question keeps only its pointer to where the answer lives.

## Q-001 — What does each placement kind do when its volume is absent?

Status: open
Candidates: wait (with a visible message), fail with a clear message, or use an
explicit temporary internal location that is reported and cleaned. Caches,
scratch, build output and VMs probably differ. Oxide's current build-pool rule
falls back to the internal pool, with a loud warning, only when the drive is
absent; low space never does.

## Q-002 — Which volume owns which data when several are registered?

Status: open
Builds (permanent, expendable, 1 TB) and Vault (ephemeral, kept, 4 TB) are both attached today.
Placement-to-volume binding, preference order and moving a placement between
volumes are undecided.

## Q-003 — How does Barge change another tool's configuration?

Status: open
Pointing npm, bun or Effigy at a placement means editing that tool's own
settings. Does Barge write those files itself (explicitly, with a report and an
undo), print the change for the operator, or use links only?

## Q-004 — How does Oxide's existing state migrate?

Status: open
Oxide's `pools.json` registration, its owned output roots and receipts, and its
scratch markers already exist on disk. The migration order, compatibility
period and who owns the files in between are undecided.

## Q-005 — Which platforms are in scope?

Status: open
macOS on APFS first. Whether Linux (ext4 or btrfs, with no Finder metadata and
different mount identity) is designed for now or later is undecided.
