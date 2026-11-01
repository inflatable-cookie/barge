"""Assertions for stage 3 guard documents, without filesystem effects."""


def success(code, document):
    if code != 0 or document.get("schema") != "barge.space.guard/1" or document.get("ok") is not True:
        raise AssertionError("expected a completed guard document")
    return document["data"]


def refusal(code, document, kind):
    if (code != 2 or document.get("schema") != "barge.error/1"
            or document.get("ok") is not False or document.get("error", {}).get("kind") != kind):
        raise AssertionError(f"expected exit 2 with {kind}")
    return document["error"].get("detail", {})


def rows(container):
    if container is None or container.get("truncated") is not False:
        raise AssertionError("row set is missing or truncated")
    result = container["rows"]
    if container.get("count") != len(result) or container.get("total") != len(result):
        raise AssertionError("row set counts do not match")
    return result


def root_ids(data):
    return [row["root_id"] for row in rows(data["pruned"])]


def case_1(code, document):
    data = success(code, document)
    assert data["decision"] == "admit" and root_ids(data) == [] and data["rounds"] == 0
    assert data["observed_kept"] is None
    assert data["detail"]["owned_prunable_bytes"] is None
    assert data["detail"]["held_or_unknown_bytes"] is None
    assert data["detail"]["excluded_bytes"] is None
    assert data["detail"]["other_estimate_bytes"] is None


def case_2(code, document, older_root, younger_root, candidates, kept):
    data = success(code, document)
    planned = rows(candidates)
    kept_rows = rows(kept)
    planned_ids = [row["root_id"] for row in planned]
    keys = [(row["last_release_ms"], row["first_created_ms"] or (2**64 - 1), row["root_id"])
            for row in planned]
    assert len(planned_ids) == len(set(planned_ids)) and keys == sorted(keys)
    assert planned_ids and planned_ids[0] == older_root and younger_root in planned_ids
    assert planned[0]["area"] == "proof/second"
    assert next(row for row in planned if row["root_id"] == younger_root)["area"] == "proof/first"
    pruned = root_ids(data)
    assert data["decision"] == "admit" and pruned and pruned[0] == older_root
    assert rows(data["pruned"])[0]["area"] == "proof/second"
    assert len(pruned) == len(set(pruned)) and all(root in planned_ids for root in pruned)
    assert [planned_ids.index(root) for root in pruned] == sorted(planned_ids.index(root) for root in pruned)
    assert not set(pruned).intersection(row.get("root_id") for row in kept_rows)
    if data["observed_kept"] is not None:
        assert not set(pruned).intersection(row.get("root_id") for row in rows(data["observed_kept"]))
    assert data["detail"]["stop"] == "target_reached"
    assert data["after"]["free_bytes"] > data["policy"]["reserve_bytes"]
    return {"planned_lru": [{"root_id": row["root_id"], "area": row["area"],
                             "last_release_ms": row["last_release_ms"],
                             "first_created_ms": row["first_created_ms"]} for row in planned],
            "pruned_order": pruned, "F": data["before"]["free_bytes"],
            "F_prime": data["after"]["free_bytes"], "R": data["policy"]["reserve_bytes"],
            "L": data["policy"]["floor_bytes"]}


def case_3(code, document, held_root, idle_roots):
    data = success(code, document)
    assert set(root_ids(data)) == set(idle_roots) and len(root_ids(data)) == len(idle_roots)
    kept = rows(data["observed_kept"])
    assert any(row["root_id"] == held_root and row["reason"] == "hold_live" for row in kept)
    assert data["detail"]["held_or_unknown_bytes"] > 0


def case_4(code, document):
    detail = refusal(code, document, "space_below_floor")
    assert detail["pruned_root_ids"] == [] and detail["cause"] == "held_owned_roots"
    assert detail["free_bytes"] <= detail["floor_bytes"]
    assert detail["held_or_unknown_bytes"] > 0


def case_4b(code, document, root):
    data = success(code, document)
    assert data["decision"] == "stop_below_floor" and root_ids(data) == [root]
    assert data["detail"]["stop"] == "nothing_eligible"


def case_5(code, document, hold_root, activity_root):
    data = success(code, document)
    kept = {row["root_id"]: row["reason"] for row in rows(data["observed_kept"])}
    assert kept[hold_root] == "hold_unknown" and kept[activity_root] == "activity_unknown"
    assert hold_root not in root_ids(data) and activity_root not in root_ids(data)


def case_6(code, document, root):
    data = success(code, document)
    assert data["decision"] == "stop_below_floor" and root_ids(data) == [root]
    detail = data["detail"]
    assert detail["reserve_bytes"] < detail["free_bytes"] <= detail["floor_bytes"]


def case_7(code, document, root):
    data = success(code, document)
    assert root_ids(data) == [root]
    assert data["detail"]["stop"] == "space_not_reclaimed"
    assert data["detail"]["cause"] == "budget_exhausted" and data["reclaimed_bytes"] == 0
    return data["guard"]


def case_7_recheck(code, document):
    data = success(code, document)
    assert data["decision"] == "continue"


def case_8(code, document, root):
    data = success(code, document)
    detail = data["detail"]
    free = data["before"]["free_bytes"]
    policy = data["policy"]
    if free <= max(policy["reserve_bytes"], policy["floor_bytes"]):
        assert root in detail["replayed_root_ids"] and detail["cause"] == "replay"
        if detail["free_bytes"] <= policy["floor_bytes"]:
            assert data["decision"] == "stop_below_floor"
    else:
        assert data["decision"] == "admit"
        assert any(row["root_id"] == root and row["reason"] == "pending_intent"
                   for row in rows(data["kept"]))
    return free


def self_check():
    """Counterfactuals for the guard document checks; no process or fixture."""
    import copy

    def rowset(items):
        return {"rows": items, "count": len(items), "total": len(items), "truncated": False}

    base = {"schema": "barge.space.guard/1", "ok": True, "data": {
        "decision": "admit", "pruned": rowset([]), "observed_kept": None, "rounds": 0,
        "detail": {"owned_prunable_bytes": None, "held_or_unknown_bytes": None,
                   "excluded_bytes": None, "other_estimate_bytes": None}}}
    problems = []
    try:
        case_1(0, base)
    except (AssertionError, KeyError, TypeError) as error:
        problems.append(f"case 1 positive oracle refused: {error}")
    altered = copy.deepcopy(base)
    altered["data"]["detail"]["held_or_unknown_bytes"] = 0
    try:
        case_1(0, altered)
    except AssertionError:
        pass
    else:
        problems.append("case 1 accepted attribution zero instead of null")
    planned = rowset([
        {"root_id": "older", "area": "proof/second", "last_release_ms": 1, "first_created_ms": 1},
        {"root_id": "younger", "area": "proof/first", "last_release_ms": 2, "first_created_ms": 2},
    ])
    guard = {"schema": "barge.space.guard/1", "ok": True, "data": {
        "decision": "admit", "pruned": rowset([{"root_id": "older", "area": "proof/second"},
                            {"root_id": "younger", "area": "proof/first"}]),
        "observed_kept": rowset([]), "before": {"free_bytes": 40}, "after": {"free_bytes": 50},
        "policy": {"reserve_bytes": 48, "floor_bytes": 24}, "detail": {"stop": "target_reached"}}}
    try:
        case_2(0, guard, "older", "younger", planned, rowset([]))
    except (AssertionError, KeyError, TypeError) as error:
        problems.append(f"case 2 batch-order positive oracle refused: {error}")
    guard["data"]["pruned"]["rows"].reverse()
    try:
        case_2(0, guard, "older", "younger", planned, rowset([]))
    except AssertionError:
        pass
    else:
        problems.append("case 2 accepted the younger root first")
    for code, document in ((0, {"schema": "barge.error/1", "ok": False,
                                "error": {"kind": "volume_mismatch"}}),
                           (2, {"schema": "barge.error/1", "ok": False,
                                "error": {"kind": "usage"}})):
        try:
            refusal(code, document, "volume_mismatch")
        except AssertionError:
            pass
        else:
            problems.append("registration mismatch oracle accepted a wrong exit or kind")
    return problems
