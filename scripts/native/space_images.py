"""Image custody operations for the stage 3 native proof.

This module never cleans up a backing. The qualified supervisor owns final
cleanup. An uncertain child or attachment means abort with custody retained.
"""
import json
import os
import plistlib
import subprocess
import time

from space_custody import DetachPreconditions, custody_document, detach_request, detached_poststate


class CustodyUnknown(Exception):
    """The owned image or a child could not be positively accounted for."""


class Images:
    def __init__(self, root, evidence, env, work_deadline_ns):
        self.root = os.path.realpath(root)
        self.evidence = os.path.realpath(evidence)
        self.env = env
        self.work_deadline_ns = work_deadline_ns
        self.children = set()
        self.unknown = False
        self.current = None
        self.events = []
        self.backings = [os.path.join(self.root, name) for name in
                         ("space-primary.sparseimage", "space-secondary.sparseimage")]
        if not os.path.isdir(self.root) or not os.path.isdir(self.evidence):
            raise CustodyUnknown("owned root or evidence directory is absent")
        if os.path.realpath(os.path.dirname(self.root)) != os.path.dirname(self.root):
            raise CustodyUnknown("owned root traverses a symlink")
        self._publish("image-custody.json", custody_document(self.root, self.backings))

    def remaining(self):
        return max(0, (self.work_deadline_ns - time.clock_gettime_ns(time.CLOCK_UPTIME_RAW)) / 1e9)

    def _publish(self, name, document):
        path = os.path.join(self.evidence, name)
        temporary = path + ".tmp"
        try:
            fd = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
            with os.fdopen(fd, "w", encoding="utf-8") as handle:
                json.dump(document, handle, separators=(",", ":"))
                handle.flush()
                os.fsync(handle.fileno())
            os.replace(temporary, path)
        except OSError as error:
            self.unknown = True
            raise CustodyUnknown(f"cannot publish {name}: {error}") from error

    def run(self, argv, timeout):
        remaining = self.remaining()
        if remaining <= 0:
            raise CustodyUnknown("no work time remains")
        child = subprocess.Popen(argv, env=self.env, cwd=self.root, stdin=subprocess.DEVNULL,
                                 stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        self.children.add(child)
        try:
            stdout, stderr = child.communicate(timeout=min(timeout, remaining))
        except subprocess.TimeoutExpired as error:
            # Do not assume that signalling or a timed wait reaped the child. The
            # supervisor's custody protocol retains the root in this situation.
            self.unknown = True
            raise CustodyUnknown(f"child timed out: {argv[0]}") from error
        self.children.remove(child)
        return child.returncode, stdout, stderr

    def create(self, index):
        if self.current is not None or self.unknown:
            raise CustodyUnknown("cannot create while an attachment is active or unknown")
        backing = self.backings[index]
        if os.path.lexists(backing):
            raise CustodyUnknown("image backing already exists")
        code, _, err = self.run(["/usr/bin/hdiutil", "create", "-size", "128m", "-fs", "APFS",
                                 "-type", "SPARSE", backing], 60)
        if code != 0:
            raise CustodyUnknown(f"image creation failed: {err[-512:]!r}")

    def attach(self, index, mount):
        if self.current is not None or self.unknown:
            raise CustodyUnknown("another image may be attached")
        backing = self.backings[index]
        if os.path.realpath(backing) != backing or os.path.lexists(mount):
            raise CustodyUnknown("backing is not canonical or mount already exists")
        code, out, err = self.run(["/usr/bin/hdiutil", "attach", backing, "-mountpoint", mount,
                                   "-nobrowse", "-owners", "on", "-plist"], 60)
        if code != 0:
            self.unknown = True
            raise CustodyUnknown(f"attach outcome is unknown: {err[-512:]!r}")
        try:
            entities = plistlib.loads(out)["system-entities"]
            matched = [row for row in entities if row.get("mount-point") == mount]
            if len(matched) != 1:
                raise ValueError("not exactly one mounted entity")
            device = matched[0]["dev-entry"]
            code, out, _ = self.run(["/usr/sbin/diskutil", "info", "-plist", mount], 10)
            if code != 0:
                raise ValueError("diskutil failed")
            uuid = plistlib.loads(out)["VolumeUUID"].lower()
            code, text, _ = self.run(["/usr/sbin/diskutil", "info", mount], 10)
            owners = [line.strip().partition(":")[2].strip().lower()
                      for line in text.decode("utf-8", "strict").splitlines()
                      if line.strip().lower().startswith("owners:")]
            if code != 0 or owners != ["enabled"]:
                raise ValueError("ownership is not positively enabled")
            request = detach_request(backing, mount, device, uuid)
            self._publish("detach-request.json", request)
            self.current = request
            self.events.append({"action": "attach", **request, "owners": "enabled"})
            return request
        except Exception as error:
            self.unknown = True
            raise CustodyUnknown(f"attach identity is unknown: {error}") from error

    def detach(self):
        """Withdraw the request and attempt one ordinary detach, never retrying."""
        if self.current is None or self.unknown:
            raise CustodyUnknown("no verified current attachment")
        request = self.current
        # Rebind both mount and backing before withdrawing the supervisor request.
        code, out, _ = self.run(["/usr/bin/hdiutil", "info", "-plist"], 10)
        if code != 0:
            raise CustodyUnknown("attachment census failed")
        try:
            images = plistlib.loads(out)["images"]
            matches = [image for image in images if image.get("image-path") == request["backing"]]
            if len(matches) != 1 or not any(
                row.get("mount-point") == request["mount"] and row.get("dev-entry") == request["device"]
                for row in matches[0]["system-entities"]
            ):
                raise ValueError("backing, mount and device no longer agree")
            code, out, _ = self.run(["/usr/sbin/diskutil", "info", "-plist", request["mount"]], 10)
            info = plistlib.loads(out)
            if code != 0 or info.get("VolumeUUID", "").lower() != request["volume_uuid"]:
                raise ValueError("volume UUID no longer agrees")
        except Exception as error:
            self.unknown = True
            raise CustodyUnknown(f"attachment identity is unknown: {error}") from error
        if self.children:
            raise CustodyUnknown("children are not all reaped")
        try:
            os.unlink(os.path.join(self.evidence, "detach-request.json"))
        except OSError as error:
            self.unknown = True
            raise CustodyUnknown("detach request could not be withdrawn") from error
        timeout = DetachPreconditions(not self.children, True, True, self.remaining()).timeout()
        self.unknown = True  # Never retry after an attempted detach or ambiguous post-state.
        code, _, err = self.run(["/usr/bin/hdiutil", "detach", request["mount"]], timeout)
        if code != 0:
            raise CustodyUnknown(f"ordinary detach failed: {err[-512:]!r}")
        code, out, _ = self.run(["/usr/bin/hdiutil", "info", "-plist"], 10)
        if code != 0:
            raise CustodyUnknown("post-detach census failed")
        try:
            images = plistlib.loads(out)["images"]
            entities = [row for row in images if row.get("image-path") == request["backing"]]
            absent = not os.path.lexists(request["mount"])
        except Exception as error:
            raise CustodyUnknown("post-detach census unreadable") from error
        if not detached_poststate(absent, entities):
            raise CustodyUnknown("detach post-state is unknown or still attached")
        self.events.append({"action": "detach", "mount": request["mount"],
                            "backing": request["backing"], "post_state": "detached"})
        self.current = None
        self.unknown = False


def self_check():
    """Exercise the real detach gate with inert canned observations and no I/O."""
    from unittest.mock import patch

    problems = []
    backing = "/owned/image.sparseimage"
    mount = "/Volumes/barge-proof"
    device = "/dev/disk12s1"
    volume_uuid = "12345678-1234-1234-1234-123456789abc"
    attachment = plistlib.dumps({"images": [{"image-path": backing, "system-entities": [
        {"mount-point": mount, "dev-entry": device}]}]})
    absent = plistlib.dumps({"images": []})
    info = plistlib.dumps({"VolumeUUID": volume_uuid})

    def make():
        fixture = Images.__new__(Images)
        fixture.current = detach_request(backing, mount, device, volume_uuid)
        fixture.unknown = False
        fixture.children = set()
        fixture.evidence = "/owned/evidence"
        fixture.events = []
        fixture.remaining = lambda: 12.0
        calls = []
        def run(argv, timeout):
            calls.append((argv, timeout))
            if argv[1:3] == ["info", "-plist"]:
                return 0, attachment if len(calls) == 1 else absent, b""
            if argv[1:3] == ["detach", mount]:
                return 0, b"", b""
            if argv[1:3] == ["info", "-plist"] or argv[0].endswith("diskutil"):
                return 0, info, b""
            raise AssertionError(f"unexpected command {argv!r}")
        # diskutil and hdiutil both use info -plist, so distinguish by program.
        def dispatch(argv, timeout):
            if argv[0].endswith("diskutil"):
                calls.append((argv, timeout))
                return 0, info, b""
            return run(argv, timeout)
        fixture.run = dispatch
        return fixture, calls

    fixture, calls = make()
    with patch("space_images.os.unlink") as unlink, patch("space_images.os.path.lexists", return_value=False):
        fixture.detach()
    if (unlink.call_count != 1 or len(calls) != 4 or calls[2][0] != ["/usr/bin/hdiutil", "detach", mount]
            or calls[2][1] != 12.0 or fixture.current is not None or fixture.unknown):
        problems.append("ordinary detach sequence, timeout or post-state changed")
    fixture, calls = make()
    fixture.children.add(object())
    with patch("space_images.os.unlink") as unlink:
        try:
            fixture.detach()
        except CustodyUnknown:
            pass
        else:
            problems.append("detach accepted an unreaped child")
    if unlink.called or any("detach" in argv for argv, _ in calls):
        problems.append("unreaped child reached request withdrawal or detach")
    fixture, calls = make()
    with patch("space_images.os.unlink", side_effect=OSError("unavailable")):
        try:
            fixture.detach()
        except CustodyUnknown:
            pass
        else:
            problems.append("detach accepted a request that was not withdrawn")
    if not fixture.unknown or any("detach" in argv for argv, _ in calls):
        problems.append("failed withdrawal reached detach or lost unknown state")
    return problems
