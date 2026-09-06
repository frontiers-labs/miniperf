# Roofline

```sh
mperf record -s roofline -o results/spmv -- ./spmv
```

The `roofline` scenario measures how close each loop comes to the machine's ceilings. Before the workload runs, `mperf` measures the host's sustainable FP64 throughput and DRAM bandwidth with the same CPU affinity. Then it runs the workload natively for timing and PMU samples, and once more under an accounting engine that counts floating-point operations and bytes per loop.

```
Calibrating host Roofline ceilings...
Host ceilings: 123.67 GFLOP/s FP64, 19.76 GB/s memory (4 Rayon threads)
Roofline method: native timing with DynamoRIO binary accounting
Run 1: collecting native performance data for './spmv'
Run 2: collecting DynamoRIO instruction and memory accounting for './spmv'
DynamoRIO aggregate CFG: 8 natural loops, 1 irreducible cycles; candidates saved to 'results/spmv/qemu-roofline.loops.json'
```

## Backends

`--roofline-backend` selects the accounting engine. The default, `auto`, probes the executable and the host and picks the most accurate method available:

| Backend | Accounting | Traffic | When `auto` picks it |
|---|---|---|---|
| `dynamorio` | Binary loops, exact block counts, low overhead | Architectural bytes | Native executable, DynamoRIO available, host not riscv64 |
| `qemu` | Binary loops, exact operation counts, plus a shared last-level cache model | Modeled DRAM traffic | Native executable, DynamoRIO unavailable |
| `compiler` | Source loops instrumented at build time | Bytes the instrumentation reports | Executable carries the instrumentation and no engine is available |

If the DynamoRIO run fails under `auto`, `mperf` retries with QEMU and says so. An explicit backend never falls back. On riscv64 hosts `auto` prefers QEMU: the riscv64 DynamoRIO client is currently far slower than QEMU there, and `--roofline-backend dynamorio` still selects it explicitly.

For a cross-architecture executable, for example a RISC-V binary on an x86 host, `auto` refuses to run. QEMU could count operations, but the throughput would be emulator time and would say nothing about RISC-V hardware. Run the same command on a RISC-V host. `--roofline-backend qemu` forces the emulated run for accounting only, and the result is labeled `emulation-analysis`.

## Loop timing

Native PMU samples are matched to loop address ranges after the run. A loop gets a plotted throughput only when its estimated 95 % timing error is at most 10 %. Loops with too few samples keep their operation counts and appear with `timing_quality = 'insufficient-samples'`. Make the run long enough that the loops you care about collect hundreds of samples at 1000 Hz. Repeating the kernel a few hundred times inside the program is the usual fix.

Loops in shared libraries and the dynamic loader are excluded.

## What it produces

- `roofline` is the chart table: per loop, operations per second and arithmetic intensity for scalar and vector single and double precision, plus `timing_quality`, `traffic_source`, and byte counts.
- `roofline_loops` has the raw per-loop accounting: trip count, wall-clock duration, CPU time, thread count, sample count, load and store bytes, and operation counts by kind.
- `info.json` holds the full calibration under `cpu_info.roofline_calibration`: five samples per ceiling with their median, the thread count, affinity, kernel used, and the ridge point.
- `qemu-roofline.loops.json`, `qemu-roofline.cfg`, and `qemu-roofline.counts` are the engine's own output, kept for audit.

With the QEMU backend the recording also contains the `memory_*` tables described in [Memory analysis](scenario-mem.md).

## Compile with the instrumentation pass

To use the `compiler` backend, build the LLVM pass as described in [Install miniperf](install.md), then compile your program with it and link the trace stub:

```sh
clang -O3 -g source.c collector-core/stub/mperf_trace_stub.c -o workload \
  -Xclang -fpass-plugin=target/clang_plugin/lib/miniperf_plugin.so -ldl
mperf record -s roofline -o results/compiler -- ./workload
```

The result has one row per instrumented source loop instead of per binary loop.

## Threads

Every thread of the process is sampled and every thread's operations are counted, whatever created them: OpenMP, pthreads, Rayon, or a custom pool. A loop's `duration_ns` is the wall-clock time during which any of its threads was inside it, so the plotted GFLOP/s is the aggregate rate of the whole team and compares with the all-core ceilings. `cpu_time_ns` and `thread_count` say how much of that came from parallelism: a loop that four threads share has a CPU time near four times its duration. Threads that spin in the OpenMP runtime's barriers spend that time outside the executable, so it counts toward no loop.

The accounting run keeps its own thread identities. `roofline_loop_threads` lists each accounting thread's share of a loop's operations, and `accounting_threads` in `roofline_loops` counts them, so a loop whose native timing saw four threads but whose accounting run saw one is easy to spot. The DynamoRIO x86 fast path counts blocks on a shared counter and leaves both empty.

Kernels before Linux 6.12 cannot sample threads created after `exec`. On such a host the recording keeps only the main thread's timing and says so in its method warnings.

## Keep calibration and workload comparable

The calibration uses Rayon threads. The workload may use OpenMP or its own pool. Set both thread counts, pin the whole `mperf` command, and do not run other heavy work during calibration:

```sh
export RAYON_NUM_THREADS=4 OMP_NUM_THREADS=4 OMP_PLACES=cores OMP_PROC_BIND=close
taskset -c 0-3 mperf record -s roofline -o results/spmv -- ./spmv
```

Compare two recordings only when affinity, thread counts, problem size, precision, backend, and calibration conditions match. [Roofline analysis](../concepts/roofline.md) explains the model, and the [tutorial](../tutorials/roofline.md) records and reads a sparse matrix kernel end to end.
