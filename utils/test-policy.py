#!/usr/bin/env python3
"""Enforces where unit tests may live, and what those crates may depend on.

Crates outside ALLOWED carry no tests at all: their coverage comes from
checks/ running the packaged binaries on real recordings. Crates inside it may
keep fast, pure unit tests, but must not be able to reach a recording, which is
what makes a hand-built pmu_counters table impossible rather than discouraged.
"""
import json
import pathlib
import subprocess
import sys

ALLOWED = {
    "miniperf-pmu-data",
    "mperf-data",
    "shmem",
    "miniperf-roofline-core",
    "event-import",
}
FORBIDDEN_FOR_ALLOWED = {"miniperf-store", "libduckdb-sys", "duckdb"}

root = pathlib.Path(__file__).resolve().parent.parent
metadata = json.loads(
    subprocess.run(
        ["cargo", "metadata", "--no-deps", "--format-version", "1"],
        cwd=root, capture_output=True, text=True, check=True,
    ).stdout
)

problems = []
for package in metadata["packages"]:
    name = package["name"]
    manifest = pathlib.Path(package["manifest_path"])
    crate = manifest.parent
    if crate.is_relative_to(root / "third_party"):
        continue

    dependencies = {dep["name"] for dep in package["dependencies"]}
    if name in ALLOWED:
        reachable = dependencies & FORBIDDEN_FOR_ALLOWED
        if reachable:
            problems.append(
                f"{name} may keep unit tests, so it must not depend on "
                f"{', '.join(sorted(reachable))}"
            )
        continue

    if (crate / "tests").is_dir():
        problems.append(f"{name} has a tests/ directory; move it to checks/")
    for target in package["targets"]:
        if "test" in target["kind"] and target["test"]:
            problems.append(f"{name} target `{target['name']}` still has test = true")
    for source in crate.rglob("src/**/*.rs"):
        text = source.read_text(encoding="utf-8", errors="replace")
        if "#[cfg(test)]" in text:
            problems.append(f"{source.relative_to(root)} has #[cfg(test)]")

if problems:
    print("test policy violations:", file=sys.stderr)
    for problem in sorted(problems):
        print(f"  {problem}", file=sys.stderr)
    sys.exit(1)
print(f"  test policy ok ({len(ALLOWED)} crates may keep unit tests)")
