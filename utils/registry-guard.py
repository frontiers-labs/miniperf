#!/usr/bin/env python3
"""checks/ and xtask's CHECKS must name the same set of checks."""
import pathlib
import re
import sys

root = pathlib.Path(__file__).resolve().parent.parent
registry = (root / "xtask/src/registry.rs").read_text()

declared = set(re.findall(r'name:\s*"([a-z0-9-]+)"', registry))
declared |= set(re.findall(r'lint\("([a-z0-9-]+)"\)', registry))

on_disk = {
    path.name
    for path in (root / "checks").iterdir()
    if path.is_file() and path.name not in {"run", "lib.sh"}
}

missing_script = sorted(declared - on_disk)
missing_entry = sorted(on_disk - declared)
problems = []
if missing_script:
    problems.append(f"declared in the registry with no script: {', '.join(missing_script)}")
if missing_entry:
    problems.append(f"in checks/ with no registry entry: {', '.join(missing_entry)}")

# A GUI check with no producer of recordings can never have an input.
if 'Needs::Recordings' in registry and 'records: true' not in registry:
    problems.append("a Needs::Recordings check exists but no check has records: true")

if problems:
    print("registry drift:", file=sys.stderr)
    for problem in problems:
        print(f"  {problem}", file=sys.stderr)
    sys.exit(1)
print(f"  registry ok ({len(declared)} checks)")
