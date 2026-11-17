# Vision

Barge moves heavy, rebuildable developer data off the internal drive onto a
dedicated external volume, and keeps it there safely. Build output, caches,
scratch and similar bulk data stop churning the internal SSD. Tools that would
each reinvent drive handling link Barge's library or call its CLI instead.

## What it does

- **Registers volumes.** A volume is identified by its UUID at its mount, and
  declares its availability (permanent or ephemeral) and durability (expendable
  data Barge may prune, or kept data it never touches).
- **Gives consumers areas.** A consumer such as Oxide declares an area on a
  volume. Barge creates owned roots there, tracks holds and pins, and prunes
  whole idle roots when space runs low. What lives inside a root is the
  consumer's business.
- **Falls back deliberately.** When a volume is absent, each area either fails
  clearly or uses a declared internal fallback, visibly. It never silently moves
  bulk writes back onto the internal drive.
- **Keeps space by pruning.** The reserve triggers pruning of the oldest idle
  roots; only a hard floor stops work.
- **Hands out scratch.** Per-project scratch roots with expiry, so test runs
  never scatter folders across a drive root or a home folder.
- **Shows where writes go.** Status reports what lives where and how much space
  each area uses.

## Who uses it

- **Oxide**, by linking the library. An operator who installs only Oxide gets
  drive placement, fallback and pruning with nothing else installed.
- **Effigy and other tools**, through the CLI and its JSON output.
- **A developer directly**, for status, pruning and scratch.

## Success

- The internal SSD carries a small, measured share of development writes.
- One small, readable implementation of volume selection, fallback, ownership
  and pruning replaces the per-tool copies.
- Unplugging the drive, a full drive and a crash mid-delete are all handled
  conservatively and explained clearly.

## Not this

- Not a backup, sync or archive tool.
- Not a build system. Rust build logic stays in Oxide.
- Storage needs no daemon. Oxide links the library and works on its own.
- No silent rewriting of other tools' configuration.
- Not a proof of what every process on the machine is doing. Barge protects
  roots through cooperative holds and never reuses an evicted path
  ([architecture](architecture.md#safety-model)).

## Direction

Barge's larger aim (Tom, 2026-11-17) is to distribute developer resources
across machines: storage, compute, memory and networking, with the glue
between them. That means a library, a CLI for agents, a daemon for machine
management and cross-network reach, and an operator UI. Storage on this
machine is the first resource. Nothing beyond local storage is designed yet,
and none of it may make Oxide depend on a running Barge.

## Authority

Tom approved Barge as a separate product on 2026-10-26, split out of Oxide's
external-drive machinery, with Oxide as a consumer. Tom owns scope and naming.

On 2026-11-17 Tom reset Barge:

- Barge is planned by its own planner (a Claude Code thread), no longer by the
  Oxide Supervisor. Oxide's planner keeps Oxide.
- Storage stays a library Oxide links; an Oxide operator must not need Barge
  installed or running.
- The earlier design (about 141k lines of Rust and 20k lines of proof scripts
  for this one feature) is replaced by the lean design in
  [architecture](architecture.md). Tom: "just get it working without all the
  ceremony". Ordinary tests and one independent review replace native proof
  ladders and capture harnesses.
- Barge and Oxide must not be coupled: Barge carries no Oxide-specific code
  and reads no Oxide formats.

Moving live data, changing host or tool settings, and deleting existing data
outside Barge's own roots still need Tom's explicit approval.
