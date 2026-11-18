# Quality gate

Every part of Barge is held at a minimum of 8/10 on one rubric (Tom,
2026-11-18). Barge reached that baseline through independent sweeps. This
contract keeps it there: each change is rated as it's reviewed, so new code
enters at the baseline instead of waiting for another sweep.

## Rubric

Rate each **part** a change touches, 1 to 10, on six dimensions:

| Dimension | What it asks |
|---|---|
| Correctness and safety | Real bugs: races, check-then-act gaps, symlink following, lost errors, panics outside tests, and whether the [safety model](../architecture.md#safety-model) still holds |
| Clarity | Names, function length, control flow, comments that earn their place |
| Structure | Cohesion, size, duplication, module boundaries |
| Error handling | Typed errors of the right kind: refused, usage, unknown state, I/O. Exit codes as the CLI contract says |
| Tests | Real behaviour and failure paths. Deterministic: no sleeps to order threads or processes; no real drives, home folders or network beyond loopback |
| Idiom and simplicity | No needless machinery, no dead code, a public surface no larger than needed |

The part's overall score is the reader's judgement across the six, not an
average. Anchors:
- **10:** exemplary.
- **8:** solid, with only minor nits.
- **7:** good, but with clear improvements due.
- **5-6:** real debt.
- **Below 5:** risky.

A **part** is one library module (for example `area.rs`), one crate's CLI or
daemon surface, the app's Rust side or renderer, the tests touched, or a docs
file.

## The gate

- **Workers** read this contract and rate the parts they touched before
  reporting ready. Any part below 8 is fixed in the same change, or a
  follow-up is briefed and named in the PR. A pre-existing score below 8
  doesn't block a change that doesn't make it worse.
- **The reviewer** rates every touched part at the exact head and lists the
  scores in the review. A part that this change made worse, or that is new
  and below 8, is `changes_required` with the findings that hold it back
  (file:line and what reaches 8). Real bugs come first, whatever the score.
- A claim in a review is checked before it's acted on. Independent reads
  in the sweeps produced false positives, such as a "failing" format check
  and a "broken" macOS identity call that worked on the real drive.

## Sweeps

A sweep is a whole-repository read by independent readers, who aren't shown
earlier scores, against this rubric. It runs when the planner or Tom asks: at a
milestone, or when the gate's scores drift. Findings become briefs grouped by
area, ordered so that batches touching the same files don't run at once. The
quality lane in Nucleus holds the scorecards.
