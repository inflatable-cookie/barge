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

## Locations on a volume (Tom, 2026-10-26)

A drive may have other uses, so where Barge puts things is configurable for
each registered volume.

- **`root_dir`** is the folder on the volume, relative to its mount, that holds
  everything Barge owns there. Empty means the drive root. Examples:
  `root_dir = "barge"` keeps all of Barge's data under `/Volumes/<drive>/barge/`;
  `root_dir = ""` puts the groups directly at the root.
- **Feature groups** each get one folder under `root_dir`, using these defaults:
  `scratch/` (per-project scratch), `caches/` (package caches), `outputs/`
  (owned build-output roots), `pools/` (build pools such as Oxide's slots) and
  `kept/` (kept placements such as VMs). Any group's folder can be overridden
  per volume, still relative to `root_dir`.
- A path is resolved once, when a placement is adopted, and recorded in that
  placement. Changing `root_dir` or a group folder later never silently moves
  existing data; relocating it is an explicit command.
- Barge never writes outside its resolved folders. A folder name that would
  escape the volume (absolute, `..`, or a link leaving the mount) is refused.
- Today's Builds layout (`scratch/`, `caches/`, `oxide-pool/` and
  `oxide-outputs/` at the drive root) is expressed as `root_dir = ""` with the
  pool and output groups mapped to Oxide's existing folder names, so adopting
  Barge moves nothing (Q-004).

## Volume modes

A volume has two independent properties, and a placement's behaviour follows
from both.

**Availability** describes when the volume is attached:

- **Permanent:** always attached, for example a drive on the desk dock. Absence
  is abnormal: Barge reports it loudly, and each placement applies its declared
  absent-volume policy.
- **Ephemeral:** attached on and off, for example a drive that travels or a
  dock in another room. Absence is normal: placements on it are expected to be
  unavailable at times, and coming back is routine.

**Durability** describes what the data on the volume is:

- **Expendable:** rebuildable data such as build output, package caches and
  scratch. Barge may prune it under space pressure and may recreate it
  elsewhere.
- **Kept:** data that lives only on that volume, such as VMs or large archives.
  Barge never prunes it, never recreates it elsewhere and never treats an absent
  volume's copy as gone.

Examples today: Builds is permanent and expendable; Vault is ephemeral and
kept.

### Placement mechanics by mode

- **Permanent + expendable.** The original path is an owned link into the
  volume. When the volume is absent, the declared policy applies: build output
  may use an explicit, visible checkout-local fallback; caches and scratch wait
  or fail. On return, Barge relinks to the volume root, keeps any fallback root
  until it is pruned, and never merges fallback data back in. Low space prunes
  the oldest idle owned roots down to the reserve.
- **Ephemeral + expendable.** The same link model, but absence is routine:
  commands fall back without alarm, show the fallback in status, and relink
  quietly on return. The original location is the authoritative home and the
  volume is a warm extra, so no data is lost when it is away.
- **Ephemeral + kept.** No fallback copy ever exists. When the volume is away,
  the placement is unavailable and Barge says which volume to attach. On return
  it is usable again with no reconciliation. Barge prunes nothing and recreates
  nothing.
- **Permanent + kept.** Treated like ephemeral + kept, except that absence is
  reported loudly.

### Attach, detach and surprise removal

- Before a volume is used, its identity is verified by UUID and mount, every
  time. A different volume at the same mount point is never used.
- A placement that is being written when its volume disappears fails that
  write cleanly. Its owned root is checked on return before anything reuses it.
- An ordinary detach first waits for every writer on that volume to finish, and
  flushes.
- Every mode change is explicit and recorded. Changing durability from kept to
  expendable requires operator confirmation, because it makes the data
  prunable.

## Placement rules (Tom, 2026-10-26)

- **Absent volume, by kind (Q-001).** Build output uses a visible local
  fallback, which is Oxide's current rule. Package caches fail with a clear
  message naming the drive to attach, so they never quietly re-download onto
  the internal SSD. Scratch fails clearly. Kept data is unavailable and Barge
  names the drive.
- **One home per placement, chosen by durability (Q-002).** Expendable data
  (build output, caches, scratch) lives on the permanent expendable volume
  (Builds). Kept data (VMs, archives) lives on the kept volume (Vault). A
  placement's volume is chosen when it is adopted, and moving it to another
  volume is an explicit command. There is no automatic preference list.
- **Links first, tool configuration only if needed (Q-003).** Barge prefers an
  owned link at the tool's existing path, which needs no configuration change.
  Where a tool can only be redirected by its own setting (npm `cache`, bun
  `install.cache.dir`, Effigy `[scratch] root`), Barge writes that setting only
  during an explicit adopt, reports the exact change and can undo it.
- **Oxide state is read in place (Q-004).** Barge reads Oxide's existing
  formats as they are (`pools.json`, `oxide-outputs` roots, markers and
  receipts, scratch markers), so nothing moves on disk. Oxide switches to Barge
  one part at a time, in the order above. Old formats retire only once both
  sides agree.

## Platforms

macOS on APFS only for now (Tom, 2026-10-26, Q-005). Every platform-specific fact sits behind an OS layer: volume identity and mount, writer custody, Finder metadata, flush and detach. Linux can then be added later without changing the placement model.

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
