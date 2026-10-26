# Architecture

Barge is one Rust library and a thin CLI over it. Oxide links the library;
every other consumer uses the CLI and its versioned JSON output. There is no
daemon: state lives in files under the account's state directory and on each
registered volume, protected by file locks.

## Concepts

- **Volume.** A registered external APFS volume: UUID, mount point, mode
  (permanent or ephemeral), reserve (the prune trigger) and floor (the hard
  stop). Registration is account-wide, not per `HOME`.
- **Placement.** One relocated path: its kind, its original path, its owned
  root on a volume, its fallback policy and its owner. A placement is adopted
  explicitly and can be undone explicitly.
- **Owned root.** A directory Barge created, carrying an owner marker with a
  random identity, the volume UUID and the placement it serves. Only owned
  roots are ever deleted, and only whole.
- **Scratch area.** A per-project tree under the volume's `scratch/<project>`,
  with marked run roots and expiry.
- **Writer custody.** Before any move, swap or delete, Barge proves that no
  process has the data open. Where it cannot prove that, it keeps the data.

## Invariants

- Nothing is written to a drive root or a home folder as scratch.
- A placement never silently falls back to the internal drive. Every fallback
  is declared per kind and is visible when it happens.
- Low space prunes the oldest idle owned data first; only the floor refuses
  work.
- Deletion is whole-root, of owned roots only, and tolerates Finder metadata
  (`.DS_Store`, `._*`) recreated mid-delete. Any other unexpected entry stops
  the delete and keeps the root.
- A command that will refuse changes nothing first.
- Unknown is never treated as absent, idle or empty.

## Boundary with Oxide

**Moves to Barge**, staged so Oxide never loses a working path:

1. Scratch areas and their expiry, which today live in Oxide's proof supervisor
   (`<mount>/scratch/oxide`) and in host scripts.
2. Package-cache and bulk-data placements, which today are host scripts and
   hand-edited tool configuration.
3. Volume registration, mode, selection, reserve and floor, flush and
   surprise-removal detection, which today are Oxide's removable-volume support
   (`apfs-volume-v1`, `pools.json`).
4. Owned output roots, their markers, the owned-link primitive and pruning,
   which today are Oxide's `oxide-outputs` and `oxide outputs prune`.

**Stays in Oxide:** the Cargo front door, slots, leases and the ledger,
recipes and profiles, the freshness gate, `oxide exec`, Cargo-specific output
member links, and its own proofs of those.

Oxide calls Barge to learn where its pool, outputs and scratch live and how
much room there is. Barge never interprets Cargo layouts.

## Consumers today

These are the parts Barge replaces once each placement kind is qualified:

- host scripts in `~/.local/share/scratch-reaper/` (daily scratch reaper and
  Oxide outputs prune, the output-emptying script and the Cargo cache swap);
- npm (`cache` in `~/.npmrc`) and bun (`~/.bunfig.toml`) caches already pointed
  at `/Volumes/Builds/caches/`;
- Effigy's `[scratch] root` and its volume gate;
- Oxide's proof supervisor scratch parent and seven-day expiry;
- the Parallels VMs on the Vault volume.
