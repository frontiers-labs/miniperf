# Memory analysis

```sh
mperf record -s mem -o results/mem -- ./workload
```

The `mem` scenario explains a memory-bound program. It runs the program twice. The first run is native and timed, with the baseline counters, an RSS sampler, the allocation shim, precise memory samples where the CPU supports them, and the memory-controller bandwidth counters where permissions allow. The second run replays the program under DynamoRIO or QEMU and records every memory reference. Postprocessing combines the two.

The scenario requires a native executable and an accounting engine. Linux packages ship both engines. A source build needs the bundles described in [Install miniperf](install.md). `--pid` is not supported.

## What it produces

`memory_summary` is one row with the headline numbers:

| Column | Meaning |
|---|---|
| `accessed_footprint_bytes`, `unique_lines` | Distinct bytes and cache lines the program touched |
| `cold_fraction` | Share of references that were the first touch of a line |
| `architectural_load_bytes`, `architectural_store_bytes` | Exact bytes the instructions loaded and stored |
| `modeled_dram_read_bytes`, `modeled_dram_write_bytes` | Traffic a shared last-level cache model configured from the host predicts |
| `achieved_gbytes_per_second`, `peak_gbytes_per_second`, `bandwidth_utilization` | Achieved DRAM bandwidth against the calibrated sustainable ceiling |
| `bandwidth_source`, `bandwidth_scope` | `hardware_memory_controller` and `system_during_target` when uncore counters were available, otherwise `process_modeled` and `process` |
| `peak_allocated_bytes`, `peak_rss_bytes`, `live_mapped_bytes` | Heap, resident set, and mapped memory peaks |
| `quality` | The method's quality label, with `+children-excluded` appended if the program forked |

Around it:

- `memory_working_set` gives the mean, 95th percentile, and maximum number of distinct bytes touched in windows of 1024 to 1048576 references. Read it as the cache size the program needs at each time scale.
- `memory_miss_ratio` is the miss-ratio curve: for cache sizes from one line to a gigabyte, the fraction of references that would miss a fully associative LRU cache of that size. It comes from an exact reuse-distance histogram, which is also written as `memory_reuse_distance`.
- `memory_spatial_utilization` is a histogram of how much of each cache line the program used before evicting it. A peak at 12 % means the program touched one 8-byte value per 64-byte line.
- `memory_strides` is a histogram of the distance, in lines, between consecutive references.
- `memory_timeline` interleaves RSS, live heap, mapped memory, and measured DRAM bandwidth over the run.

With PEBS or SPE, `mem_samples` holds individual sampled accesses with their data address, latency, and the cache level that served them. `alloc_site_memory` groups those samples by the allocation call stack that owns the address, and `cacheline_contention` lists lines touched by more than one thread, ordered as false-sharing candidates.

```sh
mperf query results/mem 'SELECT cache_bytes, miss_ratio FROM memory_miss_ratio WHERE cache_bytes IN (32768, 524288, 4194304, 33554432)'
```

```
┌─────────────┬────────────┐
│ cache_bytes ┆ miss_ratio │
╞═════════════╪════════════╡
│      32,768 ┆   0.561073 │
│     524,288 ┆   0.125530 │
│   4,194,304 ┆   0.001234 │
│  33,554,432 ┆   0.000746 │
└─────────────┴────────────┘
```

This program misses 56 % of the time in a 32 KiB L1 and almost never in a 4 MiB cache. Its working set is between 512 KiB and 4 MiB.

## Read the bandwidth numbers carefully

Hardware memory-controller counters are system-wide. During the run they count every process on the machine, and `bandwidth_scope` says so. When they are unavailable, the modeled traffic is process-specific but comes from a cache model, not from hardware. Neither is wrong, but they answer different questions, and the `bandwidth_source` column tells you which one you have.

The allocation shim records every allocation for the replay and a sampled subset for the trace. See [Trace your code and runtimes](tracing.md) for its sampling controls.

The [Measure a working set](../tutorials/memory.md) tutorial runs the scenario on a matrix multiply and reads each table.
