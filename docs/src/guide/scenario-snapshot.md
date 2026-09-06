# System snapshot

```sh
sudo mperf record -s snapshot -o results/snap --duration 30s -- ./server
sudo mperf record -s snapshot -o results/snap --duration 30s -p 4242
```

The `snapshot` scenario is where an investigation starts. It watches a whole process tree, launched or attached, and applies the USE method: for each resource, utilization, saturation, and errors. It then ranks findings and names the measurement to run next.

Compared to the other scenarios it samples slowly, 99 Hz, and it follows every process the tree spawns, including ones created after the start.

## What it collects

Every second, for the process tree and for the system around it:

| Resource | Process tree | System |
|---|---|---|
| CPU | user and system time, context switches, faults, cgroup CPU time and PSI | per-cluster frequency and frequency ceiling, PSI, temperature, throttle events |
| Memory | RSS and PSS, cgroup memory current, peak, and limit, OOM events | total and available memory, swap, memory-controller read and write bytes, RAPL energy |
| Disk | read and write bytes and calls, cgroup I/O per device | per-device busy time and weighted I/O time |
| Network | | per-interface bytes, errors, drops, and link capacity |

With root and `bpftrace`, a BPF program attributes three more things to the tree: run-queue latency from the scheduler tracepoints, block I/O latency and bytes, and TCP retransmits.

Launched trees are placed in a private cgroup so the process-tree numbers are exact. Attached trees are tracked by PID and start time, which is best effort. The `quality` column on every row says which.

Hotspots and flame graphs are recorded too, at the low rate.

## What it produces

- `snapshot_findings` is the ranked diagnosis. Each row has a severity, a resource, a finding, the evidence, and a recommendation that names the next step.
- `snapshot_summary` has the peak value of every metric over the run, with its unit, scope, source, and quality.
- `snapshot_resource_samples` is the full time series behind the summary.
- `snapshot_processes` is the observed process tree with first and last seen times.
- `snapshot_collectors` records which collectors ran and why the others did not.

```sh
mperf query results/snap 'SELECT rank, severity, resource, finding, recommendation FROM snapshot_findings ORDER BY rank'
```

```
┌──────┬──────────┬──────────┬────────────────────────────────────────────────────┬──────────────────────────────────────────────────────────────────────────────────┐
│ rank ┆ severity ┆ resource ┆                       finding                      ┆                                  recommendation                                  │
╞══════╪══════════╪══════════╪════════════════════════════════════════════════════╪══════════════════════════════════════════════════════════════════════════════════╡
│    1 ┆ medium   ┆ network  ┆ Network utilization or reliability needs attention ┆ Inspect `ip -s link`, `ss -ti`, retransmits, and then capture packets ...        │
│    2 ┆ high     ┆ cpu      ┆ The host clocked below its ceiling                 ┆ Check cooling, the power limits (`cpupower frequency-info`, RAPL/...) ...        │
│    3 ┆ info     ┆ cpu      ┆ CPU capacity has headroom                          ┆ Use Hotspots to confirm where CPU time is spent before enabling a ...            │
│    4 ┆ info     ┆ coverage ┆ Collector throttle_events is unavailable           ┆ Use the recorded fallback data or grant the documented kernel capabilities ...   │
│    5 ┆ info     ┆ coverage ┆ Collector uncore_memory is unavailable             ┆ Use the recorded fallback data or grant the documented kernel capabilities ...   │
│    6 ┆ info     ┆ coverage ┆ Collector bpf is permission_denied                 ┆ Use the recorded fallback data or grant the documented kernel capabilities ...   │
└──────┴──────────┴──────────┴────────────────────────────────────────────────────┴──────────────────────────────────────────────────────────────────────────────────┘
```

## The findings rules

| Finding | Severity | Triggered when |
|---|---|---|
| CPU capacity or run queue pressure is high | high | the tree used 85 % or more of its CPU allowance, or CPU pressure stall was 5 % or more |
| Memory capacity or paging pressure is visible | high | memory reached 85 % of capacity, or there were major faults or OOM kills |
| A block device was highly utilized | high | any device was busy 80 % of the time or more |
| Network utilization or reliability needs attention | medium | link use at 70 % or more, or any interface errors, drops, or TCP retransmits |
| The host clocked below its ceiling | high | throttle events occurred, or a cluster's mean clock was under 90 % of its own maximum |
| CPU capacity has headroom | info | none of the CPU rules fired |
| System DRAM traffic was measurable | info | a memory-controller monitor was available |
| Collector X is unavailable | info | one row per collector that did not run |

The clock rule compares each cluster with its own ceiling, so little cores are not reported as throttled because big cores are faster.

## Degraded snapshots

Without root the snapshot still runs. `snapshot_collectors` then shows the BPF collector as `permission_denied`, and the findings lose the scheduler and block-latency evidence. `mperf doctor` lists what to change. A snapshot on a kernel older than 6.12 cannot sample threads the workload creates after `exec`, and says so in the same table.

The [Snapshot a busy machine](../tutorials/snapshot.md) tutorial walks through a complete run.
