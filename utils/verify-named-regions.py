"""Verify the known workload through the viewer's production tour and SQL API."""
import json
import pathlib
import sys

prefix = sys.argv[1]
spans = json.loads(pathlib.Path(prefix + ".spans").read_text())["rows"]
samples = json.loads(pathlib.Path(prefix + ".samples").read_text())["rows"]
lines = pathlib.Path(prefix + ".tour").read_text().splitlines()
reports = [json.loads(line.removeprefix("tour: region "))
           for line in lines if line.startswith("tour: region ")]
assert "tour: rendered Timeline" in lines, "Timeline was not rendered"
assert "tour: regions nodes=8 completed=13 incomplete=1" in lines, lines
assert len(reports) == 8, reports

# These paths come from the workload's source, independently of the viewer's
# inferred parents. Each recursive call encloses exactly one deeper call.
expected_counts = {
    ("iteration",): 4,
    ("iteration", "compute"): 4,
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
            p["start_ns"] <= s["start_ns"] < p["end_ns"] for p in complete)]
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
    assert report["intervals"] == len(complete), report
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
assert not groups, groups
print("  region hierarchy, timings, time/thread composition and invocation scopes agree")
