"""Pure custody decisions for the stage 3 native proof.

The caller owns all filesystem and process operations. Unknown observations must
never be converted to a positive detach decision or a successful post-state.
"""

from dataclasses import dataclass
from pathlib import PurePosixPath
import math
import re

_DEVICE = re.compile(r"/dev/disk[0-9]{1,4}(?:s[0-9]{1,3})?\Z")
_UUID = re.compile(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\Z")


def custody_document(root, backings):
    """Build the supervisor's pre-attach declaration for both owned images.

    Paths must already be canonical and symlink-free; the caller must verify that
    property on disk before publishing. This pure check also rejects lexical
    escapes, duplicate images, and paths outside the owned root.
    """
    root_path = PurePosixPath(root)
    if not root_path.is_absolute() or str(root_path) != root or '..' in root_path.parts:
        raise ValueError("owned root must be an absolute canonical path")
    if len(backings) != 2 or len(set(backings)) != 2:
        raise ValueError("exactly two distinct image backings are required")
    for backing in backings:
        path = PurePosixPath(backing)
        if (not path.is_absolute() or str(path) != backing or '..' in path.parts
                or root_path not in path.parents or any(ord(c) < 32 for c in backing)):
            raise ValueError("image backing is not a canonical child of the owned root")
    return {"version": 1, "backings": list(backings)}


def detach_request(backing, mount, device, volume_uuid):
    """Build the supervisor's request after the caller verifies image identity."""
    if not _DEVICE.fullmatch(device) or not _UUID.fullmatch(volume_uuid):
        raise ValueError("invalid device or volume UUID")
    if not mount.startswith('/Volumes/') or PurePosixPath(mount).parent != PurePosixPath('/Volumes'):
        raise ValueError("mount is not a direct child of /Volumes")
    if not PurePosixPath(backing).is_absolute():
        raise ValueError("backing is not absolute")
    return {"version": 1, "backing": backing, "mount": mount, "device": device,
            "volume_uuid": volume_uuid, "note": "stage 3 space native proof"}


@dataclass(frozen=True)
class DetachPreconditions:
    """Positive observations required before the one in-script detach."""

    all_children_reaped: bool
    request_withdrawn: bool
    identity_verified: bool
    remaining_seconds: float

    def timeout(self):
        """Return the bounded timeout, or refuse without attempting a detach."""
        if (self.all_children_reaped is not True or self.request_withdrawn is not True
                or self.identity_verified is not True or not math.isfinite(self.remaining_seconds)
                or self.remaining_seconds <= 0):
            raise ValueError("ordinary detach preconditions are not established")
        return min(30.0, self.remaining_seconds)


def detached_poststate(mount_absent, backing_entities):
    """Accept only a positively absent mount and a complete empty entity census.

    None denotes an unreadable observation. A failed or ambiguous result requires
    the caller to abort and retain the owned root and evidence.
    """
    return mount_absent is True and isinstance(backing_entities, list) and not backing_entities
