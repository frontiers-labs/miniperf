# Scenarios

A scenario is a question and the measurement plan that answers it. `mperf record -s` takes one of four.

| Scenario | Question | Launch or attach | Extra requirements |
|---|---|---|---|
| [`snapshot`](scenario-snapshot.md) | Which resource is this process tree short of, and what should I measure next? | both | root for the BPF collector |
| [`tma`](scenario-hotspots.md) | Which functions are hot, and is the pipeline stalled on memory, on execution, on fetch, or on mispredictions? | launch | a CPU with a top-down model |
| [`mem`](scenario-mem.md) | How big is the working set, how well does it use cache lines, and how much DRAM traffic does it cause? | launch | QEMU or DynamoRIO, native executable |
| [`roofline`](scenario-roofline.md) | How close is each loop to the machine's compute and bandwidth ceilings? | launch | QEMU or DynamoRIO, or compiler instrumentation |

Start with `snapshot` when you do not know where the problem is. Its findings table names the scenario to run next.

Use `tma` for CPU-bound code. It is the profile most people want: a flame graph, hotspots, and the reason each hotspot is slow.

Use `mem` when top-down says memory bound and you need to know whether the fix is a smaller working set, better spatial locality, or fewer allocations.

Use `roofline` for numerical kernels, where the question is how much of the hardware you are using.

## What every scenario records

All four scenarios sample the same twelve baseline counters and produce the same core tables: `hotspots` or `tma`, `pmu_counters`, `proc_map`, `cpu_observations`, `derived_metrics`, `capture_fidelity`, flame graphs, and the assembly tables when `objdump` is installed. Host telemetry, clock frequency and temperature at one-second intervals, is recorded alongside, so every recording can tell you whether the CPU was throttled while it ran.

## Sampling rate and stack size

| Scenario | Frequency | Stack copied per sample |
|---|---|---|
| `snapshot` | 99 Hz | 2 KiB |
| `tma`, `mem`, `roofline` | 1000 Hz | 8 KiB |

The snapshot rate is low on purpose. It is a survey, meant to run for tens of seconds on a whole process tree without disturbing it.
