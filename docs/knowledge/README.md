# Knowledge map

[Vision](vision.md) owns scope, Tom's rulings, direction and authority. [Architecture](architecture.md) owns the lean storage design: safety model, concepts, layout, library surface, space policy, CLI, testing and the boundary with Oxide. [Resources](resources.md) owns the resource classes, layout manifest, bindings (including workflow blocks) and doctor. [Machines](machines.md) owns machine control: the daemon and protocol. [App](app.md) owns the Barge desktop app: form, scope, architecture and build order. [Worktrees](worktrees.md) owns worktree pools: leases, return and warm paths. [Containers](containers.md) owns stack leases and cleanup. [Runs](runs.md) owns platform resolution and run evidence. [Questions](questions.md) owns design decisions.

Contracts:

- [Consumption](contracts/consumption.md) owns the exact-revision dependency path and the CLI JSON contract consumers check.
- [Quality](contracts/quality.md) owns the code-quality rubric, the per-change gate and sweeps.

There are no other contracts. A fact belongs to the file that owns it; link to
it rather than restating it.
