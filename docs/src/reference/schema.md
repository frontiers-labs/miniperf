# Database schema

`mperf query` exposes one view per table. This page lists every table with its columns. Discover the schema of a specific recording with:

```sh
mperf query DIR "SELECT view_name AS name FROM duckdb_views() WHERE NOT internal ORDER BY name"
mperf query DIR "PRAGMA table_info('hotspots')"
```

Percent-like columns hold fractions between 0 and 1. The text formatter prints known ones as percentages.

## Analysis tables

### hotspots

One row per function. Written by `snapshot`, `mem`, and `roofline`.

| Column | Meaning |
|---|---|
| `func_name` | Symbol name, or `[unknown]` |
| `total` | Share of all sampled cycles |
| `cycles`, `instructions` | Summed counters |
| `ipc` | Instructions per cycle |
| `branch_miss_rate`, `branch_mpki` | Branch misses over branches, and per thousand instructions |
| `cache_miss_rate`, `cache_mpki` | LLC misses over references, and per thousand instructions |

### tma, tma_summary, tma_intervals

Written by `tma`.

`tma` has one row per function: `func_name`, `num_samples`, `total`, `cycles`, `instructions`, `ipc`, and one column per top-down metric, named with dots replaced by underscores, for example `be_bound_memory_bound`.

`tma_summary` has one row per metric: `metric`, `value` over the whole run, and `verdict`, which is `dominant` on the largest metric and `NULL` elsewhere.

`tma_intervals` has one row per metric per second: `start_ns`, `metric`, `value`.

### derived_metrics

Recording-wide host metrics: `name`, `value`, `unit`, and `expression`, the formula that produced the value. A metric is omitted when an event it needs was not recorded.

### capture_fidelity

One row per rung considered: `scenario`, `rung`, `status` (`chosen` or `rejected`), `reason`.

### custom_events

Trace points aggregated: `name`, `function`, `file`, `line`, `kind` (`span`, `instant`, `counter`, `loss`), `count`, `total_ns` for spans, `total_value` for the others.

## Snapshot tables

### snapshot_findings

`rank`, `severity` (`high`, `medium`, `info`), `resource`, `finding`, `evidence`, `recommendation`, `scope`, `quality`.

### snapshot_summary

The peak of every metric: `resource`, `resource_id`, `category` (`utilization`, `saturation`, `errors`), `metric`, `value`, `unit`, `scope`, `source`, `quality`.

### snapshot_resource_samples

The time series behind the summary: `timestamp_ns`, `resource`, `resource_id`, `category`, `metric`, `value`, `unit`, `scope`, `source`, `quality`. BPF results appear with `source = 'bpftrace'` as `run_queue_latency`, `block_latency`, `block_bytes`, and `tcp_retransmits`.

### snapshot_processes

`pid`, `ppid`, `start_ticks`, `first_seen_ns`, `last_seen_ns`, `command`, `quality`.

### snapshot_collectors

`name`, `status` (`available`, `unavailable`, `permission_denied`, `degraded`, `error`), `source`, `quality`, `message`.

## Memory tables

### memory_summary

One row. See [Memory analysis](../guide/scenario-mem.md) for the meaning of each column: `format_version`, `process_id`, `line_size`, `reference_count`, `architectural_load_bytes`, `architectural_store_bytes`, `unique_lines`, `accessed_footprint_bytes`, `cold_references`, `cold_fraction`, `modeled_dram_read_bytes`, `modeled_dram_write_bytes`, `native_duration_ns`, `achieved_gbytes_per_second`, `peak_gbytes_per_second`, `bandwidth_utilization`, `bandwidth_source`, `bandwidth_scope`, `live_allocated_bytes`, `peak_allocated_bytes`, `live_mapped_bytes`, `peak_rss_bytes`, `quality`.

### memory_timeline

`timestamp_ns`, `live_allocated_bytes`, `live_mapped_bytes`, `rss_bytes`, `dram_read_gbytes_per_second`, `dram_write_gbytes_per_second`, `bandwidth_source`. Rows are sparse: each row fills the columns its source produced.

### memory_working_set

`window_references`, `mean_bytes`, `p95_bytes`, `max_bytes`.

### memory_miss_ratio

`cache_lines`, `cache_bytes`, `miss_ratio`, for cache sizes from 1 to 2^30 lines.

### memory_reuse_distance, memory_strides, memory_spatial_utilization

Histograms: `distance_log2_lines` and `reference_count`; `stride_log2_lines` (signed) and `reference_count`; `utilization_percent` and `lines`.

### mem_samples

Precise memory samples, when PEBS or SPE was available: `timestamp`, `pid`, `tid`, `cpu`, `ip`, `stack_id`, `call_stack`, `data_addr`, `cache_line`, `latency_cycles`, `op`, `level` (`L1`, `L2`, `L3`, `RAM`, and others), `hit_miss`, `snoop`, `tlb`, `remote`, `locked`.

### alloc_site_memory

Samples grouped by the allocation that owns the address: `alloc_site`, `alloc_stack_id`, `allocation_count`, `allocated_bytes`, `sample_count`, `miss_count`, `hitm_count`, `l1_count`, `l2_count`, `l3_count`, `ram_count`, `avg_latency_cycles`, `p95_latency_cycles`.

### cacheline_contention

Lines touched by more than one thread or with any HITM snoop: `cache_line`, `sample_count`, `distinct_threads`, `distinct_cpus`, `hitm_count`, `avg_latency_cycles`, `max_latency_cycles`.

## Roofline tables

### roofline

One row per loop with a plotted point or its accounting: `file_name`, `function_name`, `line`, then for each of `scalar_int`, `scalar_float`, `scalar_double`, `vector_int`, `vector_float`, `vector_double` the columns `<kind>_ops` in operations per second and `<kind>_ai` in operations per byte, then `timing_samples`, `timing_relative_error`, `timing_quality`, `module_offset`, `trip_count`, `arch_bytes`, `dram_bytes`, `measured_dram_bytes`, `traffic_source`.

### roofline_binary_loops

Raw per-loop accounting from the engine: `module_offset`, `function_name`, `file_name`, `line`, `trip_count`, `duration_ns`, `timing_samples`, `timing_relative_error`, `timing_quality`, `measured_dram_bytes`, `bytes_load`, `bytes_store`, `arch_bytes_load`, `arch_bytes_store`, and the six `*_ops` counts.

### roofline_ops, roofline_loop_runs

Compiler-backend tables: per instrumented loop execution, byte and operation counts (`roofline_ops`) and time bounds (`roofline_loop_runs`). Function and file columns are string ids into `strings`.

## Sample tables

### pmu_counters

One row per sample group: `unique_id`, `process_id`, `thread_id`, `cpu`, `time_enabled`, `time_running`, `confidence`, `timestamp`, `ip`, `call_stack` as a semicolon-joined folded stack, and one `pmu_<event>` column per recorded counter, for example `pmu_cycles`.

### samples, stacks, proc_map, modules

`samples`: `timestamp`, `pid`, `tid`, `cpu`, `group_id`, `event_id`, `value`, `time_enabled`, `time_running`, `ip`, `stack_id`.
`stacks`: `stack_id` and a list of instruction pointers, leaf first.
`proc_map`: `ip`, `func_name`, `file_name`, `line`, `module_path`.
`modules`: `pid`, `path`, `build_id`, `address`, `size`, `offset`.

### cpu_observations

`process_id`, `thread_id`, `cpu`, `timestamp`, `interval_start_ns`, `weight_ns`, `source`, `call_stack`.

### assembly tables

`assembly_lines`: `module_path`, `symbol`, `rel_address`, `runtime_address`, `instruction`, `source_file`, `source_line`.
`assembly_samples` and `assembly_address_stats`: `module_path`, `func_name`, `address`, `samples`, `cycles`, `instructions`, `branch_misses`, `branch_instructions`, `llc_misses`, `llc_references`.
`assembly_module_metadata`: `module_path`, `load_bias`.

## Trace tables

### events

`timestamp`, `event_id`, `instance`, `parent_id`, `flow_id`, `type` (0 begin, 1 end, 2 instant, 3 counter, 4 loss), `pid`, `tid`, `value`. An end row's `flow_id` is its begin row's `instance`.

### payloads, event_meta, strings

`payloads`: `event_id`, `name_id`, `function_id`, `file_id`, `line`, `column`, with the id columns pointing into `strings`.
`event_meta`: `event_instance`, `key_id`, `value_type`, `value_int`, `value_double`, `value_string_id`.
`strings`: `id`, `string`.

### clock, clock_sync, device_clock

`clock`: `anchor`, `monotonic_ns`, `realtime_ns`.
`clock_sync`: `peer`, `phase`, `local_ns`, `peer_ns`, `uncertainty_ns`, from the MPI shim.
`device_clock`: `device`, `host_before_ns`, `device_ns`, `host_after_ns`, from the CUDA shim.
