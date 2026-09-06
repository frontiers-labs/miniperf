# Answer questions with SQL

In this tutorial we use `mperf query` to answer questions the viewers do not ask: which functions regressed between two builds, how a metric changed over a run, and where the samples inside one function landed. Every recording is a set of Parquet tables, and DuckDB is the calculator.

Use any two `tma` recordings of the same program. Below, `before` and `after` are the two matmul builds from the [top-down tutorial](topdown.md).

## Find the tables

```sh
mperf query after "SELECT view_name AS name FROM duckdb_views() WHERE NOT internal ORDER BY name"
mperf query after "PRAGMA table_info('tma')"
```

The second query lists the columns of `tma`, including one per top-down metric for this CPU.

## Rank hotspots with their stall reasons

```sh
mperf query after '
  SELECT func_name, total, ipc, be_bound, retiring
  FROM tma
  WHERE total > 0.01
  ORDER BY total DESC'
```

Filter on `total` rather than `LIMIT`: a function with 0.1 % of samples has meaningless top-down fractions.

## Compare two recordings

Tables are files, so a query can read another recording directly:

```sh
mperf query after "
  SELECT a.func_name,
         b.cycles AS cycles_before,
         a.cycles AS cycles_after,
         round(b.cycles::DOUBLE / a.cycles, 1) AS speedup
  FROM tma a
  JOIN read_parquet('before/tma.parquet') b USING (func_name)
  WHERE b.total > 0.01
  ORDER BY cycles_before DESC"
```

```
┌────────────────┬───────────────┬──────────────┬─────────┐
│   func_name    ┆ cycles_before ┆ cycles_after ┆ speedup │
╞════════════════╪═══════════════╪══════════════╪═════════╡
│ multiply_naive ┆ 2,914,499,132 ┆  216,985,532 ┆    13.4 │
└────────────────┴───────────────┴──────────────┴─────────┘
```

Absolute cycles compare across runs. Shares do not, because the total changed.

## See a metric over time

`tma_intervals` has every metric per second. A program with phases shows them here:

```sh
mperf query after "
  SELECT (start_ns - min(start_ns) OVER ()) / 1e9 AS second, round(value, 3) AS be_bound
  FROM tma_intervals
  WHERE metric = 'be_bound'
  ORDER BY start_ns"
```

Pivot several metrics into columns with a conditional aggregate:

```sh
mperf query after "
  SELECT (start_ns - min(start_ns) OVER ()) / 1e9 AS second,
         max(CASE WHEN metric = 'retiring' THEN value END) AS retiring,
         max(CASE WHEN metric = 'be_bound' THEN value END) AS be_bound
  FROM tma_intervals GROUP BY start_ns ORDER BY start_ns"
```

## Find the hot instructions

The assembly tables hold per-address sample counts for hot functions:

```sh
mperf query after "
  SELECT address, samples, cycles,
         round(100.0 * cycles / sum(cycles) OVER (PARTITION BY func_name), 1) AS pct
  FROM assembly_address_stats
  WHERE func_name = 'multiply_naive'
  ORDER BY cycles DESC LIMIT 5"
```

```
┌────────────────────┬─────────┬───────────────┬──────┐
│       address      ┆ samples ┆     cycles    ┆  pct │
╞════════════════════╪═════════╪═══════════════╪══════╡
│ 94,640,948,797,914 ┆     725 ┆ 2,474,226,190 ┆ 84.9 │
│ 94,640,948,797,918 ┆     128 ┆   428,528,632 ┆ 14.7 │
│ 94,640,948,797,930 ┆       3 ┆     7,916,488 ┆  0.3 │
│ 94,640,948,797,892 ┆       1 ┆     3,827,822 ┆  0.1 │
└────────────────────┴─────────┴───────────────┴──────┘
```

Two instructions take 99.6 % of the function's cycles. In the naive matrix multiply they are the load of `b[k][j]` and the multiply-add that waits for it.

Join `assembly_lines` on `module_path` and `runtime_address` to see the instruction text and source line for each address.

## Check what the recording could measure

Before trusting any number, two queries:

```sh
mperf query after 'SELECT rung, status, reason FROM capture_fidelity'
mperf query after "SELECT name, status, message FROM snapshot_collectors WHERE status <> 'available'"
```

The first says which hardware mechanism produced the top-down metrics. The second lists the collectors that did not run and why.

## Script it

JSON output has a fixed envelope, so a script can check for truncation and read typed values:

```sh
mperf query --format json --max-rows 1000 after 'SELECT func_name, total FROM tma' \
  | jq -e '.truncated == false and (.rows | length) > 0' > /dev/null && echo complete
```

For a long query, keep it in a file and run it against each recording:

```sh
for dir in results/*/; do
  echo "== $dir"
  mperf query --file hot.sql "$dir"
done
```

## Use your own tools

The same files open anywhere Parquet does:

```python
import duckdb
con = duckdb.connect()
print(con.sql("SELECT func_name, total FROM 'after/tma.parquet' ORDER BY total DESC LIMIT 5"))
```

Tables written during the run are split into segments named `<table>-<pid>-<n>.parquet`. Read them with a glob, for example `'after/samples-*.parquet'`.
