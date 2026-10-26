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

Status: answered 2026-10-26
Answer: it does not have to. Tom's ruling bcaa83d8 (carried by barge#011 v2)
decides that each candidate is classified from the kernel's own process table
(`sysctl`/`KERN_PROC_ALL`: effective uid and start identity, no privilege) before
any descriptor is inspected; a process of another effective uid is the named
blind spot, reported in the `Safe` evidence and in `describe.custody`, and does
not block. An unreadable effective uid and a same-uid descriptor that cannot be
read stay `Unknown`. The measured reason it mattered: `PROC_PIDTBSDINFO` answers
`EPERM` for another uid's processes (357 of 1328 on macOS 26.6.2, 25G83), so
identifying processes that way made every whole-system scan `Unknown`. See the
[stage 1 design](contracts/stage1-design.md#writer-custody-and-safe-deletion).

## Q-008 — What about this account's own processes whose descriptors cannot be read?

Status: open (measured 2026-10-26 in barge#011 v2)
Measured: `PROC_PIDFDVNODEPATHINFO` answers `EPERM` for some descriptors of this
account's own processes — the sandboxed and TCC-protected system daemons:
`progressd`, `routined`, `ScreenTimeAgent`, `dmd`, `lsd`, `nsurlsessiond`,
`UserEventAgent`, `com.apple.quicklook.ThumbnailsAgent` on macOS 26.6.2, 8 of
1004 processes of this account. Listing their descriptors is allowed; reading
the path of the protected ones is not.
Consequence: under the rule that an unreadable descriptor is `Unknown`, a
whole-system scan is `Unknown` on a real session, so a full-scan `Safe` verdict is
not reachable there — the same "no delete proceeds" symptom from a third cause.
The correction in barge#011 does not touch it: an unreadable same-uid descriptor
stays `Unknown` by the brief, and relaxing that would let a same-uid writer hide.
Question for the architect: is a same-uid process whose descriptor paths cannot
be read a second named blind spot (counted and reported like a foreign effective
uid), or does custody keep refusing? Any answer changes what `Safe` promises, so
it is a decision, not a correction.
