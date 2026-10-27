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

Status: answered 2026-10-26
Answer: a vnode descriptor whose path and vnode facts the platform both hide is
an OS blind spot for a process its own code-signing status verifies as an Apple
platform binary (`csops`/`CS_OPS_STATUS` carries `CS_PLATFORM_BINARY`, with the
kernel start identity unchanged around the read). It is counted and reported in
the `Safe` evidence and in `describe.custody`, and it does not block; every other
hidden descriptor, a failed status read and an identity that moves around it stay
`Unknown` (Tom, decision e528a712, implemented by barge#013); see the [stage 1
design](contracts/stage1-design.md#writer-custody-and-safe-deletion).
Measured before the decision (barge#011 v2, macOS 26.6.2):
`PROC_PIDFDVNODEPATHINFO` answered `EPERM` for 8 of 1004 processes of this
account — the sandboxed and TCC-protected system daemons `progressd`, `routined`,
`ScreenTimeAgent`, `dmd`, `lsd`, `nsurlsessiond`, `UserEventAgent` and
`com.apple.quicklook.ThumbnailsAgent` — while listing their descriptors was
allowed. That made a whole-system scan `Unknown` under the rule that every
unreadable descriptor refuses, and so a full-scan `Safe` verdict unreachable.
