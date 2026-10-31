"""Compare viewer allocation sites with operation-ordered recording SQL."""
import json
import pathlib
import subprocess
import sys

mperf, recording, tour = sys.argv[1:]
inventory = subprocess.run(
    [mperf, "query", "-f", "json", recording,
     "SELECT COUNT(*) > 0 AS present FROM information_schema.tables "
     "WHERE table_name = 'allocation_summary'"],
    check=True, capture_output=True, text=True,
)
if not json.loads(inventory.stdout)["rows"][0]["present"]:
    sys.exit(0)
lines = pathlib.Path(tour).read_text().splitlines()
assert "tour: rendered Allocations" in lines, "Allocations tab was not rendered"
assert any(line.startswith("tour: allocation_data ") for line in lines), \
    "viewer did not report allocation data"
actual_rows = [json.loads(line.removeprefix("tour: allocation_site "))
               for line in lines if line.startswith("tour: allocation_site ")]
sql = """
WITH changes AS (
    SELECT stack_id, allocated_ns AS time, allocation_id AS sequence,
           requested_bytes::BIGINT AS delta
    FROM allocations
    UNION ALL
    SELECT stack_id, freed_ns, freed_sequence, -requested_bytes::BIGINT
    FROM allocations WHERE freed_ns IS NOT NULL
), live AS (
    SELECT stack_id,
           SUM(delta) OVER (
               PARTITION BY stack_id ORDER BY time, sequence
               ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW
           ) AS bytes
    FROM changes
), peaks AS (
    SELECT stack_id, MAX(bytes)::UBIGINT AS peak_bytes
    FROM live GROUP BY stack_id
), totals AS (
    SELECT stack_id, COUNT(*) AS count,
           SUM(requested_bytes)::UBIGINT AS requested_bytes,
           SUM(CASE WHEN freed_ns IS NULL THEN requested_bytes ELSE 0 END)::UBIGINT
               AS live_bytes
    FROM allocations GROUP BY stack_id
)
SELECT stack_id, count, requested_bytes, live_bytes, peak_bytes
FROM totals JOIN peaks USING (stack_id)
ORDER BY stack_id
"""
result = subprocess.run(
    [mperf, "query", "-f", "json", "--max-rows", "10000", recording, sql],
    check=True, capture_output=True, text=True,
)
reference = json.loads(result.stdout)
assert not reference["truncated"], "allocation site reference query was truncated"
expected = {row["stack_id"]: row for row in reference["rows"]}
actual = {row["stack_id"]: row for row in actual_rows}
assert len(actual) == len(actual_rows), "viewer reported duplicate allocation sites"
assert actual.keys() == expected.keys(), (
    "allocation site inventory differs", actual.keys() ^ expected.keys())
for stack, row in expected.items():
    stats = {key: actual[stack][key] for key in row}
    assert stats == row, ("allocation site differs", row, actual[stack])
print(f"  {len(expected)} allocation site(s) agree with operation-ordered SQL")
