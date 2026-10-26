# Knowledge map

[Vision](vision.md) owns scope, Tom's ruling and authority. [Architecture](architecture.md) owns concepts, invariants and the boundary with Oxide. [Questions](questions.md) owns open design decisions. [Retirements](retired.toml) records removed concepts.

Contracts own durable interfaces and formats:

- [Stage 1 design](contracts/stage1-design.md) owns the crate and CLI shape,
  versioned JSON, account state and volume records, area declarations, root
  markers, scratch and cache placement, writer custody and safe deletion, the
  scheduled-job contract, the stage 1 failure and race matrix, the implementation
  slices and the proposed validation.
