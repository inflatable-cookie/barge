# Questions

Questions that block or shape work. Reference them by ID from the plan and from
briefs. An answered question keeps only its pointer to where the answer lives.

## Q-001 — What does each placement kind do when its volume is absent?

Status: answered 2026-10-26
Answer: per kind: build output uses a visible local fallback; caches and scratch fail clearly; kept data is unavailable (Tom); see [architecture](architecture.md#absent-volume).

## Q-002 — Which volume owns which data when several are registered?

Status: answered 2026-10-26
Answer: one home per placement chosen by durability: expendable data on Builds, kept data on Vault; moves are explicit (Tom); see [architecture](architecture.md#concepts).

## Q-003 — How does Barge change another tool's configuration?

Status: answered 2026-10-26
Answer: links first; a tool's own setting is written only during an explicit adopt, reported and undoable (Tom); see [architecture](architecture.md#later).

## Q-004 — How does Oxide's existing state migrate?

Status: revised 2026-11-17
Answer: nothing moves on disk: Oxide's areas map to its existing folder names. Barge no longer reads Oxide's formats; Oxide adopts the lean library and keeps its own records (Tom, 2026-11-17); see [architecture](architecture.md#layout-on-a-volume).

## Q-005 — Which platforms are in scope?

Status: answered 2026-10-26
Answer: macOS on APFS only for now, with an OS layer throughout so Linux can follow later (Tom); see [architecture](architecture.md#platforms).

## Q-006 — Does kept data have to sit outside `root_dir`, or only outside its managed folders?

Status: answered 2026-10-26
Answer: outside Barge's own folders and every declared area, not outside `root_dir` (design freeze d3e170db, O-004, matching Tom's ruling c80e2034). The Vault needs no layout change; see [architecture](architecture.md#layout-on-a-volume).

## Q-007 — Can a same-uid descriptor scan read every process it must decide?

Status: retired 2026-11-17
Answer: moot. The lean design removed the writer custody machinery this answered (Tom, 2026-11-17); see [architecture](architecture.md#safety-model). The earlier answer stays in Git history.

## Q-008 — What about this account's own processes whose descriptors cannot be read?

Status: retired 2026-11-17
Answer: moot. The lean design removed the writer custody machinery this answered (Tom, 2026-11-17); see [architecture](architecture.md#safety-model). The earlier answer stays in Git history.

## Q-009 — Should Barge manage VM mounts, such as Colima profiles?

Open (raised by Effigy, Tom 2026-10-27). Container fixtures run in a Colima VM
whose profile mounts only `$HOME`, so files that `nerdctl compose` must open
cannot live in Barge's scratch on the Builds drive. Proposed direction, not
designed: a VM mount is another tool-configuration placement under Q-003. Barge
would write a profile's `mounts:` entry only on explicit adopt, mount only a
declared folder (for example `scratch/`) and never the whole volume, report the
change exactly and undo it. It would also need a rule for an absent volume at VM
start. This is not in stage 1. Until it is designed, mount changes stay manual
operator changes to the Colima profile.

## Q-010 — What about a vnode descriptor that is dead (revoked or reclaimed)?

Status: retired 2026-11-17
Answer: moot. The lean design removed the writer custody machinery this answered (Tom, 2026-11-17); see [architecture](architecture.md#safety-model). The earlier answer stays in Git history.

## Q-011 — What makes an interrupted release unknown?

Status: retired 2026-11-17
Answer: moot. The lean design removed the activity record recovery machinery this answered (Tom, 2026-11-17); see [architecture](architecture.md#safety-model). The earlier answer stays in Git history.

## Q-012 — How is an unconfirmed activity record marked as unknown when the state volume refuses every write?

Status: retired 2026-11-17
Answer: moot. The lean design removed the activity record recovery machinery this answered (Tom, 2026-11-17); see [architecture](architecture.md#safety-model). The earlier answer stays in Git history.

## Q-013 — Who is authoritative for an area declaration, the stored record or the caller's object?

Status: answered 2026-11-01
Answer: the stored `barge.area/1` record. A library caller presents the same
declaration or is refused before any effect, naming the differing `folder`,
`overrides`, `absent`, `prune` or `reserve`; the first creation, when no record
exists, publishes the caller's validated declaration under `barge area declare`'s
rules with generation 1, and a later creation can only match it. The library reads
the state directory itself, so a caller object can no longer override a stored
policy, and the effective declaration recorded in a root's marker is the governing
record's (Tom's Q-G decision `12729764`; Oxide Supervisor's Barge design ruling,
Queue decision `5e57544c-dfc8-4d2c-9c0b-a7a363c7b547`; D2 of Oxide's F6 adoption
contract, closing O-003); see the [architecture](architecture.md#concepts).
