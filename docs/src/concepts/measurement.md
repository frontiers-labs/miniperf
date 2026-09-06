# How miniperf measures

This chapter explains the vocabulary that appears in `mperf record` output and in the `capture_fidelity` and `snapshot_collectors` tables: features, mechanisms, rungs, quality, and scope. Understanding it tells you how much to trust a number.

## Features and mechanisms

A **feature** is a question the profiler asks of the hardware. There are four:

| Feature | Question |
|---|---|
| `PreciseMem` | Which loads and stores missed, at what address, with what latency? |
| `Topdown` | How are pipeline slots split between retiring, bad speculation, frontend, and backend? |
| `HwCallstack` | Can the hardware provide call stacks without copying the user stack? |
| `DramBw` | How many bytes crossed the memory controller? |

A **mechanism** is one way a particular CPU answers. For each feature miniperf holds an ordered list, best first:

| Feature | Mechanisms, in preference order |
|---|---|
| `PreciseMem` | Intel PEBS, AMD IBS, Arm SPE |
| `Topdown` | Intel PERF_METRICS fixed counters, Arm pmuv3 slots |
| `HwCallstack` | LBR call stacks |
| `DramBw` | uncore memory-controller PMU, or the vendor `/dev/ddr_perf` device |

At record time, `libprof::resolve` walks the list and takes the first mechanism the kernel exposes and the process may use. Each rejected mechanism is recorded with its reason. If none is available, the feature falls back to `Baseline`, ordinary programmable counters, and the rung is called `counter_only`.

That walk is why there is no flag for PEBS or LBR. A new hardware facility is a new mechanism behind an existing feature, and the scenario asks for the feature.

## Rungs and capture fidelity

The chosen mechanism is the recording's **rung**, printed on the second line of every `mperf record`:

```
Capture fidelity: tma at 'counter_only' (fixed topdown: core PMU exposes no `slots` + `topdown-*` events (PERF_METRICS is Icelake and newer))
```

The `capture_fidelity` table keeps the whole ladder:

```
┌──────────┬───────────────────┬──────────┬─────────────────────────────────────────────────────────────────────┐
│ scenario ┆        rung       ┆  status  ┆                                reason                               │
╞══════════╪═══════════════════╪══════════╪═════════════════════════════════════════════════════════════════════╡
│ tma      ┆ fixed_topdown     ┆ rejected ┆ fixed topdown: core PMU exposes no `slots` + `topdown-*` events ... │
│ tma      ┆ arm_slots_topdown ┆ rejected ┆ Arm topdown: no `armv8_pmuv3*` PMU exposed                          │
│ tma      ┆ counter_only      ┆ chosen   ┆                                                                     │
└──────────┴───────────────────┴──────────┴─────────────────────────────────────────────────────────────────────┘
```

Each scenario resolves one feature for its rung: `snapshot` asks for `HwCallstack`, `tma` for `Topdown`, `mem` for `PreciseMem`, and `roofline` for `DramBw`.

## Measurement quality

Every mechanism carries a **quality**:

- **Exact.** The hardware answers the exact question asked. PEBS, PERF_METRICS, Arm slots, LBR, and uncore counters are exact.
- **Estimated.** The hardware answers a nearby question. IBS and SPE sample every micro-operation and tag the memory ones, so their sample period is in operations, not loads. Baseline counters computing top-down from stall events are estimated too.

Quality appears as a column on the collector and resource tables. Collector rows use labels such as `exact_system`, `exact_process_tree`, `best_effort`, `attributed`, and `unavailable`.

## Scope

A measurement has a **scope**: what it counts.

- `process_tree` and `process` values count the profiled program and its children only. Process CPU time from the cgroup, RSS, and modeled DRAM traffic are process-scoped.
- `system_during_target` values count the whole machine for as long as the recording ran. Memory-controller bandwidth, clock frequency, temperature, and disk busy time are system-scoped, because the hardware has no way to attribute them.

The distinction matters most for bandwidth. A measured DRAM figure includes your compiler running in another terminal. A modeled figure does not, but it is a model. Both tables and the GUI label which one you are looking at.

## Sources and collectors

Inside a scenario, each independent stream of data is a **source**: PMU sampling, precise memory sampling, procfs and cgroup polling, host telemetry, BPF, and the trace collector. A scenario declares which sources are required and which are optional. A required source that cannot start aborts the recording. An optional one prints a warning and is recorded in `snapshot_collectors` with a status such as `unavailable` or `permission_denied` and a message explaining why.

Nothing is silently zero. If a column exists and holds a number, something measured it.

## Multiplexing and confidence

Programmable counters are scarce. When a scenario needs more events than the PMU has counters, the kernel time-slices groups of events. Every sample row in `pmu_counters` carries `time_enabled`, `time_running`, and their ratio `confidence`. Formulas divide by `confidence` to scale a partially counted event up to the full interval. `mperf stat` shows the same ratio in its `Scaling` column. A scaled value is a statistically sound estimate, not a count. When exactness matters, ask for fewer events.

Disabling the NMI watchdog frees one counter, which on a four-counter PMU lets a whole top-down group fit without multiplexing.
