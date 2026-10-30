"""Verify the known workload through the viewer's production tour and SQL API."""
import json
import math
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
observations = read_rows("cpu")
lines = pathlib.Path(prefix + ".tour").read_text().splitlines()
reports = [json.loads(line.removeprefix("tour: region "))
           for line in lines if line.startswith("tour: region ")]
assert "tour: rendered Timeline" in lines, "Timeline was not rendered"
assert "tour: rendered Events" in lines, "Events was not rendered"
assert "tour: regions nodes=8 completed=15 incomplete=1" in lines, lines
assert len(reports) == 8, reports

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


def check_balance(actual, invocations, time, disabled_threads):
    ranges = {}
    for span in invocations:
        if span["tid"] in disabled_threads:
            continue
        start = max(time["start_ns"], span["start_ns"])
        end = min(time["end_ns"], span["end_ns"])
        if start < end:
            ranges.setdefault((span["pid"], span["tid"]), []).append((start, end))
    for key, intervals in ranges.items():
        merged = []
        for start, end in sorted(intervals):
            if merged and start <= merged[-1][1]:
                merged[-1] = (merged[-1][0], max(end, merged[-1][1]))
            else:
                merged.append((start, end))
        ranges[key] = merged
    coverage = {}
    for (_, tid), intervals in ranges.items():
        coverage[tid] = coverage.get(tid, 0) + sum(end - start for start, end in intervals)
    cpu_time = {}
    for observation in observations:
        tid = observation["thread_id"]
        intervals = ranges.get((observation["process_id"], tid), [])
        end = observation["timestamp"]
        weight = observation["weight_ns"]
        if observation["source"] == "sampled_occupancy":
            if not any(start <= end < stop for start, stop in intervals):
                continue
            attributed = weight
        else:
            start = observation["interval_start_ns"]
            if start is None:
                start = max(0, end - weight)
            overlap = sum(max(0, min(end, stop) - max(start, begin))
                          for begin, stop in intervals)
            if overlap == 0 or end <= start:
                continue
            attributed = weight * overlap / (end - start)
        cpu_time[tid] = cpu_time.get(tid, 0) + attributed
    expected_threads = set(cpu_time)
    expected_threads.update(sample["thread_id"] for sample in samples
                            if sample["thread_id"] not in disabled_threads
                            and any(start <= sample["timestamp"] < end
                                    for start, end in ranges.get(
                                        (sample["process_id"], sample["thread_id"]), [])))
    assert {row["thread_id"] for row in actual} == expected_threads, actual
    for row in actual:
        tid = row["thread_id"]
        expected = min(1.0, max(0.0, cpu_time.get(tid, 0) / coverage[tid]))
        assert math.isclose(row["busy_fraction"], expected, rel_tol=1e-8, abs_tol=1e-9), (
            tid, row, expected)


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
    check_balance(report["balance"], complete, report["recording_range"], ())
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
        check_balance(scope["balance"], invocations,
                      scope["range"] or report["recording_range"], scope["disabled_threads"])
assert not groups, groups
print("  region hierarchy, timings, multi-thread scopes and CPU busy fractions agree")
