# Trace your code and runtimes

Sampling tells you where time goes. Tracing tells you what the program was doing. Any process that runs under `mperf record` can emit spans, instants, and counters into the recording, and they end up in the `custom_events` table next to the hotspots.

Trace points are identified by a stable hash of their name and source location, so the same line of code has the same identity across processes, MPI ranks, and runs.

## C and C++

Include `collector-core/include/mperf_trace.h` and compile the stub `collector-core/stub/mperf_trace_stub.c` into your program. Every call is a no-op unless the process runs under `mperf record`, so you can leave the instrumentation in production builds.

```c
#include <mperf_trace.h>

void step(int iteration, double residual) {
    MPERF_SCOPE("solver_step");            // C++ only: a span that ends when the scope exits
    MPERF_COUNTER("residual", residual);   // a value series
    MPERF_INSTANT("checkpoint", iteration); // a point in time
}
```

```sh
cc -O2 -g -I collector-core/include app.c collector-core/stub/mperf_trace_stub.c -o app -ldl
```

The macros register the trace point on first use with `__func__`, `__FILE__`, and `__LINE__`. For a span in C, or for cross-thread and explicitly parented spans, use the functions directly:

| Function | Purpose |
|---|---|
| `mperf_trace_register(payload)` | Register a trace point once and keep the handle. Returns `NULL` when no collector is present. |
| `mperf_trace_begin(handle, parent)` | Start a span. Returns its instance id. Pass 0 for no explicit parent. |
| `mperf_trace_end(handle, instance)` | End the span. May be called from another thread. |
| `mperf_trace_instant(handle, value)` | Record a point with a value. |
| `mperf_trace_counter(handle, value)` | Record a counter sample. |

Set `flags = MPERF_TRACE_FLAG_STACK` in the payload to capture a frame-pointer call stack, up to 64 frames, on every begin, instant, and counter from that trace point. The macros do not set it.

## Rust

The `mperf-trace` crate wraps the same interface:

```rust
let _guard = mperf_trace::trace_scope!("phase");
```

The macro caches the trace point in a static and ends the span when the guard drops. For counters, instants, or stack capture, register a `TracePoint` yourself:

```rust
static RESIDUAL: OnceLock<TracePoint> = OnceLock::new();
let tp = RESIDUAL.get_or_init(|| TracePoint::register("residual", module_path!(), file!(), line!(), false));
tp.counter(value as i64);
```

## Run it

```sh
mperf record -s snapshot -o results/traced -- ./app
mperf query results/traced 'SELECT name, kind, count, total_ns, total_value FROM custom_events'
```

```
┌───────────┬─────────┬───────┬───────────┬─────────────┐
│    name   ┆   kind  ┆ count ┆  total_ns ┆ total_value │
╞═══════════╪═════════╪═══════╪═══════════╪═════════════╡
│ fill      ┆ span    ┆     1 ┆ 1,445,576 ┆           0 │
│ multiply  ┆ span    ┆     3 ┆ 1,052,317 ┆           0 │
│ iteration ┆ counter ┆     3 ┆         0 ┆           3 │
│ done      ┆ instant ┆     1 ┆         0 ┆           3 │
└───────────┴─────────┴───────┴───────────┴─────────────┘
```

Spans report their count and total duration. Counters and instants report their count and summed value. The raw events with timestamps, thread ids, and parent links are in the `events` table, joined to names through `payloads` and `strings`.

The stub finds the collector library `libmperf_collector.so` through the dynamic loader. Release packages place it where `mperf` sets it up. With a source build, point at it explicitly:

```sh
MPERF_COLLECTOR_LIBRARY=target/release/libmperf_collector.so mperf record -s snapshot -o out -- ./app
```

## Runtime shims

Thin proxies forward events from common runtimes without code changes. Each one is a Rust shared library under `shims/` that hooks the runtime's own tool interface:

| Runtime | Library | Activate with |
|---|---|---|
| libc allocations, `mmap`, threads | `libmperf_libc.so` | `LD_PRELOAD` |
| OpenMP parallel regions, tasks, barriers | `libmperf_ompt.so` | `OMP_TOOL_LIBRARIES` |
| TBB and ITT tasks, frames, domains | `libmperf_itt.so` | `INTEL_LIBITTNOTIFY64` |
| CUDA kernel launches and transfers | `libmperf_cupti.so` | `CUDA_INJECTION64_PATH` |
| MPI rank identity and clock alignment | `libmperf_mpi.so` | `LD_PRELOAD` |

`mperf record` activates the libc shim itself in the `mem` scenario. The others you set up in the environment before the workload, for example:

```sh
OMP_TOOL_LIBRARIES=$PWD/target/release/libmperf_ompt.so \
  mperf record -s snapshot -o results/omp -- ./omp_app
```

Release packages ship the libc shim. Build the others from source with `cargo build --release -p miniperf-shim-ompt`, and likewise `itt`, `mpi`, and `cupti`.

The libc shim samples allocations to keep overhead low: every sixteenth allocation per thread, plus every allocation of 65536 bytes or more. Frees are never sampled, so lifetimes stay complete. `MPERF_LIBC_SAMPLE_EVERY` and `MPERF_LIBC_SIZE_THRESHOLD` change the rates, and the effective rates are recorded in the trace as two counters.

## Loss policy

Trace buffers are bounded at 64 buffers of 256 KiB per process. When a producer thread cannot get a buffer, its events are dropped and counted, never blocked. Drops surface as `loss` rows in `custom_events` with the dropped count as their value.

## Environment

- `MPERF_SESSION_DIR` names the recording directory. `mperf record` sets it. Unset means tracing is off.
- `MPERF_COLLECTOR_LIBRARY` overrides the collector library path.
- `MPERF_CONTROL_SHMEM` enables a shared-memory control channel for live statistics and pause, resume, and flush commands. It is a library interface for embedding miniperf; the `mperf` command does not use it.

A `manifest.yaml` schema for assigning trace points to timeline tracks exists in the `mperf-data` crate, but the current GUI does not read it. Custom events appear as the `custom_events` table.
