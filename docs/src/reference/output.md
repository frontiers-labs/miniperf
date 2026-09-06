# Recording directory layout

`mperf record -o DIR` creates `DIR` and fills it during and after the run. Everything a viewer needs is inside, and the directory can be copied between machines.

## Metadata

`info.json` describes the recording: format version (currently 3), scenario, command line, CPU model and vendor, logical CPU count and core clusters, sampling frequency, the capture fidelity ladder, collector statuses, and a `scenario_info` object with scenario details. Roofline and memory recordings add `cpu_info.roofline_calibration` or the memory ceiling with every calibration sample.

## Tables

Every table is a Parquet file. Tables written while the workload runs are split into segments named `<table>-<pid>-<n>.parquet` or `<table>-<n>.parquet`. Each segment closes with a proper footer at 64 MiB, so a crash loses at most the open segment. Tables built by postprocessing are single files named `<table>.parquet`. Both forms appear as one view named `<table>` in `mperf query`.

Present in every recording:

| File | Contents |
|---|---|
| `samples*.parquet` | One row per PMU sample with its `stack_id` |
| `stacks.parquet` | Deduplicated call stacks, leaf first |
| `modules*.parquet` | Executable mappings per process, with build ids |
| `strings*.parquet` | Interned strings |
| `clock*.parquet` | Monotonic and realtime clock anchors |
| `proc_map.parquet` | Address to function, file, line, and module |
| `pmu_counters.parquet` | One row per sample group with one column per counter |
| `cpu_observations.parquet` | Logical-CPU activity intervals, independent of stacks |
| `derived_metrics.parquet` | Recording-wide host metrics with their formulas |
| `capture_fidelity.parquet` | The mechanism ladder with the chosen and rejected rungs |
| `snapshot_collectors.parquet` | Collector statuses |
| `snapshot_resource_samples.parquet`, `snapshot_summary.parquet` | Host telemetry time series and peaks |
| `assembly_lines.parquet`, `assembly_samples.parquet`, `assembly_address_stats.parquet`, `assembly_module_metadata.parquet` | Disassembly of hot functions with per-address samples, when `objdump` is installed |

Per scenario:

| Scenario | Additional tables |
|---|---|
| `snapshot` | `hotspots`, `snapshot_processes`, `snapshot_findings`, `bpf_metrics` when BPF ran |
| `tma` | `tma`, `tma_summary`, `tma_intervals`, `mem_samples`, `alloc_site_memory`, `cacheline_contention` |
| `mem` | `hotspots`, `memory_summary`, `memory_timeline`, `memory_working_set`, `memory_miss_ratio`, `memory_reuse_distance`, `memory_spatial_utilization`, `memory_strides`, `mem_samples`, `alloc_site_memory`, `cacheline_contention` |
| `roofline` | `hotspots`, `roofline`, `roofline_loops`, `roofline_loop_threads`, `roofline_ops`, `roofline_loop_runs`, and the `memory_*` tables with the QEMU backend |

When the program emitted trace events: `events*.parquet`, `payloads*.parquet`, `event_meta*.parquet`, and the aggregated `custom_events.parquet`.

Intermediate tables named `samples_raw`, `resource_samples`, and `process_samples` exist during the run and are deleted by postprocessing. `mem_samples_raw` is kept.

## Flame graphs

`flamegraph_cycles.svg` and `flamegraph_instructions.svg` are rendered from `flamegraph_cycles.folded` and `flamegraph_instructions.folded`, which are in the folded-stack format that other flame graph tools accept. On heterogeneous CPUs, per-cluster files such as `flamegraph_cycles_cortex_a720.svg` are added.

## Side files

| File | Scenario | Contents |
|---|---|---|
| `qemu-roofline.counts` | `roofline`, `mem` | Engine totals as `name=value` lines, including architectural and modeled bytes |
| `qemu-roofline.cfg` | `roofline`, `mem` | Observed translation-block entries, edges, and counts |
| `qemu-roofline.loops.json` | `roofline`, `mem` | Binary loop candidates with address ranges, trip counts, and per-loop accounting |
| `qemu-roofline.memory.json` | `mem`, `roofline` with QEMU | Reuse distance, working set, stride, and spatial histograms |
| `memory-native.json` | `roofline`, `mem` | PID and time bounds of the timed run |
| `memory-rss.txt` | `mem` | RSS samples every 10 ms |
| `memory-bandwidth.txt` | `mem`, `roofline` | Memory-controller read and write bytes every 5 ms, when available |
| `memory-allocations.txt` | `mem` | Every allocation, free, and mapping from the libc shim |

## Recovery

After a crash, `mperf recover DIR` moves segments without a footer aside so the remaining files open. Postprocessing does not rerun.
