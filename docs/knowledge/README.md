# Knowledge map

[Vision](vision.md) owns scope, Tom's ruling and authority. [Architecture](architecture.md) owns concepts, invariants and the boundary with Oxide. [Questions](questions.md) owns open design decisions. [Retirements](retired.toml) records removed concepts.

Contracts own durable interfaces and formats:

- [Consumption](contracts/consumption.md) owns the install and exact-revision dependency paths.

- [Stage 1 design](contracts/stage1-design.md) owns the crate and CLI shape,
  versioned JSON, account state and volume records, area declarations, root
  markers, scratch and cache placement, writer custody and safe deletion, the
  scheduled-job contract, the stage 1 failure and race matrix, the implementation
  slices and the proposed validation.
- [Stage 3 space policy](contracts/stage3-space-policy.md) owns volume
  registration of the reserve and floor, the space measurement and its limits,
  the reserve prune trigger and the hard floor, area `reserve` participation,
  idleness and least-recently-used order, concurrent pruning under consumer
  holds, the stage 3 formats, bounds, failure matrix, slices and its bounded
  native design.
- [Stage 4 design](contracts/stage4-design.md) owns owned-root identity across
  remounts, the durable identity and the path-absence documents and their
  versions, the reference check, the gate that authorizes releasing a whole
  owned root, how that gate combines with the stage 3 space prune, the
  diagnostic that answers the carried "device or inode changed" finding, the
  read-only Oxide output-registry census and its drift report, the stage 4
  matrix and bounds, the Oxide compatibility reading of the absence rule, the
  stage 4 slices and the proposed validation.
- [Host-job hand-off](contracts/host-job.md) owns how a host job calls
  `barge maintain`: the preview-then-real protocol, the digest it passes, the
  exit codes it records, what it must never do, and one example plist under
  `docs/`, uninstalled.
- [S6 native proof](contracts/s6-native-proof.md) records S6's elevated native
  proof plan: the fixed command, the pinned supervisor bytes, the nine HIGH
  premises traced through the case bodies, the receipt shape and the activation
  order. It is a plan, not a receipt.
- [S7 native proof](contracts/s7-maintain-proof.md) records S7's elevated native
  proof plan: `barge maintain` preview and real on one owned image, the pinned
  supervisor, the nine premises, the six cases, the receipt shape and the
  activation order. It is a plan, not a receipt.

Rust quality policy owns the strict profile for the Rust workspace:
[activation](contracts/language-quality-activation.json) records the
repository's everyday-authoring opt-in,
[profile](contracts/rust-quality-profile.json) declares the profile and its MSRV
sources, and [deviations](contracts/rust-quality-deviations.json) records every
accepted deviation. The `effigy rust:*` selectors in [AGENTS.md](../../AGENTS.md)
are the validation route through `oxide`.
