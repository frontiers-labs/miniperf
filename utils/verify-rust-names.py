#!/usr/bin/env python3
"""Check labels, full identities, and inline resolution from a compiled Rust fixture."""
import csv
import json
import sys

if sys.argv[1] == "--folded":
    with open(sys.argv[2]) as source:
        lines = source.read().splitlines()
    assert lines[0] == "# miniperf-folded-names-percent-v1", "folded name encoding is unmarked"
    with open(sys.argv[3], newline="") as source:
        expected = {row[1] for row in csv.reader(source, delimiter="\t") if "rust_names" in row[1] and "::generic" in row[1]}
    decoded = set()
    for line in lines[1:]:
        stack, count = line.rsplit(" ", 1)
        assert int(count) > 0
        for name in stack.split(";"):
            decoded.add(name.replace("%3B", ";").replace("%0A", "\n").replace("%0D", "\r").replace("%25", "%"))
    assert expected <= decoded, f"folded names lost complete generic frames: {expected - decoded}"
    print("verified three complete generic frames across the folded delimiter boundary")
    sys.exit(0)

if sys.argv[1] == "--tour":
    frames, inspections = {}, {}
    with open(sys.argv[2]) as source:
        for line in source:
            for prefix, target in [("tour: function_name ", frames), ("tour: function_inspection ", inspections)]:
                if line.startswith(prefix):
                    row = json.loads(line[len(prefix):])
                    target[row["id"]] = row
    generic = [row for row in frames.values() if "rust_names" in row["full"] and "::generic" in row["full"]]
    assert len(generic) == 3, f"expected three independently selectable generic instances: {generic}"
    assert len({row["full"] for row in generic}) == 3, "full generic names collapsed"
    assert len({row["friendly"] for row in generic}) == 1, "generic labels were not abbreviated consistently"
    for frame in generic:
        result = inspections[frame["id"]]
        assert result["full"] == frame["full"]
        assert result["full_search"] and result["friendly_search"], result
        assert result["source"].endswith("rust_names.rs") and result["instructions"] > 0, result
    print("verified viewer search and source/assembly inspection for three separate monomorphizations")
    sys.exit(0)

with open(sys.argv[1], newline="") as source:
    rows = list(csv.reader(source, delimiter="\t"))
assert rows, "no fixture symbols resolved"
for raw, full, friendly, depth, file, line in rows:
    assert raw and full and friendly, "lost symbol spelling"
    if full.startswith("rust_names"):
        assert "rust_names" in friendly
        assert "::h" not in friendly, friendly

names = {row[1] for row in rows}
generics = {row[1] for row in rows if "rust_names" in row[1] and "::generic" in row[1]}
assert len(generics) >= 3, generics
assert all(row[2] == "rust_names::generic::<…>" for row in rows if row[1].startswith("rust_names[") and "::generic" in row[1]), "v0 generic arguments not elided"
if any(row[0].startswith("_R") and row[1].startswith("rust_names[") for row in rows):
    assert any("alloc" in name and "Option" in name and "Result" in name for name in generics), "nested arguments missing from full name"
assert any("Worker" in row[2] and " as " in row[2] and "Run" in row[2] for row in rows if "Run" in row[1] and "::run" in row[1]), "trait receiver context lost"
same_labels = {row[2] for row in rows if "left::same" in row[1] or "right::same" in row[1]}
assert same_labels == {"rust_names::left::same", "rust_names::right::same"}, same_labels
assert any("closure" in name for name in names)
assert any("inline_work" in row[1] and row[4].endswith("rust_names.rs") and int(row[5]) > 0 for row in rows), "no inline callee source"
assert any(int(row[3]) > 0 for row in rows), "inline caller lost"
assert any(row[1] == row[2] == "plain_c_name" for row in rows), "non-Rust name changed"
print(f"verified {len(rows)} resolved fixture frames and {len(generics)} generic identities")
