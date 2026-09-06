# Roofline analysis

A Roofline plot relates a kernel's throughput to the amount of data it moves, on the same axes as the machine's limits. It answers two questions at once: is this loop bound by compute or by bandwidth, and how far is it from the ceiling that binds it.

## The model

For a loop with arithmetic intensity \\(I\\), in floating-point operations per byte moved, the attainable throughput is

$$
P(I) = \min(P_\text{compute},\; I \times B_\text{memory})
$$

\\(P_\text{compute}\\) is the machine's peak in GFLOP/s and \\(B_\text{memory}\\) its sustainable bandwidth in GB/s. The two lines cross at the ridge point

$$
I_\text{ridge} = P_\text{compute} / B_\text{memory}
$$

A loop to the left of the ridge cannot go faster without moving fewer bytes per operation. A loop to the right is limited by compute, and the fix is vectorization, instruction mix, or dependency chains.

## Where miniperf gets each number

**Ceilings.** Before every Roofline recording, `mperf` measures the host rather than reading a datasheet. It runs an FP64 FMA kernel and a streaming kernel five times each, with the same thread count and affinity the workload will get, and keeps the medians. The full samples are in `info.json` under `cpu_info.roofline_calibration`. A calibration whose samples spread widely means the machine was busy or the clock moved, and the recording should be repeated.

The calibration also measures bandwidth from each cache level, with working sets sized from the host's cache geometry, so the viewer can draw L2 and L3 roofs as well as DRAM.

**Throughput.** Operations come from the accounting run, which counts every executed floating-point instruction per loop, scalar and vector, single and double precision. For RISC-V vector code the count uses the runtime `vl`, `vstart`, SEW, and mask state of each instruction. Time comes from the native run: PMU samples from every thread are matched to the loop's address ranges, and the loop's duration is the wall-clock time during which at least one thread was inside it. Operations are summed over threads, so the plotted throughput is the aggregate rate of the whole team and the ceilings it is compared with are the all-core ones. The `roofline_loops` table keeps the CPU time and thread count beside the duration. A loop gets a plotted point only when the estimated 95 % error of that timing is at most 10 %; concurrent threads' samples overlap in time and do not count as extra evidence.

**Bytes.** This is the subtle part, and `traffic_source` in the `roofline` table tells you which kind you have:

- `architectural` bytes are what the loads and stores in the loop asked for, counted exactly by DynamoRIO or QEMU. They are an upper bound on traffic to any cache level.
- `dram-model` bytes come from QEMU feeding every reference through a set-associative LRU model of the host's shared last-level cache. The model counts misses and dirty writebacks. It is deterministic and process-specific, but it is a model.
- `uncore_measured` bytes come from the memory controller's own counters when the recording could open system-wide events. They are exact but system-scoped: every process on the machine is in them.

A point is only meaningful against the roof that matches its bytes. Architectural bytes against a DRAM roof understate intensity. The viewer labels the denominator on the plot for this reason, and `mperf record` warns when the two are not the same level.

## A worked example

Suppose the calibration reports 124 GFLOP/s FP64 and 19.8 GB/s, so the ridge is at 6.3 FLOP/byte.

A sparse matrix-vector kernel does two operations per nonzero and moves, per nonzero, an 8-byte value, a 4-byte index, and an 8-byte gathered vector element, plus a little for row offsets. Its intensity is about 0.1 FLOP/byte. It sits far left of the ridge. Its ceiling is \\(0.1 \times 19.8 = 2\\) GFLOP/s, and a measured 1.8 GFLOP/s is 90 % of the attainable. No amount of vectorization helps. Reordering the matrix for cache reuse, or compressing the indices, would.

A dense matrix multiply with blocking has intensity in the tens. Its ceiling is the 124 GFLOP/s roof, and if it runs at 30 GFLOP/s the loop is compute-bound and poorly vectorized or dependency-limited.

## Reading the plot

- Far below the sloped roof: poor locality, not enough memory-level parallelism, synchronization, or the loop range includes work outside the kernel.
- Far below the flat roof: dependency chains, scalar code where vector code was expected, frontend pressure, or too little parallelism.
- Above a roof: the bytes and the roof describe different levels, or the calibration ran under different conditions than the workload. Check `traffic_source` and the calibration samples.

Compare two recordings only when affinity, thread counts, problem size, precision, backend, and calibration conditions match. Keep the raw calibration with each recording rather than copying one machine-wide roof between experiments.

The original paper is Williams, Waterman, and Patterson, "Roofline: An Insightful Visual Performance Model for Multicore Architectures", CACM 2009.
