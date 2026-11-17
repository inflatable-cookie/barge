# Knowledge map

[Vision](vision.md) owns scope, Tom's rulings, direction and authority. [Architecture](architecture.md) owns the lean storage design: safety model, concepts, layout, library surface, space policy, CLI, testing and the boundary with Oxide. [Questions](questions.md) owns design decisions. [Retirements](retired.toml) records removed concepts.

Contracts:

- [Consumption](contracts/consumption.md) owns the exact-revision dependency path.
- `rust-quality-*.json` and `language-quality-activation.json` own the RustQuality profile and deviations.

Every other file under `contracts/` describes the pre-reset design (writer
custody, the operation journal, capture tapes and native proofs). They are no
longer current truth and are deleted with that code by the lean-core task.
