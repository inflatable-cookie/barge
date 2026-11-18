# Knowledge map

[Vision](vision.md) owns scope, Tom's rulings, direction and authority. [Architecture](architecture.md) owns the lean storage design: safety model, concepts, layout, library surface, space policy, CLI, testing and the boundary with Oxide. [Resources](resources.md) owns the proposed resource classes, layout manifest, bindings and doctor. [Machines](machines.md) owns machine control: the daemon, protocol and UI. [Questions](questions.md) owns design decisions. [Retirements](retired.toml) records removed concepts.

Contracts:

- [Consumption](contracts/consumption.md) owns the exact-revision dependency path and the CLI JSON contract consumers check.

There are no other contracts. A fact belongs to the file that owns it; link to
it rather than restating it.
