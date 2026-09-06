# Query results with SQL

```sh
mperf query results/tma 'SELECT func_name, total, ipc FROM tma ORDER BY total DESC LIMIT 10'
```

A recording is a directory of Parquet tables. `mperf query` opens it in an in-memory DuckDB database with one view per table and runs one read-only statement. It is the tool for questions the viewers do not ask, for scripts, and for comparing runs.

## Statements

One statement per call. `SELECT`, `VALUES`, `WITH ... SELECT`, `EXPLAIN`, and read-only `PRAGMA` are accepted, with the full DuckDB dialect: joins, window functions, aggregates, `QUALIFY`, list functions. Anything that writes is rejected, and so is a second statement after a semicolon. Semicolons inside strings and comments are fine.

Every selected expression needs a distinct name:

```
duplicate result column 'value'; give every selected expression a unique AS alias
```

Quote the SQL so the shell leaves `*`, `<`, `>`, and parentheses alone. For anything longer than a line, put it in a file:

```sh
mperf query --file hot.sql results/tma
mperf query --file - results/tma < hot.sql
```

## Output

Text output is a table followed by a row count. Columns named `total`, `branch_miss_rate`, and `cache_miss_rate` print as percentages, `ipc` and the MPKI columns with two decimals, integers with thousands separators, and top-down metric columns as percentages. Other floats print with six decimals.

The default cap is 50 rows regardless of any `LIMIT` in the SQL. Raise it up to 10000:

```sh
mperf query --max-rows 500 results/tma 'SELECT * FROM proc_map'
```

For scripts, ask for JSON. Values are unformatted, and the envelope tells you whether the cap cut the result:

```sh
mperf query --format json results/tma 'SELECT metric, value FROM tma_summary' | jq '.rows[] | select(.value > 0.5)'
```

```json
{
  "schema_version": 1,
  "record_format_version": 3,
  "columns": [{ "name": "metric", "type": "string" }, { "name": "value", "type": "number" }],
  "rows": [{ "metric": "be_bound", "value": 0.906134 }],
  "row_count": 1,
  "max_rows": 50,
  "truncated": false
}
```

Diagnostics go to stderr and results to stdout, so a pipeline sees clean JSON.

## Discover the schema

```sh
mperf query results/tma "SELECT view_name AS name FROM duckdb_views() WHERE NOT internal ORDER BY name"
mperf query results/tma "PRAGMA table_info('tma')"
```

`mperf query help` prints the built-in guide with the common tables and worked examples. [Database schema](../reference/schema.md) documents every table and column.

## Compare two recordings

Every table is a file named `<table>.parquet` under the recording directory, so DuckDB's file functions reach across recordings:

```sh
mperf query results/after "
  SELECT b.func_name, b.total AS before, a.total AS after
  FROM read_parquet('results/before/hotspots.parquet') b
  JOIN hotspots a USING (func_name)
  ORDER BY before DESC LIMIT 10"
```

Tables written during the run, such as `samples` and `events`, may be split into several files named `<table>-<pid>-<n>.parquet`. Pass a glob to `read_parquet` to read them as one.

## Use other tools

Nothing in a recording is specific to miniperf's reader. Open the same files with the DuckDB shell, pandas, polars, or pyarrow:

```python
import polars as pl
hot = pl.read_parquet("results/tma/tma.parquet")
print(hot.sort("total", descending=True).head())
```

## Export every event

`mperf events-export` dumps all samples and trace events as a JSON array sorted by time, for tools that want the raw stream:

```sh
mperf events-export results/tma > events.json
```

It exits 0 even when it fails, so check stderr in scripts.
