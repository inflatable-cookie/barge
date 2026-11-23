Prefer a free worktree root with a warm-path record when leasing, most
recently used first, so a lease reuses warm dependency folders instead of a
cold root; pools without warm records keep the previous LRU choice.
