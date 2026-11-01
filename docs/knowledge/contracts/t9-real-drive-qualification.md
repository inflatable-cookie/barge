# Builds real-drive qualification

This procedure is preparation only. Registration, a real prune and any host-job
change each require Tom's explicit go. The qualification does not claim reclaim
under real pressure. Before F6, Oxide declares no areas: only Barge-owned scratch
under `/Volumes/Builds/barge/scratch` can participate. Any candidate outside that
area is a stop. Oxide and Effigy entries may appear only as kept rows; no scratch
claim or delegation is part of this qualification.

The qualification must establish registration round-trip and record the registry
digest; compare `barge space --json` free and total against `df` within one block;
review the prune candidates; compare before and after listing digests of Builds'
top level, `oxide-pool`, `oxide-outputs` and `scratch`; record the maintenance
preview digest and `space_triggered`; and demonstrate withdrawal to kept durability.
The prune is at most one execution of the reviewed plan. No guard, recheck or
unregister is involved. The complete executable procedure and host-job change
plan are still required before this document can be used.
