"""Verify the known workload through the viewer's production tour and SQL API."""
import json
import pathlib
import re
import sys

if sys.argv[1] == "--dense":
    lines = pathlib.Path(sys.argv[2]).read_text().splitlines()
    assert "tour: rendered Timeline" in lines, "dense Timeline was not rendered"
    assert "tour: regions nodes=1 completed=100000 incomplete=0" in lines, lines
    raster = next(re.fullmatch(
        r"tour: region_render spans=100000 rows=(\d+) columns=(\d+) items=(\d+)", line
    ) for line in lines if line.startswith("tour: region_render "))
    rows, columns, items = map(int, raster.groups())
    assert 0 < items <= rows * columns, (rows, columns, items)
    print("  dense region rendering respects the per-row pixel budget")
    sys.exit(0)

prefix = sys.argv[1]
reference = pathlib.Path(sys.argv[2])


def read_rows(kind):
    document = json.loads((reference / f"region-check.{kind}.json").read_text())
    assert not document["truncated"], f"{kind} reference query was truncated"
    return document["rows"]


spans = read_rows("spans")
samples = read_rows("samples")
lines = pathlib.Path(prefix + ".tour").read_text().splitlines()
reports = [json.loads(line.removeprefix("tour: region "))
           for line in lines if line.startswith("tour: region ")]
assert "tour: rendered Timeline" in lines, "Timeline was not rendered"
assert "tour: rendered Events" in lines, "Events was not rendered"
assert "tour: regions nodes=8 completed=15 incomplete=1" in lines, lines
assert len(reports) == 8, reports
baseline = next(line.removeprefix("tour: unfiltered_balance ") for line in lines
                if line.startswith("tour: unfiltered_balance "))
unfiltered_balance = {row["thread_id"]: row["busy_fraction"] for row in json.loads(baseline)}

# These paths come from the workload's source, independently of the viewer's
# inferred parents. Each recursive call encloses exactly one deeper call.
expected_counts = {
    ("iteration",): 5,
    ("iteration", "compute"): 5,
    ("other_parent",): 1,
    ("other_parent", "compute"): 1,
    ("worker",): 1,
    ("worker", "recursive"): 1,
    ("worker", "recursive", "recursive"): 1,
    ("unfinished",): 0,
}
groups = {path: [] for path in expected_counts}
for span in spans:
    name = span["name"]
    if name in {"iteration", "other_parent", "worker", "unfinished"}:
        path = (name,)
    elif name == "compute":
        parent = next(s for s in spans if s["name"] in {"iteration", "other_parent"}
                      and s["pid"] == span["pid"] and s["tid"] == span["tid"]
                      and s["start_ns"] <= span["start_ns"] < s["end_ns"])
        path = (parent["name"], name)
    else:
        assert name == "recursive", name
        outer_calls = sum(s["name"] == name and s["tid"] == span["tid"]
                          and s["start_ns"] < span["start_ns"] < s["end_ns"]
                          for s in spans)
        path = ("worker",) + (name,) * (outer_calls + 1)
    groups[path].append(span)


def sample_count(invocations, time=None, disabled_threads=()):
    return sum(
        sample["thread_id"] not in disabled_threads
        and (time is None or time["start_ns"] <= sample["timestamp"] < time["end_ns"])
        and any(s["pid"] == sample["process_id"]
                and s["tid"] == sample["thread_id"]
                and s["start_ns"] <= sample["timestamp"] < s["end_ns"]
                for s in invocations)
        for sample in samples
    )


def check_balance(actual):
    assert all(0 <= row["busy_fraction"] <= 1 for row in actual), actual


for report in reports:
    path = tuple(report["path"])
    assert path in expected_counts, path
    complete = [s for s in groups.pop(path) if s["end_ns"] is not None]
    assert report["count"] == expected_counts[path], report
    assert report["incomplete"] == (path == ("unfinished",)), report
    inclusive = sum(s["end_ns"] - s["start_ns"] for s in complete)
    # Look up children from the raw rows, since report order is not a contract.
    if path == ("iteration",) or path == ("other_parent",):
        children = [s for s in spans if s["name"] == "compute" and any(
            p["pid"] == s["pid"] and p["tid"] == s["tid"]
            and p["start_ns"] <= s["start_ns"] < p["end_ns"] for p in complete)]
    elif path == ("worker",):
        children = sorted([s for s in spans if s["name"] == "recursive"],
                          key=lambda s: s["start_ns"])[:1]
    elif path == ("worker", "recursive"):
        children = sorted([s for s in spans if s["name"] == "recursive"],
                          key=lambda s: s["start_ns"])[1:]
    else:
        children = []
    self_ns = inclusive - sum(s["end_ns"] - s["start_ns"] for s in children)
    assert report["inclusive_ns"] == inclusive, (path, report, inclusive)
    assert report["self_ns"] == self_ns, (path, report, self_ns)
    selected = sample_count(complete)
    assert report["samples"] == selected, (path, report, selected)
    if path == ("iteration",):
        assert 0 < selected < len(samples), "recording did not exercise sample exclusion"
        assert len({(s["pid"], s["tid"]) for s in complete}) == 2, complete
    assert report["intervals"] == len(complete), report
    check_balance(report["balance"])
    scopes = report["scopes"]
    assert {s["kind"] for s in scopes} == (
        {"time", "thread", "invocation"} if complete else {"time", "thread"}
    ), report
    for scope in scopes:
        invocations = complete
        if scope["kind"] == "time":
            assert scope["range"] is not None, scope
        elif scope["kind"] == "thread":
            assert scope["disabled_threads"], scope
        else:
            instance = scope["invocation"]
            invocations = [s for s in complete if s["instance"] == instance["instance"]
                           and s["pid"] == instance["pid"] and s["tid"] == instance["tid"]]
            assert len(invocations) == 1, scope
        expected = sample_count(invocations, scope["range"], scope["disabled_threads"])
        assert scope["samples"] == expected, (path, scope, expected)
        if path == ("iteration",) and scope["kind"] == "thread":
            assert 0 < expected < selected, "thread filtering did not retain the other thread"
            surviving = {s["tid"] for s in complete} - set(scope["disabled_threads"])
            assert {row["thread_id"] for row in scope["balance"]} == surviving, scope
            for row in scope["balance"]:
                assert row["busy_fraction"] > unfiltered_balance[row["thread_id"]], scope
        check_balance(scope["balance"])
assert not groups, groups
print("  region hierarchy, timings, multi-thread scopes and CPU busy fractions agree")
