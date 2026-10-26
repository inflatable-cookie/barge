# Questions

Questions that block or shape work. Reference them by ID from the plan and from
briefs. An answered question keeps only its pointer to where the answer lives.

## Q-001 — What does each placement kind do when its volume is absent?

Status: answered 2026-10-26
Answer: per kind: build output uses a visible local fallback; caches and scratch fail clearly; kept data is unavailable (Tom); see [architecture](architecture.md#placement-rules-tom-2026-10-26).

## Q-002 — Which volume owns which data when several are registered?

Status: answered 2026-10-26
Answer: one home per placement chosen by durability: expendable data on Builds, kept data on Vault; moves are explicit (Tom); see [architecture](architecture.md#placement-rules-tom-2026-10-26).

## Q-003 — How does Barge change another tool's configuration?

Status: answered 2026-10-26
Answer: links first; a tool's own setting is written only during an explicit adopt, reported and undoable (Tom); see [architecture](architecture.md#placement-rules-tom-2026-10-26).

## Q-004 — How does Oxide's existing state migrate?

Status: answered 2026-10-26
Answer: Barge reads Oxide's existing formats in place; Oxide switches one part at a time (Tom); see [architecture](architecture.md#placement-rules-tom-2026-10-26).

## Q-005 — Which platforms are in scope?

Status: answered 2026-10-26
Answer: macOS on APFS only for now, with an OS layer throughout so Linux can follow later (Tom); see [architecture](architecture.md#platforms).

## Q-006 — Does kept data have to sit outside `root_dir`, or only outside its managed folders?

Status: answered 2026-10-26
Answer: outside Barge's own folders and every declared area, not outside `root_dir` (design freeze d3e170db, O-004, matching Tom's ruling c80e2034). The Vault needs no layout change; see the [stage 1 design](contracts/stage1-design.md#open-design-issues).

## Q-007 — Can a same-uid descriptor scan read every process it must decide?

Status: open (found 2026-10-26 in barge#011, the S5 self-exclusion correction)
Measured: on macOS 26.6.2 (25G83), `proc_pidinfo(PROC_PIDTBSDINFO)` answers `EPERM`
for the 357 processes of another uid among 1328, because this account may not
read another account's BSD facts. The scan reads that entry before it can apply
its uid filter, so a whole-system scan on a multi-user machine is an unreadable
process entry, and custody is `Unknown` for every root: the same "no delete ever
proceeds" symptom barge#006 and barge#007 saw, from a second cause. The
correction in this task (exclude the calling process's own descriptors) is
necessary and is implemented; it is not sufficient on this machine.
Proposed, for the architect: read each process's uid and start identity from
`sysctl KERN_PROC_ALL`, which reports them for every process, and keep
`proc_pidinfo`/`PROC_PIDLISTFDS` for the descriptors of same-uid processes only.
That reads the same facts through a public interface, so no unknown becomes
safe, but it changes the observation primitive the [stage 1 design](contracts/stage1-design.md#writer-custody-and-safe-deletion)
names, which is why it is a decision, not a correction. Until it is decided, an
unreadable process entry keeps meaning `Unknown`.
