# Barge documentation

Barge moves heavy, rebuildable developer data (build output, caches, scratch, VMs) off the internal drive onto a dedicated external volume, and keeps it there safely with deliberate fallback, owned roots and safe pruning. Oxide and other tools use it instead of each handling the drive themselves.

- [Vision and operator ruling](knowledge/vision.md)
- [Architecture and the boundary with Oxide](knowledge/architecture.md)
- [Stage 1 design: library, CLI, locations, areas, scratch and caches](knowledge/contracts/stage1-design.md)
- [Host-job hand-off: how a launchd job calls `barge maintain`](knowledge/contracts/host-job.md)
- [Stage 3 design: registration, space policy, reserve pruning and the floor](knowledge/contracts/stage3-space-policy.md)
- [Builds real-drive qualification procedure](knowledge/contracts/t9-real-drive-qualification.md)
- [Stage 4 design: owned-root identity across remounts and absence proofs for pruning](knowledge/contracts/stage4-design.md)
- [F6-O2 B-S1 capture tape and release-plan harness](knowledge/contracts/o2-tape.md)
- [Open decisions](knowledge/questions.md)
- [Knowledge map](knowledge/README.md)

Repository knowledge records current truth, not work status.
