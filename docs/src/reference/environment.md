# Environment variables

Variables you set before running `mperf`:

| Variable | Meaning | Default |
|---|---|---|
| `MINIPERF_CPU_FAMILY` | Force the PMU event family on a heterogeneous CPU. Arm accepts `cortex_a520` and `cortex_a720`. RISC-V accepts `x60`, `x100`, and `a100`, with or without a `spacemit_` prefix. An unknown value prints a warning and is ignored. | the first recognized core |
| `MINIPERF_DEBUGINFOD` | Set to `1` to let symbolization fetch debug info from the servers in `DEBUGINFOD_URLS`. Both variables must be set. | offline |
| `MINIPERF_CACHE_DIR` | Root of the symbol cache. | `$XDG_CACHE_HOME`, else `$HOME` |
| `DEBUGINFOD_URLS` | Debuginfod servers, as for the `debuginfod-find` tool. | unset |
| `MPERF_QEMU` | QEMU user-mode binary for the `roofline` and `mem` scenarios. | bundle next to `mperf`, then `PATH` |
| `MPERF_QEMU_PLUGIN` | The miniperf QEMU plugin shared library. | artifact next to `mperf` |
| `MPERF_DYNAMORIO` | `drrun` launcher or DynamoRIO directory. | bundle next to `mperf`, then `PATH` |
| `MPERF_DR_CLIENT` | The miniperf DynamoRIO client library `libdr_roofline.so`. | discovered next to `mperf` or in the bundle |
| `MPERF_LIBC_SAMPLE_EVERY` | The libc shim records every Nth allocation per thread. | `16` |
| `MPERF_LIBC_SIZE_THRESHOLD` | Allocations of at least this many bytes are always recorded. | `65536` |
| `MPERF_SPE_DEBUG` | Print Arm SPE decoder diagnostics. | unset |

Variables `mperf record` sets for the profiled process. Applications and shims read them. Do not set them by hand unless you run a process outside `mperf record` and want it to trace into an existing session:

| Variable | Meaning |
|---|---|
| `MPERF_SESSION_DIR` | The recording directory. When unset, every trace macro and shim is a no-op. |
| `MPERF_COLLECTOR_LIBRARY` | Path to `libmperf_collector.so`. Defaults to the bare library name, resolved by the dynamic loader. |
| `MPERF_CONTROL_SHMEM` | Control-channel prefix for live statistics, pause, resume, and flush. Remote MPI ranks run without it and write files only. |
| `MPERF_PROFILE_ROOT_PID` | PID of the profiled root process. The libc shim scopes allocation capture to that tree. |
| `MPERF_MEMORY_ALLOCATIONS` | Output path of the raw allocation stream in the `mem` scenario. |
| `MPERF_COLLECTOR_ROOFLINE_INSTRUMENTED` | `1` on the instrumented Roofline run. |

Standard variables that change behavior:

| Variable | Effect |
|---|---|
| `LD_PRELOAD` | `mperf` prepends its shims and keeps your entries. |
| `LD_LIBRARY_PATH` | `mperf` prepends its own library directory for the compiler Roofline backend. |
| `OMP_TOOL_LIBRARIES`, `INTEL_LIBITTNOTIFY64`, `CUDA_INJECTION64_PATH` | Set by `mperf record` to the OpenMP, ITT, and CUDA shims. |
| `RAYON_NUM_THREADS`, `OMP_NUM_THREADS` | Thread counts for Roofline calibration and the workload. Set both when you compare against a ceiling. |
