A root's marker is now removed last, after every other entry, so a deletion
that stops partway leaves owned trash the next eviction cleanup or sweep
finishes instead of a markerless folder Barge refuses.
