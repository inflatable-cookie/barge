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

Status: open
Context: Tom's rulings put kept data outside the group folders, and `root_dir`
may be empty, which makes the drive root the folder that holds everything Barge
owns on that volume. The stage 1 design reads the ruling as "outside the
resolved feature folders and every declared area", so kept data can live
anywhere else on a volume whose `root_dir` is empty; the alternative is that kept
data needs a non-empty `root_dir` on its volume, which would force a layout
change on the Vault drive. This decides the containment predicate used at adopt
and at every later use, so it must be settled before area and placement
implementation. The proposed reading and the alternative are in the
[stage 1 design](contracts/stage1-design.md#open-design-issues) (O-004).
