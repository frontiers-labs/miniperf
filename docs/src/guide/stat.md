# Count events with mperf stat

`mperf stat` runs a command, counts hardware and software events for its whole lifetime, and prints a table. Use it when you want totals and ratios rather than a profile: instructions per cycle, cache misses per thousand instructions, or the top-down split of a run.

```sh
mperf stat -- ./workload --input data.bin
```

Everything after `--` is the workload's own command line.

## Read the table

```
Performance counter stats for './workload --input data.bin':

+-------------------------+---------------+-----------------+---------+------------------------------------------------------+
| Counter                 | Value         | Info            | Scaling | Description                                          |
+=================================================================================================================================+
| cycles                  |   229,954,339 |                 |    1.00 | Number of CPU cycles                                 |
| instructions            |   946,774,369 | 4.12 inst/cycle |    1.00 | Number of instructions retired                       |
| llc_misses              |       769,876 | 0.81 MPKI       |    2.00 | Last level cache misses                              |
| stalled_cycles_backend  |    50,777,514 | 22.08%          |    2.00 | Number of cycles stalled due to backend bottlenecks  |
...
```

`Value` is the count over the run. `Info` holds a derived figure where one is meaningful:

| Counter | Info | Colored yellow | Colored red |
|---|---|---|---|
| `instructions`, `branches` | per cycle | below 1.5 | below 0.6 |
| `llc_misses`, `branch_misses` | misses per thousand instructions | | |
| `stalled_cycles_*` | percent of cycles | above 10 % | above 20 % |

`Scaling` is the multiplexing factor. A CPU has a handful of programmable counters. When you ask for more events than fit, the kernel rotates groups on and off the hardware and miniperf scales each count by the fraction of time its group was running. A value of 1.00 is an exact count. A value of 2.00 means the counter ran half the time and the value shown is an estimate. Ask for fewer events when exactness matters.

Rows named `derived` are host metrics computed from the counters above them, with `-` in the scaling column.

## Choose events

Without `-e`, `stat` counts twelve events: `cycles`, `instructions`, `llc_references`, `llc_misses`, `branch_misses`, `branches`, `stalled_cycles_backend`, `stalled_cycles_frontend`, `cpu_clock`, `cpu_migrations`, `page_faults`, and `context_switches`.

To count something else, name the events. Run [`mperf list`](list.md) to see what this CPU offers:

```sh
mperf stat -e cycles,instructions,l2_cache_misses_from_dc_misses -- ./workload
```

Names match case-insensitively. A name may also be a host metric, in which case `stat` counts every event the metric's formula needs and prints the metric as a derived row.

An event this PMU does not support is dropped with a notice, and counting continues without it:

```
notice: L1D.REPLACEMENT is not supported by this PMU; omitting it
```

## Count a running process

Give a process id instead of a command to count until that process exits:

```sh
mperf stat -p 12345
```

Give both, and the command defines the measurement window while the counters watch the process:

```sh
mperf stat -p 12345 -- sleep 10
```

## Print the top-down tree

`--topdown` replaces the table with the host's top-down breakdown, and `-l` chooses how deep to go:

```sh
mperf stat --topdown -l 2 -- ./workload
```

```
Top-down analysis (tma)
* be bound                      90.61%  Fraction of cycles backend was out of resources
    memory bound                  94.07%  Backend pressure coincident with outstanding L2 fills
    core bound                    -3.46%  Backend pressure not coincident with outstanding L2 fills
  retiring                       4.02%  Fraction of cycles useful work completed
  bad speculation                0.12%  Fraction of cycles lost to branch misprediction
  fe bound                       0.01%  Fraction of cycles Fetch/Decode not supplied
* dominant path at requested level
```

The `*` marks the largest bucket at each level. Children are indented under their parent. A level-2 value is a fraction of its parent, so `memory bound 94.07%` means 94 % of the backend-bound cycles coincided with outstanding cache fills.

Small negative values appear when a level is computed by subtraction from saturating stall counters. Read them as zero. [Top-down microarchitecture analysis](../concepts/topdown.md) explains which CPUs compute these values from dedicated hardware and which use the counter-based formulas.

## Heterogeneous CPUs

On a big.LITTLE system each core cluster has its own PMU. `stat` opens every counter on every cluster and prints one table per cluster, followed by a summed total. Per-cluster values are raw on-cluster counts. See [Linux on Arm](../platforms/arm.md).
