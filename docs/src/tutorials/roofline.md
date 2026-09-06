# Roofline analysis of a sparse matrix kernel

In this tutorial we record the Roofline of a sparse matrix-vector multiply, read the calibrated ceilings and the per-loop points, and work out which roof binds the kernel. Along the way we hit the two mistakes everyone makes with Roofline plots: too few samples, and bytes measured at a different level than the roof.

You need a Linux package of miniperf, or a source build with the QEMU or DynamoRIO bundle set up as in [Install miniperf](../guide/install.md), and the miniperf source tree for the example.

## Build the example

The repository ships a CRS sparse matrix-vector kernel with AVX2, AVX-512, and RVV variants:

```sh
make -C examples/spmv-crs build/spmv-avx2
```

It takes three optional arguments: rows, nonzeros per row, and repetitions. The defaults, 262144 rows, 16 nonzeros, and 40 repetitions, run for a fraction of a second. That is too short for Roofline, as we will see.

## Pin everything

Roofline compares the kernel with ceilings measured on the same machine. The calibration uses Rayon threads; the kernel uses OpenMP. Give both the same four cores and wrap `mperf` itself in `taskset`, so calibration, native run, and accounting run share the affinity:

```sh
export RAYON_NUM_THREADS=4 OMP_NUM_THREADS=4 OMP_PLACES=cores OMP_PROC_BIND=close
```

## Record

```sh
taskset -c 0-3 mperf record -s roofline -o roofline-avx2 -- examples/spmv-crs/build/spmv-avx2
```

```
Record profile with Roofline scenario
Capture fidelity: roofline at 'counter_only' (uncore bandwidth: system-wide events need CAP_PERFMON or perf_event_paranoid <= 0 (currently 2))
Calibrating host Roofline ceilings...
Host ceilings: 124.22 GFLOP/s FP64, 18.67 GB/s memory (4 Rayon threads)
Single-thread ceilings: 31.87 GFLOP/s FP64, 8.57 GB/s memory
Roofline method: native timing with DynamoRIO binary accounting
Warning: per-loop throughput is published only when native timing has at most 10% estimated 95% sampling error; lower-confidence loops retain accounting but are not plotted
Warning: native timing and DynamoRIO accounting come from separate executions
Run 1: collecting native performance data for 'examples/spmv-crs/build/spmv-avx2'
...
gflops=2.401419
arithmetic_intensity=0.095238087
...
Run 2: collecting DynamoRIO instruction and memory accounting for 'examples/spmv-crs/build/spmv-avx2'
DynamoRIO aggregate CFG: 8 natural loops, 1 irreducible cycles; candidates saved to 'roofline-avx2/qemu-roofline.loops.json'
Postprocessing...
```

Read the first lines before the numbers. The fidelity line says the memory-controller counters were not available, so the recording will not have measured DRAM bytes. The method line says `auto` picked DynamoRIO for accounting and native execution for timing. The two warnings apply to every recording with this method.

The kernel's own output reports 2.4 GFLOP/s at an intensity of 0.095 by its algorithmic byte model.

## The first mistake: too few samples

```sh
mperf query roofline-avx2 'SELECT function_name, line, vector_double_ops / 1e9 AS gflops, vector_double_ai AS ai, timing_quality FROM roofline ORDER BY line'
```

```
┌───────────────┬──────┬────────┬──────────┬──────────────────────┐
│ function_name ┆ line ┆ gflops ┆    ai    ┆    timing_quality    │
╞═══════════════╪══════╪════════╪══════════╪══════════════════════╡
│ main          ┆   82 ┆ NULL   ┆ 0.000000 ┆ insufficient-samples │
│ main          ┆   88 ┆ NULL   ┆ 0.000000 ┆ insufficient-samples │
...
```

Every loop is `insufficient-samples`. The run took 0.14 seconds, so at 1000 samples per second the kernel loop collected a hundred-odd samples, and the estimated timing error was above the 10 % gate. Loops that fail the gate keep their accounting and lose their throughput, so nothing gets plotted.

The fix is a longer run. Give the example 2000 repetitions and record again into a new directory:

```sh
rm -rf roofline-avx2
taskset -c 0-3 mperf record -s roofline -o roofline-avx2 -- examples/spmv-crs/build/spmv-avx2 262144 16 2000
```

## Read the loops

```sh
mperf query roofline-avx2 'SELECT function_name, line, vector_double_ops / 1e9 AS gflops, vector_double_ai AS ai, thread_count, timing_relative_error AS err, timing_quality FROM roofline ORDER BY vector_double_ops DESC NULLS LAST LIMIT 3'
```

```
┌────────────────┬──────┬──────────┬──────────┬──────────────┬──────────┬──────────────────────┐
│  function_name ┆ line ┆  gflops  ┆    ai    ┆ thread_count ┆    err   ┆    timing_quality    │
╞════════════════╪══════╪══════════╪══════════╪══════════════╪══════════╪══════════════════════╡
│ spmv._omp_fn.0 ┆  174 ┆ 2.056383 ┆ 0.125000 ┆            4 ┆ 0.036139 ┆ high-confidence      │
│ spmv._omp_fn.0 ┆  707 ┆ 1.475979 ┆ 0.142857 ┆            4 ┆ 0.036474 ┆ high-confidence      │
│ main           ┆   93 ┆ NULL     ┆ 0.000000 ┆            0 ┆ NULL     ┆ insufficient-samples │
└────────────────┴──────┴──────────┴──────────┴──────────────┴──────────┴──────────────────────┘
```

Now the two OpenMP kernel loops have about 8000 samples each, a 4 % timing error, and all four threads were seen inside them. Line 174 is the gather-and-FMA loop over the nonzeros of a row. Line 707 is the horizontal reduction and store. Together they account for the kernel's FP64 work.

Throughput is in GFLOP/s, summed over the four threads. Intensity is in FLOP per byte. The `traffic_source` column, not shown here, says `architectural`: the bytes are what the loads and stores asked for, counted by DynamoRIO. That word matters in the next step.

The raw accounting is in `roofline_loops`:

```sh
mperf query roofline-avx2 "SELECT line, trip_count, duration_ns, cpu_time_ns, timing_samples, vector_double_ops FROM roofline_loops WHERE function_name LIKE 'spmv%'"
```

```
┌──────┬───────────────┬───────────────┬────────────────┬────────────────┬───────────────────┐
│ line ┆   trip_count  ┆  duration_ns  ┆   cpu_time_ns  ┆ timing_samples ┆ vector_double_ops │
╞══════╪═══════════════╪═══════════════╪════════════════╪════════════════╪═══════════════════╡
│  174 ┆   525,066,420 ┆ 8,681,518,468 ┆ 23,387,669,696 ┆          7,924 ┆    17,852,530,688 │
│  707 ┆ 1,575,223,295 ┆ 8,537,916,452 ┆ 21,344,796,351 ┆          7,219 ┆    12,601,786,368 │
└──────┴───────────────┴───────────────┴────────────────┴────────────────┴───────────────────┘
```

525 million trips of the inner loop, 17.9 billion vector FP64 operations, and 8.68 seconds during which at least one thread was inside the loop. Divide and you get the 2.1 GFLOP/s above. `cpu_time_ns` adds up the four threads' own time in the loop, 23.4 seconds, so on average 2.7 of the four threads were inside it whenever any was; the rest of their time went to the OpenMP runtime between parallel regions. A loop that keeps all four threads busy the whole time would show a CPU time of four times its duration.

## Read the ceilings

```sh
jq '.cpu_info.roofline_calibration | {fp64_gflops, memory_gbytes_per_second, ridge_point_flops_per_byte, threads, cpu_affinity, single_thread: (.single_thread | {fp64_gflops, memory_gbytes_per_second})}' roofline-avx2/info.json
```

```json
{
  "fp64_gflops": 124.22,
  "memory_gbytes_per_second": 18.67,
  "ridge_point_flops_per_byte": 6.65,
  "threads": 4,
  "cpu_affinity": "0-3",
  "single_thread": {
    "fp64_gflops": 31.87,
    "memory_gbytes_per_second": 8.57
  }
}
```

Four cores sustain 124 GFLOP/s of FP64 FMA and 18.7 GB/s from DRAM. The ridge is at 6.7 FLOP/byte. Our loops sit at 0.125 and 0.143, fifty times to the left of the ridge. This kernel is bandwidth-bound by a wide margin, which is what everyone expects of SpMV. One core alone reaches 31.9 GFLOP/s and 8.6 GB/s; the viewer rates a loop that ran on one thread against those instead of the four-core roofs. The full calibration object also has the five samples behind each median and the L2 and L3 bandwidths.

## The second mistake: bytes and roof at different levels

Now the arithmetic. At intensity 0.125 the DRAM roof allows \\(0.125 \times 18.67 = 2.3\\) GFLOP/s. The loop at line 174 runs at 2.1 GFLOP/s, which reads as 88 % of the DRAM roof.

That reading is wrong, and the `traffic_source` column says why: the denominator and the roof describe different things. Architectural bytes count every load, including the gathers from the `x` vector, which is 2 MiB and lives in the L3 for the whole run. Those bytes never reach DRAM, so the intensity is far lower than the loop's DRAM intensity and the point can drift on either side of the DRAM roof depending on how much of its traffic the cache absorbs. Measured against the L3 bandwidth from the calibration, the roof that matches architectural bytes, the point sits well below its ceiling.

There are two honest ways to put this loop on a DRAM roofline:

- Record with memory-controller counters. Set `kernel.perf_event_paranoid` to 0 or run as root, and the fidelity line changes to the uncore rung, `traffic_source` becomes `uncore_measured`, and the bytes are what actually crossed to DRAM, for the whole system during the run.
- Record with the QEMU backend, `--roofline-backend qemu`. QEMU feeds every reference through a model of the host's last-level cache and reports `dram-model` bytes. The result is process-specific but modeled.

Either way, always read `traffic_source` before comparing a point with a roof.

## Interpret

With DRAM bytes in hand, the question becomes how close the kernel is to its bandwidth ceiling, and the example's own `effective_bandwidth_gbs` of 25 GB/s with the algorithmic model already tells us it is near or at the sustainable 19 GB/s once cache reuse of `x` is accounted for. Vectorizing harder will not help. Reducing bytes will: 32-bit values instead of 64, or a row ordering that keeps the gathered `x` entries in L2.

## Try the RISC-V variant

On a RISC-V host with a vector unit, the same command works on `build/spmv-rvv`, and the accounting uses each instruction's runtime `vl`. On an x86 host, `auto` refuses to run the RISC-V binary, because QEMU's speed is not RISC-V hardware speed. `--roofline-backend qemu` still produces operation counts for a code-path analysis, labeled `emulation-analysis`, with no throughput claim.

## In the viewer

`mperf-gui roofline-avx2` draws the loops against all the calibrated roofs and lets you double-click a point to open its source and disassembly. `mperf show roofline-avx2` lists the same loops in its Loops tab and the ceilings in Summary.
