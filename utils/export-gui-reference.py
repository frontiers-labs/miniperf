"""Export SQL references beside real recordings for viewer-only GUI packages."""
import json
import pathlib
import subprocess
import sys


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
