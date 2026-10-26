# Barge documentation

Barge moves heavy, rebuildable developer data (build output, caches, scratch, VMs) off the internal drive onto a dedicated external volume, and keeps it there safely with deliberate fallback, owned roots and safe pruning. Oxide and other tools use it instead of each handling the drive themselves.

- [Vision and operator ruling](knowledge/vision.md)
- [Architecture and the boundary with Oxide](knowledge/architecture.md)
- [Stage 1 design: library, CLI, locations, areas, scratch and caches](knowledge/contracts/stage1-design.md)
- [Open decisions](knowledge/questions.md)
- [Knowledge map](knowledge/README.md)

Repository knowledge records current truth, not work status.
