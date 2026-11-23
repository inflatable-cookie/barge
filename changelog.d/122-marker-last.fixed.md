A root's marker is now removed last and put back if the empty folder cannot be
removed, so a deletion that stops partway leaves owned trash the next eviction
cleanup or sweep finishes.
