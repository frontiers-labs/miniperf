# Snapshot a busy machine

In this tutorial we point the `snapshot` scenario at a running workload, read the ranked findings, and use them to choose the next measurement. A snapshot is the right first step when you have a slow system and no hypothesis.

## Record

Any process tree works. Here we launch a CPU-heavy program and bound the snapshot to three seconds:

```sh
sudo taskset -c 0-7 mperf record -s snapshot -o snap --duration 3s -- ./matmul 20
```

```
Record profile with Snapshot scenario
Capture fidelity: snapshot at 'counter_only' (LBR: core PMU advertises no branch-record depth in `caps/branches`)
Postprocessing...
```

To snapshot something already running, attach by PID instead:

```sh
sudo mperf record -s snapshot -o snap --duration 30s -p $(pgrep -o myserver)
```

`sudo` is what lets the BPF collector load. Without it the snapshot still completes, and the findings tell you what it missed.

## Read the findings

```sh
mperf query snap 'SELECT rank, severity, resource, finding, recommendation FROM snapshot_findings ORDER BY rank'
```

```
┌──────┬──────────┬──────────┬────────────────────────────────────────────────────┬─────────────────────────────────────────────────────────────────────────────────┐
│ rank ┆ severity ┆ resource ┆                       finding                      ┆                                 recommendation                                  │
╞══════╪══════════╪══════════╪════════════════════════════════════════════════════╪═════════════════════════════════════════════════════════════════════════════════╡
│    1 ┆ medium   ┆ network  ┆ Network utilization or reliability needs attention ┆ Inspect `ip -s link`, `ss -ti`, retransmits, and then capture packets ...       │
│    2 ┆ high     ┆ cpu      ┆ The host clocked below its ceiling                 ┆ Check cooling, the power limits (`cpupower frequency-info`, RAPL/...) and ...   │
│    3 ┆ info     ┆ cpu      ┆ CPU capacity has headroom                          ┆ Use Hotspots to confirm where CPU time is spent before enabling a ...           │
│    4 ┆ info     ┆ coverage ┆ Collector throttle_events is unavailable           ┆ Use the recorded fallback data or grant the documented kernel capabilities ...  │
│    5 ┆ info     ┆ coverage ┆ Collector uncore_memory is unavailable             ┆ Use the recorded fallback data or grant the documented kernel capabilities ...  │
│    6 ┆ info     ┆ coverage ┆ Collector bpf is permission_denied                 ┆ Use the recorded fallback data or grant the documented kernel capabilities ...  │
└──────┴──────────┴──────────┴────────────────────────────────────────────────────┴─────────────────────────────────────────────────────────────────────────────────┘
```

This snapshot was taken without root, and rank 6 says so. Rank 2 is the interesting one: the host ran below its own clock ceiling, so every counter in this recording was measured at a reduced frequency. That finding comes first in practice, because tuning code on a throttled machine measures the cooling, not the code.

Rank 1 is a network finding on a program that does no networking. The rule fires on any interface error, drop, or retransmit during the run, and something else on this machine had one. The `evidence` column, omitted here for width, names the interface and the count.

The `coverage` rows are not findings about the workload. They tell you what the snapshot could not see and how to see it next time.

## Look at the evidence

The summary table has the peak of every metric, with its scope and source:

```sh
mperf query snap "SELECT metric, value, unit, scope, source FROM snapshot_summary WHERE resource = 'cpu' AND metric LIKE 'frequency%'"
```

```
┌────────────────┬───────────────────┬───────┬──────────────────────┬─────────────┐
│     metric     ┆       value       ┆  unit ┆         scope        ┆    source   │
╞════════════════╪═══════════════════╪═══════╪══════════════════════╪═════════════╡
│ frequency      ┆ 2269151291.666667 ┆ hertz ┆ system_during_target ┆ cpufreq_avg │
│ frequency_max  ┆ 4200000000.000000 ┆ hertz ┆ system_during_target ┆ cpufreq_avg │
│ frequency_peak ┆ 3999526000.000000 ┆ hertz ┆ system_during_target ┆ cpufreq_avg │
└────────────────┴───────────────────┴───────┴──────────────────────┴─────────────┘
```

The mean clock was 2.27 GHz against a ceiling of 4.2 GHz. The `scope` column reminds us this is the whole machine, not the process. The process-tree numbers sit next to them:

```sh
mperf query snap "SELECT metric, value, unit, source FROM snapshot_summary WHERE scope = 'process_tree' AND resource = 'cpu'"
```

`cgroup_cpu_time` of 3.06 seconds over a 3-second run means the tree used one core's worth of CPU. With eight CPUs pinned, that is headroom, which is what rank 3 said.

## See what ran

```sh
mperf query snap 'SELECT name, status, quality, message FROM snapshot_collectors'
```

```
┌─────────────────┬───────────────────┬────────────────────┬───────────────────────────────────────────────────────────────────────┐
│       name      ┆       status      ┆       quality      ┆                                message                                │
╞═════════════════╪═══════════════════╪════════════════════╪═══════════════════════════════════════════════════════════════════════╡
│ throttle_events ┆ unavailable       ┆ unavailable        ┆ no thermal_throttle counters exposed by this host                     │
│ host_telemetry  ┆ available         ┆ exact_system       ┆ host clock and temperature sensors                                    │
│ process_tree    ┆ available         ┆ best_effort        ┆ existing and future descendants are polled by PID/start-time identity │
│ uncore_memory   ┆ unavailable       ┆ unavailable        ┆ no memory-controller PMU aliases and no vendor bandwidth device       │
│ cgroup          ┆ available         ┆ exact_process_tree ┆ launched descendants inherit a private cgroup                         │
│ bpf             ┆ permission_denied ┆ unavailable        ┆ unprivileged BPF is disabled; run with suitable BPF/perf capabilities │
└─────────────────┴───────────────────┴────────────────────┴───────────────────────────────────────────────────────────────────────┘
```

Every collector reports whether it ran and, if not, why. This is the table to read before trusting a finding that depends on a collector.

## Decide what to do next

The findings are ranked so the top one is the next measurement:

- **The host clocked below its ceiling.** Fix the cooling or governor, then repeat any measurement you care about.
- **CPU capacity or run queue pressure is high.** Run `mperf record -s tma` on the hot process and read its hotspots.
- **Memory capacity or paging pressure is visible.** Run `mperf record -s mem`.
- **A block device was highly utilized.** Use `iostat` and the BPF block-latency data in `snapshot_resource_samples`.

The snapshot did the triage. The specialized scenarios do the diagnosis.

## In the viewer

`mperf show snap` has a Resources tab titled *What to measure next* with the same findings. `mperf-gui snap` adds one card per resource with utilization, saturation, and error rows over time.
