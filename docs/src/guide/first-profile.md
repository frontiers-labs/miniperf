# Your first profile

This page takes ten minutes. You count events for a program, record a top-down profile, and read the result three ways.

## Count events

Pick any program that runs for at least a second. Put its command line after `--`:

```sh
mperf stat -- ./workload
```

```
Performance counter stats for './workload':

+-------------------------+---------------+-----------------+---------+------------------------------------------------------+
| Counter                 | Value         | Info            | Scaling | Description                                          |
+=================================================================================================================================+
| cycles                  | 2,945,454,930 |                 |    1.00 | Number of CPU cycles                                 |
| instructions            | 1,078,666,632 | 0.37 inst/cycle |    1.00 | Number of instructions retired                       |
| llc_references          |   289,425,415 |                 |    2.00 | Last level cache references                          |
| llc_misses              |   113,606,753 | 105.32 MPKI     |    2.00 | Last level cache misses                              |
| branch_misses           |       270,642 | 0.25 MPKI       |    2.00 | Branch instruction missess                           |
| branches                |   134,336,840 | 0.05 inst/cycle |    2.00 | Branch instructions retired                          |
| stalled_cycles_backend  | 2,724,990,753 | 92.52%          |    2.00 | Number of cycles stalled due to backend bottlenecks  |
| stalled_cycles_frontend |       450,007 | 0.02%           |    2.00 | Number of cycles stalled due to frontend bottlenecks |
| cpu_clock               |   748,878,460 |                 |    1.00 | A high-resolution per-CPU timer                      |
| cpu_migrations          |             0 |                 |    1.00 | Number of the times the process has migrated ...     |
| page_faults             |           574 |                 |    1.00 | Number of page faults                                |
| context_switches        |             0 |                 |    1.00 | Number of context switches                           |
+-------------------------+---------------+-----------------+---------+------------------------------------------------------+
```

The `Info` column does the arithmetic for you. This program retires 0.37 instructions per cycle and misses the last-level cache 105 times per thousand instructions. Both numbers print in red because they are far outside the healthy range. `Scaling` of 2.00 means the kernel time-shared those counters across two groups, and the value shown is scaled up to the full run.

## Record a top-down profile

Now find out which function is responsible. The `tma` scenario samples the program and attributes pipeline stalls to functions:

```sh
mperf record -s tma -o first-run -- ./workload
```

```
Record profile with TMA scenario
Capture fidelity: tma at 'counter_only' (fixed topdown: core PMU exposes no `slots` + `topdown-*` events (PERF_METRICS is Icelake and newer))
Postprocessing...
```

The `Capture fidelity` line tells you how the top-down metrics were obtained. `counter_only` means they were computed from programmable counters, because this CPU has no dedicated top-down hardware. On an Ice Lake or newer Intel core you would see `fixed_topdown`.

The output directory must not exist yet. `mperf record` refuses to overwrite a recording.

## Read the result

Open it in the terminal:

```sh
mperf show first-run
```

Press `Tab` to move between the Summary, Hotspots, and Flamegraph tabs, and `q` to quit. The Hotspots tab lists functions with their share of cycles and one column per top-down metric.

Or ask a question in SQL:

```sh
mperf query first-run 'SELECT func_name, total, cycles, instructions, ipc FROM tma ORDER BY total DESC LIMIT 3'
```

```
┌─────────────────────┬────────┬───────────────┬───────────────┬──────┐
│      func_name      ┆  total ┆     cycles    ┆  instructions ┆  ipc │
╞═════════════════════╪════════╪═══════════════╪═══════════════╪══════╡
│ multiply_naive      ┆ 99.99% ┆ 2,914,499,132 ┆ 1,068,428,237 ┆ 0.37 │
│ fill                ┆  0.01% ┆       268,423 ┆       685,382 ┆ 2.55 │
│ __tunable_get_val   ┆  0.00% ┆        83,513 ┆        88,341 ┆ 1.06 │
└─────────────────────┴────────┴───────────────┴───────────────┴──────┘
3 rows
```

Or open it in the desktop viewer, which adds a flame graph, a timeline, and a source and assembly view:

```sh
mperf-gui first-run
```

## Where to go next

- [Record a profile](record.md) covers the record options that apply to every scenario.
- [Scenarios](scenarios.md) helps you choose between `snapshot`, `tma`, `mem`, and `roofline`.
- The [tutorial on diagnosing a bottleneck](../tutorials/topdown.md) continues this investigation and fixes the program.
