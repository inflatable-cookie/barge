# Barge

Barge moves heavy, rebuildable developer data off the internal drive onto a dedicated external volume and keeps it there safely. Read [docs/README.md](docs/README.md), then the owning knowledge files, before changing anything.

## Ownership and work

Repository: `inflatable-cookie/barge`. Planning, briefs, tasks and reviews belong in Queue once the `barge` project is registered there; do not create repository planning or status mirrors. Until then, changes are knowledge only.

Tom's scope and authority ruling lives in [vision](docs/knowledge/vision.md). Concepts, invariants and the Oxide boundary live in [architecture](docs/knowledge/architecture.md). Read [questions](docs/knowledge/questions.md) and [retirements](docs/knowledge/retired.toml) before planning.

## Guardrails

- No moving live data, changing host or tool settings, or deleting existing data without explicit operator approval for that action.
- Never write scratch to a home folder or a drive root. Use `/Volumes/Builds/scratch/barge/` when the Builds drive is present.
- Do not change Oxide from this repository. Oxide changes go through Oxide's own Queue project.
- When a change alters current truth, update its owning knowledge file in the same change. Record operator rulings before yielding.

## Validate

- `effigy docs:check` runs the Northstar link and retired-concept checks plus `git diff --check HEAD`. Check its exit code.
- There is no code yet. When Rust lands, build with `oxide`, never `cargo`, and run `oxide init` for its guidance block.
