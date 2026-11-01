"""Stage 3 native scenarios. All effects target the owned image and state."""
import errno
import hashlib
import json
import os
import signal
import subprocess
import sys
import time

import space_oracles as oracle
from space_fixture import CaseInfrastructure

MIB = 1024 * 1024


def case_2_filler_step(free, reserve, payload, size):
    """Choose one bounded adjustment from Barge's latest measured free bytes."""
    target = reserve - payload + 4 * MIB
    if free <= 0 or target <= 0 or size < 0 or size > 128 * MIB:
        raise CaseInfrastructure("case 2 filler observation is outside the image bounds")
    if target <= free < reserve and size > 0:
        return ("done", 0)
    if free < target:
        amount = min(size, target - free + MIB)
        if amount <= 0:
            raise CaseInfrastructure("case 2 cannot shorten the filler to recover free space")
        return ("shorten", amount)
    # A probe and subsequent bounded top-ups prevent a large unexplained drop
    # from being repeated before Barge can measure it.
    gap = free - target
    amount = min(4 * MIB, max(64 * 1024, gap // 2))
    if size + amount > 128 * MIB or amount >= free:
        raise CaseInfrastructure("case 2 filler would exceed its owned image bound")
    return ("append", amount)


def case_2_sizing(free, reserve, payload):
    """Check and record the pre-guard free-space and payload invariant."""
    result = {"setup_free_bytes": free, "older_bytes": payload, "reserve_bytes": reserve,
              "reclaim_margin_bytes": free + payload - reserve}
    if free <= 0 or free >= reserve or free + payload <= reserve + 2 * MIB:
        raise AssertionError(f"case 2 setup does not establish the two MiB margin: {result}")
    return result


def case_2_sizing_self_check():
    """Model real image bounds and both historical measured post-filler values."""
    problems = []
    total, initial_free = 134176768, 132763648
    reserve, payload = 48 * MIB, 12 * MIB
    if not 0 < initial_free <= total or initial_free - payload <= reserve:
        problems.append("case 2 model's image or case-1 reading is invalid")
    # A zero-size first filler is a bounded probe, followed by measured steps.
    action, amount = case_2_filler_step(initial_free - payload, reserve, payload, 0)
    if action != "append" or not 0 < amount <= 4 * MIB:
        problems.append("case 2 did not start with a bounded filler probe")
    # The two failed receipts' actual post-filler free readings are the inputs,
    # not assumptions about their metadata overhead. Model a positive measured
    # result after shortening only where the first reading is below target.
    for label, observed in (("f73230b", 37343232), ("4ed7d8d", 47820800)):
        size = 40 * MIB
        action, amount = case_2_filler_step(observed, reserve, payload, size)
        if action == "shorten":
            if amount <= 0 or amount > size:
                problems.append(f"{label}: invalid shortening amount")
                continue
            observed += amount  # synthetic measured reclaim, not an APFS guarantee
            size -= amount
            action, _ = case_2_filler_step(observed, reserve, payload, size)
        if action != "done":
            problems.append(f"{label}: measured correction did not reach the target band")
        else:
            try:
                case_2_sizing(observed, reserve, payload)
            except AssertionError as error:
                problems.append(f"{label}: corrected measurement refused: {error}")
    try:
        case_2_filler_step(1, reserve, payload, 0)
    except CaseInfrastructure:
        pass
    else:
        problems.append("case 2 accepted an unrecoverable filler shortfall")
    return problems


def case_4b_fill_target(free, floor):
    """Size the idle payload from the measured free space and floor gap."""
    if free <= 0 or free >= floor:
        raise ValueError("case 4b requires positive free space below the floor")
    payload = min(2 * MIB, (floor - free) // 2, free // 2)
    if payload < MIB:
        raise ValueError("case 4b lacks room for a measurable idle root below the floor")
    return free - payload


def case_4b_sizing_self_check():
    """Exercise the case 4 setup and the floor and free-space boundaries."""
    problems = []
    floor = 24 * MIB
    for free in (18 * MIB, 20 * MIB, 21 * MIB):
        target = case_4b_fill_target(free, floor)
        payload = free - target
        if target <= 0 or not 0 < payload < floor - free:
            problems.append(f"case 4b sizing is invalid for {free} free bytes")
    for free in (0, MIB, 23 * MIB, floor, floor + MIB):
        try:
            case_4b_fill_target(free, floor)
        except ValueError:
            pass
        else:
            problems.append(f"case 4b accepted an unusable measurement: {free}")
    return problems


class Cases:
    def __init__(self, fixture):
        self.fixture = fixture
        self.first = None
        self.second = None
        self.filler = None
        self.context = {}

    def case_1(self):
        fixture = self.fixture
        fixture.declare("first")
        fixture.declare("second")
        self.first = fixture.root_create("first")
        self.second = fixture.root_create("second")
        if fixture.free() <= 48 * MIB:
            raise AssertionError("new fixture image is not above the reserve")
        code, document = fixture.guard()
        oracle.case_1(code, document)
        if not os.path.isdir(self.first["path"]) or not os.path.isdir(self.second["path"]):
            raise AssertionError("above-reserve guard removed a root")
        return {"free_bytes": document["data"]["before"]["free_bytes"]}

    def case_2(self):
        fixture = self.fixture
        # The 12 MiB older payload precedes the filler. Both writes stay on the
        # owned image; the filler is sized from the measurement after release.
        payload = 12 * MIB
        reserve_target = 48 * MIB
        fixture.hold(self.second, "old", os.getpid())
        try:
            if fixture.free() <= payload:
                raise CaseInfrastructure("case 2 image lacks space for the older payload")
            fixture.fill_bytes(os.path.join(self.second["path"], "payload"), payload)
            fixture.release(self.second, "old")
            time.sleep(0.05)
            fixture.idle(self.first, "new")
            fixture.wait_until_idle()
            free_before_filler = fixture.free()
            self.filler = os.path.join(fixture.mount, "proof-filler")
            measured, filler_bytes = free_before_filler, 0
            adjustments = []
            self.context["2"] = {"free_before_filler": free_before_filler,
                                 "older_root": self.second["root_id"], "older_area": "proof/second",
                                 "adjustments": adjustments}
            for _ in range(32):
                action, amount = case_2_filler_step(measured, reserve_target, payload, filler_bytes)
                adjustments.append({"free_bytes": measured, "filler_bytes": filler_bytes,
                                    "action": action, "bytes": amount})
                if action == "done":
                    break
                measured = fixture.adjust_filler(self.filler, action, amount, filler_bytes)
                filler_bytes += amount if action == "append" else -amount
            else:
                raise CaseInfrastructure("case 2 measured filler adjustment did not converge")
        except OSError as error:
            if error.errno == errno.ENOSPC:
                raise CaseInfrastructure("case 2 fixture write ran out of image space") from error
            raise
        sizing = case_2_sizing(measured, reserve_target, payload)
        sizing.update({"free_before_filler": free_before_filler, "filler_bytes": filler_bytes,
                       "adjustments": adjustments})
        self.context["2"]["sizing"] = sizing
        _, preview = fixture.command("space", "prune", "--volume", fixture.uuid, "--plan")
        candidates = preview["data"]["plan"]["candidates"]
        candidate_rows = oracle.rows(candidates)
        older = [row for row in candidate_rows if row["root_id"] == self.second["root_id"]]
        if len(older) != 1:
            raise AssertionError("case 2 older root is not a unique eligible candidate")
        reserve = preview["data"]["policy"]["reserve_bytes"]
        older_bytes = older[0]["bytes"]
        if reserve != reserve_target:
            raise AssertionError("case 2 registered reserve changed")
        sizing["planned_older_bytes"] = older_bytes
        self.context["2"]["candidates"] = candidates
        code, document = fixture.guard()
        evidence = oracle.case_2(code, document, self.second["root_id"], self.first["root_id"],
                                 candidates, preview["data"]["kept"])
        if os.path.exists(self.second["path"]):
            raise AssertionError("case 2 older root survived")
        evidence["sizing"] = sizing
        return evidence

    def case_3(self):
        fixture = self.fixture
        os.unlink(self.filler)
        self.filler = None
        held = fixture.root_create("first")
        idle = fixture.root_create("second")
        payload = os.path.join(held["path"], "writer")
        with open(payload, "xb") as handle:
            handle.write(os.urandom(4096))
        before = os.stat(payload)
        writer = subprocess.Popen([sys.executable, os.path.join(os.path.dirname(__file__),
                                  "space_writer.py"), payload], cwd=fixture.root, env=fixture.env,
                                  stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                                  stderr=subprocess.PIPE, text=True)
        fixture.images.children.add(writer)
        try:
            # A bounded poll of the child's pipe: never wait indefinitely for a
            # child that failed before its readiness line.
            import select
            ready, _, _ = select.select([writer.stdout], [], [], min(10, fixture.images.remaining()))
            if not ready or writer.stdout.readline().strip() != "ready":
                raise AssertionError("writer did not become ready")
            hold = fixture.hold(held, "writer", writer.pid)
            if hold["hold"]["pid"] != writer.pid or not hold["hold"]["process_start"]:
                raise AssertionError("writer hold lacks its process identity")
            fixture.hold(idle, "idle", os.getpid())
            free = fixture.free()
            fixture.fill_to(os.path.join(idle["path"], "payload"), free - 12 * MIB)
            fixture.release(idle, "idle")
            fixture.wait_until_idle()
            self.filler = os.path.join(fixture.mount, "proof-filler")
            measured = fixture.fill_to(self.filler, 35 * MIB)
            if not 24 * MIB < measured < 48 * MIB:
                raise AssertionError("case 3 free space outside its band")
            idle_roots = [idle["root_id"]]
            if os.path.isdir(self.first["path"]):
                idle_roots.append(self.first["root_id"])
            code, document = fixture.guard()
            oracle.case_3(code, document, held["root_id"], idle_roots)
        finally:
            if writer.poll() is None:
                writer.send_signal(signal.SIGTERM)
            try:
                out, err = writer.communicate(timeout=min(10, fixture.images.remaining()))
            except subprocess.TimeoutExpired as error:
                fixture.images.unknown = True
                raise AssertionError("writer could not be reaped") from error
            fixture.images.children.remove(writer)
        if writer.returncode != 0:
            raise AssertionError(f"writer failed: {err[-400:]}")
        last = int(out.strip())
        after = os.stat(payload)
        with open(payload, "rb") as handle:
            record = handle.read()
        if (before.st_ino != after.st_ino or before.st_size != after.st_size or last < 1
                or record != f"{last:020d}".encode().ljust(4096, b".")):
            raise AssertionError("held writer inode, size or final record changed unexpectedly")
        fixture.release(held, "writer")
        self.first = held
        return {"writer_pid": writer.pid, "writer_start": hold["hold"]["process_start"],
                "last_record": last, "setup_free_bytes": measured}

    def case_4(self):
        fixture = self.fixture
        fixture.hold(self.first, "floor", os.getpid())
        free = fixture.free()
        if free <= 20 * MIB:
            raise AssertionError("insufficient setup space for held root")
        measured = fixture.fill_to(os.path.join(self.first["path"], "floor-payload"), 18 * MIB)
        if measured > 24 * MIB:
            raise AssertionError("case 4 did not cross the floor")
        code, document = fixture.guard(expect=2)
        oracle.case_4(code, document)
        if not os.path.isdir(self.first["path"]):
            raise AssertionError("held floor root disappeared")
        return {"setup_free_bytes": measured}

    def case_4b(self):
        fixture = self.fixture
        idle = fixture.root_create("second")
        fixture.hold(idle, "short", os.getpid())
        before = fixture.free()
        target = case_4b_fill_target(before, 24 * MIB)
        measured = fixture.fill_to(os.path.join(idle["path"], "payload"), target)
        fixture.release(idle, "short")
        fixture.wait_until_idle()
        code, document = fixture.guard()
        oracle.case_4b(code, document, idle["root_id"])
        if os.path.exists(idle["path"]):
            raise AssertionError("case 4b idle root survived")
        return {"setup_free_bytes": measured, "after_free_bytes": document["data"]["after"]["free_bytes"]}

    def case_5(self):
        fixture = self.fixture
        os.unlink(self.filler)
        self.filler = None
        unknown_hold = fixture.root_create("first")
        missing_activity = fixture.root_create("second")
        activity = os.path.join(fixture.env["BARGE_STATE_DIR"], "activity",
                                missing_activity["root_id"] + ".json")
        os.unlink(activity)
        hold = fixture.hold(unknown_hold, "unknown", os.getpid())
        os.chmod(hold["path"], 0)
        try:
            with open(hold["path"], "rb"):
                pass
        except PermissionError:
            pass
        else:
            raise AssertionError("fixture hold is readable under this account")
        free = fixture.free()
        fixture.fill_to(os.path.join(unknown_hold["path"], "payload"), free - 3 * MIB)
        free = fixture.free()
        fixture.fill_to(os.path.join(missing_activity["path"], "payload"), free - 3 * MIB)
        self.filler = os.path.join(fixture.mount, "proof-filler")
        measured = fixture.fill_to(self.filler, 35 * MIB)
        if not 24 * MIB < measured < 48 * MIB:
            raise AssertionError("case 5 free space outside its band")
        code, document = fixture.guard()
        oracle.case_5(code, document, unknown_hold["root_id"], missing_activity["root_id"])
        if not os.path.isdir(unknown_hold["path"]) or not os.path.isdir(missing_activity["path"]):
            raise AssertionError("case 5 removed a protected root")
        return {"setup_free_bytes": measured}

    def case_6(self):
        fixture = self.fixture
        images = fixture.images
        first_uuid, first_mount = fixture.uuid, fixture.mount
        images.detach()
        if os.path.lexists(first_mount):
            raise AssertionError("first mount was recreated after detach")
        images.create(1)
        second_mount = first_mount + "-second"
        attachment = images.attach(1, second_mount)
        second_uuid = fixture.register(second_mount, 16 * MIB, 32 * MIB, "second-plan.json")
        if attachment["volume_uuid"] != second_uuid.lower() or second_uuid == first_uuid:
            raise AssertionError("second registration did not name its own volume")
        idle = fixture.root_create("first")
        fixture.hold(idle, "second-idle", os.getpid())
        free = fixture.free()
        fixture.fill_to(os.path.join(idle["path"], "payload"), free - 2 * MIB)
        fixture.release(idle, "second-idle")
        fixture.wait_until_idle()
        filler = os.path.join(second_mount, "proof-filler")
        measured = fixture.fill_to(filler, 28 * MIB)
        if not 16 * MIB < measured <= 30 * MIB:
            raise AssertionError("case 6 free space outside its band")
        code, document = fixture.guard()
        oracle.case_6(code, document, idle["root_id"])
        after = document["data"]["after"]["free_bytes"]
        images.detach()
        if os.path.lexists(second_mount):
            raise AssertionError("second mount was recreated after detach")
        attachment = images.attach(0, first_mount)
        if attachment["volume_uuid"] != first_uuid.lower():
            raise AssertionError("reattached primary image UUID changed")
        fixture.uuid, fixture.mount = first_uuid, first_mount
        return {"setup_free_bytes": measured, "after_free_bytes": after,
                "second_uuid": second_uuid}

    def case_7(self):
        fixture = self.fixture
        os.unlink(self.filler)
        self.filler = None
        root = fixture.root_create("second")
        fixture.hold(root, "hardlink", os.getpid())
        outside = os.path.join(fixture.mount, "proof-outside-link")
        free = fixture.free()
        fixture.fill_to(outside, free - 10 * MIB)
        os.link(outside, os.path.join(root["path"], "payload"))
        fixture.release(root, "hardlink")
        fixture.wait_until_idle()
        self.filler = os.path.join(fixture.mount, "proof-filler")
        measured = fixture.fill_to(self.filler, 35 * MIB)
        if not 24 * MIB < measured < 48 * MIB:
            raise AssertionError("case 7 free space outside its band")
        code, document = fixture.guard()
        guard = oracle.case_7(code, document, root["root_id"])
        if os.path.exists(root["path"]):
            raise AssertionError("case 7 chosen root survived")
        os.unlink(outside)
        code, document = fixture.command("space", "recheck", "--guard", guard)
        oracle.case_7_recheck(code, document)
        return {"setup_free_bytes": measured,
                "recheck_free_bytes": document["data"]["after"]["free_bytes"]}

    def case_8(self):
        fixture = self.fixture
        images = fixture.images
        root = fixture.root_create("second")
        fixture.hold(root, "interrupted", os.getpid())
        free = fixture.free()
        if free <= 32 * MIB:
            raise AssertionError("case 8 has insufficient setup space")
        fixture.fill_to(os.path.join(root["path"], "payload"), free - 5 * MIB)
        fixture.release(root, "interrupted")
        fixture.wait_until_idle()
        if fixture.free() > 48 * MIB:
            raise AssertionError("case 8 is not below the reserve")
        source = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", "scratch-enotempty-shim.c"))
        library = os.path.join(fixture.root, "space-pause.dylib")
        code, _, err = images.run(["/usr/bin/clang", "-dynamiclib", "-Wall", "-Wextra", "-Werror",
                                   source, "-o", library], 30)
        if code:
            raise AssertionError(f"proof interposer compilation failed: {err[-400:]!r}")
        log = os.path.join(images.evidence, "space-pause.log")
        if os.path.lexists(log):
            raise AssertionError("pause log already exists")
        stat = os.stat(root["path"])
        env = dict(fixture.env, DYLD_INSERT_LIBRARIES=library,
                   BARGE_PROOF_PAUSE_PATH=root["path"], BARGE_PROOF_PAUSE_LOG=log)
        child = subprocess.Popen([fixture.binary, "space", "guard", "--volume", fixture.uuid,
                                  "--area", "proof/first", "--json"], env=env, cwd=fixture.root,
                                 stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        images.children.add(child)
        reaped = False
        try:
            until = time.monotonic() + min(30, images.remaining())
            expected = f"{root['path']}\t{stat.st_dev}\t{stat.st_ino}"
            reached = False
            while time.monotonic() < until and child.poll() is None:
                if os.path.isfile(log):
                    with open(log, encoding="utf-8") as handle:
                        reached = handle.read().splitlines() == [expected]
                    if reached:
                        break
                time.sleep(0.01)
            if not reached:
                raise AssertionError("exact-root unlink pause was not reached")
            child.send_signal(signal.SIGTERM)
            out, err = child.communicate(timeout=min(10, images.remaining()))
            images.children.remove(child)
            reaped = True
            if child.returncode != -signal.SIGTERM or out:
                raise AssertionError(f"paused guard did not terminate without a document: {child.returncode}, {out!r}, {err[-200:]!r}")
        finally:
            if not reaped:
                images.unknown = True
        pending = self.pending_intents()
        matching = [row for row in pending
                    if (row.get("unit", {}).get("root_id") or row.get("unit", {}).get("name"))
                    == root["root_id"]]
        if len(matching) != 1:
            raise AssertionError("interrupted root has no unique pending intent")
        capture = matching[0]["unit"]["capture_name"]
        if not capture.startswith(".barge-prune-") or os.path.basename(capture) != capture:
            raise AssertionError("intent has an unsafe quarantine name")
        quarantine = os.path.join(os.path.dirname(root["path"]), capture)
        if not os.path.isdir(quarantine):
            raise AssertionError("interrupted quarantine leaf is absent")
        old_uuid, mount = fixture.uuid, fixture.mount
        images.detach()
        if os.path.lexists(mount):
            raise AssertionError("mount recreated after interruption detach")
        code, absent = fixture.guard(expect=2)
        oracle.refusal(code, absent, "volume_absent")
        if not self.pending_intents() or os.path.lexists(mount):
            raise AssertionError("absence changed the intent or recreated the mount")
        attachment = images.attach(0, mount)
        if attachment["volume_uuid"] != old_uuid.lower():
            raise AssertionError("reattached image UUID changed")
        code, replay = fixture.guard()
        measured = oracle.case_8(code, replay, root["root_id"])
        def sha256(path):
            digest = hashlib.sha256()
            with open(path, "rb") as handle:
                for block in iter(lambda: handle.read(1024 * 1024), b""):
                    digest.update(block)
            return digest.hexdigest()
        return {"free_bytes": measured, "root_id": root["root_id"],
                "decision": replay["data"]["decision"], "source_sha256": sha256(source),
                "interposer_sha256": sha256(library)}

    def pending_intents(self):
        directory = os.path.join(self.fixture.env["BARGE_STATE_DIR"], "intents")
        if not os.path.isdir(directory):
            return []
        result = []
        for name in sorted(os.listdir(directory)):
            with open(os.path.join(directory, name), encoding="utf-8") as handle:
                document = json.load(handle)
            if document["phase"] != "deleted":
                result.append(document)
        return result

    def case_9(self):
        # Each successful Images.detach checks mount absence before returning.
        # The stale mount-root seam itself is covered by the Rust unit test.
        if self.fixture.images.current is None or self.fixture.images.unknown:
            raise AssertionError("primary attachment is not positively restored")
        return {"seam": "guard_refuses_absent_and_stale_mount_without_recreating_it"}

    def case_10(self):
        import hashlib
        import tempfile
        fixture = self.fixture
        images = fixture.images
        old_state = fixture.env["BARGE_STATE_DIR"]
        private = tempfile.mkdtemp(prefix="registration-refusals-", dir=fixture.root)
        fixture.env["BARGE_STATE_DIR"] = private
        args = ["volume", "register", "--plan", "--mount", fixture.mount,
                "--availability", "permanent", "--durability", "expendable",
                "--root-dir", "", "--reserve", "48MiB", "--floor", "24MiB",
                "--declared-by", "native-fixture"]
        try:
            _, preview = fixture.command(*args)
            plan = json.loads(preview["data"]["plan"])
            plan["record"]["volume_uuid"] = "A6AC3ED7-33EA-46B4-8082-7254CF291ED2"
            if plan["record"]["volume_uuid"].lower() == fixture.uuid.lower():
                raise AssertionError("tampered UUID unexpectedly names the fixture")
            raw = json.dumps(plan, separators=(",", ":")).encode()
            plan_file = os.path.join(fixture.root, "tampered-registration-plan.json")
            with open(plan_file, "xb") as handle:
                handle.write(raw)
            code, doc = fixture.command("volume", "register", "--plan-file", plan_file,
                                        "--plan-sha256", hashlib.sha256(raw).hexdigest(), expect=2)
            oracle.refusal(code, doc, "volume_mismatch")
            malformed = args.copy()
            malformed[malformed.index("48MiB")] = "not-a-size"
            code, doc = fixture.command(*malformed, expect=2)
            oracle.refusal(code, doc, "usage")
            too_large = args.copy()
            too_large[too_large.index("24MiB")] = "1TiB"
            code, doc = fixture.command(*too_large, expect=2)
            oracle.refusal(code, doc, "usage")
            if os.listdir(private):
                raise AssertionError("registration refusals wrote fixture state")
            images.detach()
            if os.path.lexists(fixture.mount):
                raise AssertionError("mount recreated after final detach")
            code, doc = fixture.command(*args, expect=2)
            oracle.refusal(code, doc, "volume_absent")
            if os.listdir(private) or os.path.lexists(fixture.mount):
                raise AssertionError("absent registration wrote state or recreated mount")
            return {"unverified_uuid_kind": "volume_mismatch", "refusals": 4}
        finally:
            fixture.env["BARGE_STATE_DIR"] = old_state
