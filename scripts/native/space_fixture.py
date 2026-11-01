"""Fixture operations for the bounded stage 3 space proof."""
import json
import os
import time


class FixtureFault(Exception):
    """A fixture operation failed its explicit precondition or oracle."""


class Fixture:
    def __init__(self, binary, images, root, env, pool):
        self.binary = binary
        self.images = images
        self.root = root
        self.env = env
        self.pool = pool
        self.uuid = None
        self.mount = None

    def command(self, *args, expect=0):
        code, out, err = self.images.run([self.binary, *args, "--json"], 30)
        try:
            document = json.loads(out)
        except (ValueError, UnicodeDecodeError) as error:
            raise FixtureFault(f"no JSON from {args!r}: exit {code}, {err[-400:]!r}") from error
        if expect is not None and code != expect:
            raise FixtureFault(f"{args!r}: exit {code}, expected {expect}: {document!r}")
        return code, document

    def register(self, mount, reserve, floor, plan_name):
        self.mount = mount
        _, preview = self.command("volume", "register", "--plan", "--mount", mount,
                                  "--availability", "permanent", "--durability", "expendable",
                                  "--root-dir", "", "--reserve", str(reserve), "--floor", str(floor),
                                  "--declared-by", "native-fixture")
        plan = preview["data"]["plan"]
        filename = os.path.join(self.root, plan_name)
        with open(filename, "x", encoding="utf-8") as handle:
            handle.write(plan)
        _, result = self.command("volume", "register", "--plan-file", filename,
                                 "--plan-sha256", preview["data"]["plan_sha256"])
        self.uuid = result["data"]["volume_uuid"]
        return self.uuid

    def declare(self, area):
        self.command("area", "declare", "--namespace", "proof", "--area", area,
                     "--folder", area, "--absent", "fallback:" + self.pool, "--prune", "oldest-idle:1",
                     "--reserve", "counts")

    def root_create(self, area):
        _, result = self.command("area", "root", "create", "--namespace", "proof",
                                 "--area", area, "--volume", self.uuid, "--kind", "area_root")
        root = result["data"]["root"]
        path = root["path"]
        if os.path.commonpath([os.path.realpath(self.mount), os.path.realpath(path)]) != os.path.realpath(self.mount):
            raise FixtureFault("created root is outside the owned mount")
        return root

    def hold(self, root, holder, pid):
        _, document = self.command("area", "hold", "--root", root["root_id"], "--holder", holder,
                                   "--pid", str(pid), "--scope", "write", "--purpose", "native proof")
        return document["data"]

    def release(self, root, holder):
        self.command("area", "release", "--root", root["root_id"], "--holder", holder)

    def idle(self, root, holder):
        self.hold(root, holder, os.getpid())
        self.release(root, holder)

    def free(self):
        _, document = self.command("space")
        rows = document["data"]["volumes"]
        if rows["truncated"]:
            raise FixtureFault("space report was truncated")
        matches = [row for row in rows["rows"] if row["volume_uuid"].lower() == self.uuid.lower()]
        if len(matches) != 1 or matches[0]["measurement"] is None:
            raise FixtureFault("fixture measurement unavailable")
        return matches[0]["measurement"]["free_bytes"]

    def fill_to(self, path, target):
        """Write allocated bytes only after measuring Barge's current free space."""
        free = self.free()
        if target <= 0 or target >= free:
            raise FixtureFault(f"target {target} is not below measured free {free}")
        # Keep a margin for APFS metadata and require a fresh Barge measurement
        # afterward. Random contents prevent transparent compression of zeroes.
        amount = free - target
        if amount > 128 * 1024 * 1024:
            raise FixtureFault("measured fill exceeds the entire fixture image")
        with open(path, "xb", buffering=0) as handle:
            while amount:
                block = min(amount, 1024 * 1024)
                if handle.write(os.urandom(block)) != block:
                    raise FixtureFault("fixture fill made a short write")
                amount -= block
            os.fsync(handle.fileno())
        return self.free()

    def guard(self, area="first", expect=0):
        return self.command("space", "guard", "--volume", self.uuid,
                            "--area", "proof/" + area, expect=expect)

    @staticmethod
    def wait_until_idle():
        # The fixture declares a one-second prune age; no wall-clock rewriting
        # of product state is needed to make a just-released root idle.
        time.sleep(1.1)

    def custody_preflight(self):
        """Use the product's real deletion custody census on one owned probe."""
        _, document = self.command("scratch", "alloc", "--volume", self.uuid, "--project", "census",
                                   "--consumer", "probe", "--pid", str(os.getpid()))
        root = document["data"]["root"]
        self.command("scratch", "conclude", "--volume", self.uuid, "--root", root["root_id"],
                     "--holder", "probe", "--outcome", "closed", "--closure", "positive",
                     "--kept-reason", "evidence", "--nested-scope", "exclusive")
        self.release(root, "probe")
        marker = os.path.join(root["path"], ".barge-root.json")
        with open(marker, encoding="utf-8") as handle:
            record = json.load(handle)
        record["conclusion"]["concluded_ms"] = int(time.time() * 1000) - 9 * 86400 * 1000
        with open(marker, "w", encoding="utf-8") as handle:
            json.dump(record, handle)
        _, plan = self.command("scratch", "expire", "--volume", self.uuid, "--plan")
        candidates = [row["name"] for row in plan["data"]["plan"]["candidates"]["rows"]]
        if candidates != [root["root_id"]]:
            raise FixtureFault(f"census probe is not the sole candidate: {candidates!r}")
        _, result = self.command("scratch", "expire", "--volume", self.uuid,
                                 "--plan-sha256", plan["data"]["plan_digest"])
        data = result["data"]
        deleted = [row["name"] for row in data["deleted"]["rows"]]
        skipped = data["skipped"]["rows"]
        pending = data["pending"]["rows"]
        if deleted == [root["root_id"]] and not skipped and not pending:
            return {"outcome": "safe", "probe_root": root["root_id"],
                    "plan_digest": plan["data"]["plan_digest"]}
        reasons = [row.get("reason", "") for row in skipped]
        if (len(reasons) == 1 and skipped[0].get("name") == root["root_id"]
                and reasons[0].startswith("custody is not proven") and not deleted and not pending):
            raise CensusUnavailable(reasons[0])
        raise FixtureFault(f"census probe had an unexpected result: {data!r}")


class CensusUnavailable(Exception):
    """The product could not decide the host's same-UID descriptor census."""
