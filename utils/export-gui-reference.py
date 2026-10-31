"""Export SQL references beside real recordings for viewer-only GUI packages."""
import json
import pathlib
import subprocess
import sys


ALLOCATION_SITES_SQL = """
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


def query(mperf, recording, sql):
    result = subprocess.run(
        [mperf, "query", "-f", "json", "--max-rows", "10000", str(recording), sql],
        capture_output=True, text=True,
    )
    if result.returncode:
        raise RuntimeError(f"reference query failed:\n{result.stdout}{result.stderr}")
    document = json.loads(result.stdout)
    if document["truncated"]:
        raise ValueError(f"GUI reference query was truncated: {sql}")
    return document["rows"]


def export(mperf, recording):
    samples = query(mperf, recording,
                    "SELECT COUNT(*) > 0 AS has_samples FROM pmu_counters")
    reference = {"schema_version": 1, "has_samples": bool(samples[0]["has_samples"])}
    tables = query(mperf, recording,
                   "SELECT table_name FROM information_schema.tables")
    reference["allocation_sites"] = (
        query(mperf, recording, ALLOCATION_SITES_SQL)
        if any(row["table_name"] == "allocation_summary" for row in tables) else None
    )
    destination = recording / "gui-check.json"
    temporary = destination.with_suffix(".json.tmp")
    temporary.write_text(json.dumps(reference, indent=2) + "\n")
    temporary.replace(destination)
    print(f"  exported GUI references for {recording}")


def main():
    mperf, root = sys.argv[1:]
    failed = False
    for info in sorted(pathlib.Path(root).rglob("info.json")):
        try:
            export(mperf, info.parent)
        except (OSError, RuntimeError, ValueError) as error:
            print(f"FAIL: {info.parent}: {error}", file=sys.stderr)
            failed = True
    return int(failed)


if __name__ == "__main__":
    sys.exit(main())
