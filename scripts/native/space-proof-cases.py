#!/usr/bin/env python3
"""Case identities and expected outcomes for the stage 3 native proof.

The proof apparatus must implement and verify these cases, including custody.
This inventory does not execute them.
"""

CASES = (
    ("admit_above_reserve", "admit", 0),
    ("oldest_across_areas", "target_reached", 0),
    ("held_writer", "held_or_unknown_bytes", 0),
    ("below_floor_all_held", "space_below_floor", 2),
    ("below_floor_idle_root", "nothing_eligible", 0),
    ("unreadable_and_missing_activity", "not_idle", 0),
    ("floor_above_reserve", "stop_below_floor", 0),
    ("hard_link_not_reclaimed", "space_not_reclaimed", 0),
    ("interrupted_intent_replay", "measured_replay_or_admit", 0),
    ("absent_mount", "volume_absent", 2),
    ("registration_refusals", "volume_mismatch", 2),
)
