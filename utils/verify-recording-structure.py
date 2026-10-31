#!/usr/bin/env python3
"""Pin logical tables, schemas and values across a recording-reader refactor."""
import argparse
import json
import pathlib
import subprocess
import sys


def query(binary, directory, sql):
    result = subprocess.run(
        [str(binary), 'query', '-f', 'json', '--max-rows', '10000', str(directory), sql],
        check=True, capture_output=True, text=True,
    )
    payload = json.loads(result.stdout)
    if payload['truncated']:
        raise ValueError(f'query was truncated: {sql}')
    return payload


def capture(binary, directories):
    snapshots = {}
    for directory in directories:
        tables = query(binary, directory,
                       'SELECT view_name FROM duckdb_views() WHERE NOT internal ORDER BY view_name')
        schema = query(binary, directory,
                       "SELECT table_name, column_name, data_type, ordinal_position "
                       "FROM information_schema.columns WHERE table_schema = 'main' "
                       "AND table_name IN (SELECT view_name FROM duckdb_views() WHERE NOT internal) "
                       'ORDER BY table_name, ordinal_position')
        values = {}
        for row in tables['rows']:
            name = row['view_name']
            quoted = '"' + name.replace('"', '""') + '"'
            # Both aggregates are insensitive to row ordering. Count protects
            # multiplicity; the two hash aggregates detect changed row values.
            values[name] = query(binary, directory,
                                 'SELECT COUNT(*) AS rows, bit_xor(hash(t)) AS checksum, '
                                 'CAST(SUM(CAST(hash(t) AS HUGEINT)) AS VARCHAR) AS hash_sum '
                                 f'FROM {quoted} t')
        snapshots[str(directory)] = {'tables': tables, 'schema': schema, 'values': values}
    return snapshots


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('mode', choices=['capture', 'compare'])
    parser.add_argument('--mperf', type=pathlib.Path, required=True)
    parser.add_argument('--baseline', type=pathlib.Path, required=True)
    parser.add_argument('recordings', nargs='+', type=pathlib.Path)
    args = parser.parse_args()
    result = capture(args.mperf.resolve(), [path.resolve() for path in args.recordings])
    if args.mode == 'capture':
        args.baseline.write_text(json.dumps(result, indent=2, sort_keys=True) + '\n', encoding='utf-8')
        print(f'Pinned {len(result)} real recordings')
        return
    baseline = json.loads(args.baseline.read_text(encoding='utf-8'))
    if result != baseline:
        actual_path = args.baseline.with_suffix('.actual.json')
        actual_path.write_text(json.dumps(result, indent=2, sort_keys=True) + '\n', encoding='utf-8')
        raise SystemExit(f'recording behavior changed; compare {args.baseline} with {actual_path}')
    count = sum(len(recording['values']) for recording in result.values())
    print(f'PASS: table names, schemas, query envelopes and row-value fingerprints match for {count} tables in {len(result)} real recordings')


if __name__ == '__main__':
    main()
