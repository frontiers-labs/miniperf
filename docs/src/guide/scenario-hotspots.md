# Hotspots and top-down

```sh
mperf record -s tma -o results/tma -- ./workload
```

The `tma` scenario is the general-purpose CPU profile. It samples at 1000 Hz with call stacks and, in the same run, counts the events the host's top-down model needs. The result attributes both time and stall reasons to functions.

## What you get

- **Hotspots.** The `tma` table has one row per function with its share of cycles, cycles, instructions, IPC, and one column per top-down metric. `mperf show` presents it as the Hotspots tab.
- **Recording-wide verdict.** `tma_summary` gives each metric's value over the whole run and marks the largest as `dominant`.
- **Timeline.** `tma_intervals` gives every metric per second of the run, so you can see phases.
- **Flame graphs.** `flamegraph_cycles.svg` and `flamegraph_instructions.svg`, plus the folded text files they are rendered from.
- **Assembly.** Per-instruction sample counts for the hot functions, for the assembly view in `mperf show` and `mperf-gui`.
- **Precise memory samples,** when the CPU has PEBS or SPE. The `mem_samples`, `alloc_site_memory`, and `cacheline_contention` tables show which loads missed, how long they took, and which cache lines several threads fought over.

## Which top-down model runs

The metrics depend on the CPU. `mperf record` prints the choice on its second line and records it in `capture_fidelity`:

| Rung | CPUs | Levels |
|---|---|---|
| `fixed_topdown` | Intel Ice Lake and newer, non-hybrid | Level 1, plus level 2 on Sapphire Rapids and newer |
| `arm_slots_topdown` | Arm cores with the pmuv3 `slots` events | Level 1 |
| `counter_only` | everything else with a curated event table | Level 1 and 2 on Intel Tiger Lake, AMD Zen, and SpacemiT A100. Three levels on SpacemiT X100. Level 1 on Cortex-A720 and A520. |
| `counter_only`, architectural fallback | CPUs without a table | Three metrics from `stalled_cycles_frontend`, `stalled_cycles_backend`, and instructions, assuming a retire width of 4 |

If the host cannot open the counters the model needs, recording stops:

```
TMA needs hardware counters this host cannot open (...); run `mperf doctor`, or use `record -s snapshot`
```

[Top-down microarchitecture analysis](../concepts/topdown.md) explains what the levels mean and how to read them.

## Read the result

```sh
mperf query results/tma 'SELECT metric, value, verdict FROM tma_summary ORDER BY value DESC'
```

```
┌───────────────────────┬──────────┬──────────┐
│         metric        ┆   value  ┆  verdict │
╞═══════════════════════╪══════════╪══════════╡
│ be_bound              ┆ 0.906134 ┆ dominant │
│ retiring              ┆ 0.040225 ┆ NULL     │
│ fe_bound              ┆ 0.000138 ┆ NULL     │
│ bad_speculation       ┆ 0.001205 ┆ NULL     │
│ be_bound.core_bound   ┆ NULL     ┆ NULL     │
│ be_bound.memory_bound ┆ NULL     ┆ NULL     │
└───────────────────────┴──────────┴──────────┘
```

Values are fractions of cycles or of pipeline slots. A `NULL` level-2 value means the sample groups needed for that formula were not all present in the recording, which happens when a group had to be dropped for lack of counters.

Then find the functions responsible:

```sh
mperf query results/tma 'SELECT func_name, total, ipc, be_bound, retiring FROM tma ORDER BY total DESC LIMIT 5'
```

Metric columns in `tma` take the metric name with dots replaced by underscores, so `be_bound.memory_bound` is the column `be_bound_memory_bound`.

## When the numbers look wrong

Counter-based level-1 metrics can exceed 1.0 or go slightly negative on some CPUs, because the stall counters they use saturate rather than partition. On AMD Zen and SpacemiT cores the frontend and backend stall counters can both count the same cycle. On a loaded host with SMT siblings busy, the shared counters inflate further. Treat the ranking as the signal and the level-2 breakdown as the reliable part. Recording on an idle machine helps.
