# Vision

Barge moves heavy, rebuildable developer data off the internal drive and onto
a dedicated external volume, then keeps it there safely. Data comes back when
needed. Tools that would otherwise each reinvent drive handling use Barge
instead. It manages build output, caches, scratch, virtual machines and
similar bulk data that should not live on, or churn, the internal SSD:
rebuildable data it may prune, and kept data it only places and never deletes.

## What it does

- **Registers volumes.** A registered external volume is identified by its
  UUID and mount. It declares its availability (permanent: always attached;
  ephemeral: attached on and off) and its durability (expendable data Barge
  may prune and recreate, or kept data it never touches). Placement behaviour
  follows from both ([architecture](architecture.md#volume-modes)). Each
  volume also sets a `root_dir` (empty means the drive root) and, optionally, a
  folder for each feature group of expendable data, so a drive used for other
  things keeps Barge's data in one place. Kept data stays wherever the user
  organises it ([locations](architecture.md#locations-on-a-volume-tom-2026-10-26)).
- **Places data.** A path such as a package cache, a scratch tree or a VM
  folder is relocated onto a registered volume behind an owned link. The
  original path keeps working, and the placement can be undone.
- **Falls back deliberately.** Each placement kind declares what happens when
  its volume is absent or low: wait, fail with a clear message, or use an
  explicit temporary internal location. It never silently moves bulk writes
  back onto the internal drive.
- **Keeps space by pruning.** A volume's reserve is a prune trigger, not a stop.
  Below it, Barge removes the oldest idle owned data until the volume is back
  above the reserve; only a hard floor stops work.
- **Owns what it creates.** Every root Barge creates carries an owner marker
  and identity, so it can prove what is safe to remove: nothing with an open
  writer, nothing it does not own and nothing it cannot positively identify.
  Deletion tolerates Finder and other metadata that macOS writes on external
  volumes.
- **Hands out scratch.** It provides a scratch area per project, with markers,
  expiry and cleanup, so test and proof runs never scatter folders across a
  drive root or a home folder.
- **Shows where writes go.** It reports what lives where, how much space each
  placement uses, and which volumes take the write traffic.

## Who uses it

- Oxide, through Barge's Rust library, for its build pool's volume, output
  placement, scratch and pruning. Oxide keeps Cargo, slots, leases, recipes
  and freshness.
- Effigy, Queue and other tools, through the CLI and its JSON output, for
  scratch roots, run folders and workspace placement.
- A developer directly, for package caches (npm, bun, Cargo), VMs and other
  bulk data.

## Success

- The internal SSD carries a small, steady share of development writes, and
  the share is measured, not guessed.
- One audited implementation of volume selection, fallback, ownership and safe
  deletion replaces the per-tool copies.
- Unplugging the drive, a full drive and a crash mid-delete are all handled
  conservatively and explained clearly.
- Adopting a placement is one command, and undoing it is one command.

## Not this

- Not a backup, sync or archive tool. Barge places kept data but does not copy
  or protect it; backing it up stays the owner's responsibility.
- Not a build system. Rust build logic stays in Oxide.
- No background daemon in the first version: commands and scheduled host jobs
  only.
- No silent rewriting of other tools' configuration. Changing a tool's own
  settings to point at a placement is an explicit, reported step.

## Authority

Tom approved creating Barge as a separate product on 2026-10-26, split out
from Oxide's external-drive machinery, with Oxide becoming a consumer. Tom
owns scope and naming. Moving live data, changing host settings and deleting
existing data each need explicit operator approval until a placement kind is
qualified.

Tom selected bounded shared descriptor-listing design in decision
`9e68c8ce-ac61-4190-a31c-b45f99fd6ecd` v2, then “Implement reviewed listing
on fixtures” in decision `13834566-5b7f-44be-a655-6444a4eeaa32` v2
(2026-11-10). The [writer-custody contract](contracts/stage1-design.md#bounded-shared-descriptor-listing-2026-11-10)
keeps the 4096-process, 65,536-returned-row and 10 s ceilings. The approved
scope covers implementation, ordinary checks and native readiness, with
independent exact-head review. Diagnostic, author and distinct reviewer native
occurrences each require separate planner activation before merge. No
installation, live adoption, capacity increase or retained-image cleanup follows.

Tom asked for simpler shared recovery on 2026-11-10 (decision
`1d4a7aee-a8e6-4a75-96f0-b142c4176db2` v2): the delegated-deletion machinery
must shrink in both products, and a later consumer must not rebuild the
recovery protocol. Tom then selected implementation of the reviewed
single-authority design — one Barge-owned operation journal with typed
durable caller hooks — on disposable fixtures on 2026-11-11 (decision
`1735df31-7cc1-4748-9f8a-0767ee6259ff` v2), in bounded reviewed slices
([stage 4 design](contracts/stage4-design.md#one-operation-journal-for-delegated-recovery)).
The first slice, the journal/integrity store, is implemented (barge#058);
the checked coordinator, the Oxide caller and the old join's removal are
later slices. No slice connects a caller or changes shipped behavior until
its own reviewed code lands, and live migration/adoption, native execution
and host change remain separately gated.

Tom reaffirmed on 2026-11-12 that the existing Parallels macOS VM is the
selected preparation context for Barge proofs and no new host account is wanted
(decision `49861fc2` v3). The [proof execution context](contracts/proof-execution-context.md)
records the guest identity, storage and evidence boundaries; it authorizes no
VM action or proof.
