# Command-line reference

`mperf` has eight subcommands. Every command exits with status 1 on error and prints the error chain to stderr. Usage errors from the argument parser exit with status 2. `mperf events-export` is the exception: it prints failures to stderr and still exits 0.

A workload always follows a bare `--`. Everything after it belongs to the workload, including options that start with a hyphen.

```
mperf <command> [options] -- <program> [args...]
```

## mperf list

Print every counter the current host supports, one per line, as `name - description`.

No options.

## mperf doctor

Diagnose the host's profiling readiness. Prints one row per check with a status, a severity, and the action that fixes it.

No options.

Severity values are `blocker`, `degraded`, `info`, and `ok`. If any check is a blocker, the command prints `N blocker(s) found` and exits with status 1. Otherwise it prints `no blockers found`.

The checks, in the order they print:

| Check | Blocker when | Degraded when |
|---|---|---|
| `perf_event_paranoid` | level above 2 | level 1 (no kernel samples) or 2 (no kernel samples, no system-wide events) |
| `hardware counters` | `cpu-cycles` cannot be opened | |
| `kernel symbols (kptr_restrict)` | | `kptr_restrict` hides kernel addresses |
| `NMI watchdog` | | enabled, since it holds one hardware counter |
| `bpftrace` | not installed | |
| `eBPF collection (snapshot)` | kernel BTF missing at `/sys/kernel/btf/vmlinux`, or not running as root | |
| mechanism rows | | a hardware mechanism this CPU could have is unavailable |
| `objdump (disassembly)` | | not installed, so the assembly view in `mperf show` is unavailable |
| `debuginfod-find` | | `DEBUGINFOD_URLS` is set but the tool is missing |

The mechanism rows depend on the host: `precise sampling (Intel PEBS)`, `precise sampling (AMD IBS)`, `precise sampling (Arm SPE)`, `fixed topdown (PERF_METRICS)`, `topdown (Arm pmuv3 slots)`, `branch records (LBR call stacks)`, `uncore memory bandwidth`, and `baseline counters`.

## mperf stat

Count events for a command or a running process and print a table.

```
mperf stat [-p PID] [-e EVENTS] [--topdown [-l LEVEL]] -- <program> [args...]
```

| Option | Value | Default | Meaning |
|---|---|---|---|
| `-p`, `--pid` | process id | none | Count events of an existing process. With a command, the command only defines the measurement window. Without a command, counting runs until the process exits. |
| `-e`, `--event` | comma-separated names, repeatable | the default set below | Counters or metrics to count. Names match case-insensitively. A metric name expands to the counters its formula needs. |
| `--topdown` | flag | off | Show the host's top-down tree instead of the flat table. Overrides `-e`. |
| `-l`, `--level` | integer | 1 | Deepest top-down level to show. Ignored without `--topdown`. |

`stat` requires either a command or `--pid`. Without both it fails with:

```
stat requires a command, or --pid with a command used as the measurement duration
```

The default counter set is `cycles`, `instructions`, `llc_references`, `llc_misses`, `branch_misses`, `branches`, `stalled_cycles_backend`, `stalled_cycles_frontend`, `cpu_clock`, `cpu_migrations`, `page_faults`, and `context_switches`, plus every host metric whose inputs are all present.

A counter the PMU does not support prints a notice on stderr and is dropped. Counting continues with the rest:

```
notice: L1D.REPLACEMENT is not supported by this PMU; omitting it
```

An unknown name is an error:

```
unknown event or metric 'foo'; run `mperf list` to see supported names
```

The flat table has the columns `Counter`, `Value`, `Info`, `Scaling`, and `Description`. `Info` carries derived numbers: instructions per cycle for `instructions` and `branches`, misses per thousand instructions for miss counters, and stall percentages. `Scaling` is the multiplexing factor. A value of 1.00 means the counter was on the hardware the whole time. Derived metrics show `derived` in `Info` and `-` in `Scaling`. On a heterogeneous CPU the command prints one table per core cluster followed by a summed total.

Top-down output prints one row per metric, indented by tree level, with the value in percent. The `*` marker names the dominant path at the requested level.

## mperf record

Record a sampling profile into a new directory.

```
mperf record -s SCENARIO -o DIR [options] -- <program> [args...]
mperf record -s snapshot -o DIR -p PID [--duration D]
```

| Option | Value | Default | Meaning |
|---|---|---|---|
| `-s`, `--scenario` | `snapshot`, `tma`, `mem`, `roofline` | required | What to collect. See [Scenarios](../guide/scenarios.md). |
| `-o`, `--output-directory` | path | required | Directory to create. Must not exist. |
| `-p`, `--pid` | process id | none | Attach to a running process tree. Only `snapshot` supports it. `mem` rejects it. |
| `--duration` | `250ms`, `10s`, `2m`, `1h`, or bare seconds | run to exit | Stop a snapshot after this long. Other scenarios ignore it. |
| `--roofline-backend` | `auto`, `compiler`, `qemu`, `dynamorio` | `auto` | Accounting method for `roofline` and `mem`. |
| `--qemu` | path | auto | QEMU user-mode binary. Otherwise `MPERF_QEMU`, then the bundle next to `mperf`, then `PATH`. |
| `--qemu-plugin` | path | auto | The miniperf QEMU plugin shared library. |
| `--qemu-arg` | string, repeatable | none | Extra argument passed to QEMU before the guest executable. May start with a hyphen. |
| `--dynamorio` | path | auto | The `drrun` launcher or a DynamoRIO build or bundle directory. |
| `--dynamorio-client` | path | auto | The miniperf DynamoRIO client shared library. |

An existing output directory is refused:

```
Error: profiling results must be put in different directories

Caused by:
    'results' already exists
```

The backend options are validated before anything runs:

- `--roofline-backend` and the QEMU and DynamoRIO options are valid only with `roofline` or `mem`.
- The QEMU and DynamoRIO options cannot be combined with `--roofline-backend compiler`.
- `--dynamorio` and `--dynamorio-client` cannot be combined with `--roofline-backend qemu`.
- `--qemu`, `--qemu-plugin`, and `--qemu-arg` cannot be combined with `--roofline-backend dynamorio`.

With `auto`, a DynamoRIO accounting failure retries with QEMU and prints a warning. An explicit backend never falls back.

When `--duration` expires on a launched command, the process tree receives `SIGTERM`, then `SIGKILL` two seconds later.

Sampling runs at 99 Hz for `snapshot` and 1000 Hz for the other scenarios.

Per-scenario errors:

| Scenario | Error |
|---|---|
| `snapshot` | `record snapshot requires a command or --pid` |
| `tma` | `TMA is not supported on this CPU` |
| `tma` | `TMA needs hardware counters this host cannot open (...); run mperf doctor, or use record -s snapshot` |
| `mem` | `record mem requires a command and does not support --pid` |
| `mem` | `the mem scenario requires address accounting; install DynamoRIO with the miniperf client, or a plugin-enabled QEMU and the miniperf QEMU plugin` |
| `mem` | `the mem scenario requires a native executable for trustworthy timing` |
| `roofline` | `record roofline requires a command` |

## mperf show

Open a recording in the terminal viewer.

```
mperf show DIR
```

No options. See [View results in the terminal](../guide/show.md) for the tabs and keys.

## mperf query

Run one read-only SQL statement against a recording.

```
mperf query [-f FORMAT] [--max-rows N] DIR 'SQL'
mperf query [-f FORMAT] [--max-rows N] --file PATH DIR
mperf query --file - DIR < query.sql
mperf query help
```

| Option | Value | Default | Meaning |
|---|---|---|---|
| `-f`, `--format` | `text`, `json` | `text` | Output format. |
| `--file` | path or `-` | none | Read SQL from a file, or from stdin with `-`. Cannot be combined with inline SQL. |
| `--max-rows` | 1 to 10000 | 50 | Maximum rows emitted, independent of any SQL `LIMIT`. |

`mperf query help` prints the built-in guide with the dataset list and worked examples.

Accepted statements are `SELECT`, `VALUES`, `WITH ... SELECT`, `EXPLAIN`, and read-only `PRAGMA`. One statement per call. Writes and statement lists are rejected. Every selected expression needs a unique name, so alias computed columns with `AS`.

Text output is a table followed by `N rows`, or `N rows shown (truncated at --max-rows N)`. JSON output is one object:

```json
{
  "schema_version": 1,
  "record_format_version": 3,
  "columns": [{ "name": "func_name", "type": "string" }],
  "rows": [["main"]],
  "row_count": 1,
  "max_rows": 50,
  "truncated": false
}
```

Column types are `integer`, `number`, `string`, `binary`, `mixed`, or `null`. JSON values are unformatted. See [Database schema](schema.md) for the tables.

## mperf events-export

Print every recorded event as a JSON array on stdout, sorted by timestamp.

```
mperf events-export DIR
```

No options. Errors print to stderr as `failed to export events: ...` and the exit status stays 0, so check stderr in scripts.

## mperf recover

Validate a session directory after a crash and quarantine segments that were left without a footer.

```
mperf recover DIR
```

No options. Prints `N healthy segment(s)`, one `quarantined PATH` line per damaged segment, or `no crash-damaged segments found`. This command modifies the directory.
