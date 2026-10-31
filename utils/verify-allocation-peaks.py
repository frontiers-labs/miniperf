"""Compare viewer allocation sites with operation-ordered recording SQL."""
import json
import pathlib
import sys

reference_path, tour = sys.argv[1:]
reference = json.loads(pathlib.Path(reference_path).read_text())
assert reference["schema_version"] == 1, "unsupported GUI reference schema"
expected_rows = reference["allocation_sites"]
if expected_rows is None:
    sys.exit(0)
lines = pathlib.Path(tour).read_text().splitlines()
assert "tour: rendered Allocations" in lines, "Allocations tab was not rendered"
assert any(line.startswith("tour: allocation_data ") for line in lines), \
    "viewer did not report allocation data"
actual_rows = [json.loads(line.removeprefix("tour: allocation_site "))
               for line in lines if line.startswith("tour: allocation_site ")]
expected = {row["stack_id"]: row for row in expected_rows}
assert len(expected) == len(expected_rows), "reference reported duplicate allocation sites"
actual = {row["stack_id"]: row for row in actual_rows}
assert len(actual) == len(actual_rows), "viewer reported duplicate allocation sites"
assert actual.keys() == expected.keys(), (
    "allocation site inventory differs", actual.keys() ^ expected.keys())
for stack, row in expected.items():
    stats = {key: actual[stack][key] for key in row}
    assert stats == row, ("allocation site differs", row, actual[stack])
print(f"  {len(expected)} allocation site(s) agree with operation-ordered SQL")
