# Measure a working set

In this tutorial we run the `mem` scenario on the matrix multiply from the [top-down tutorial](topdown.md), read its working-set table and miss-ratio curve, and decide what cache the program needs.

You need a Linux package of miniperf, or a source build with the QEMU or DynamoRIO bundle set up as described in [Install miniperf](../guide/install.md).

## Record

Build the original, naive version of `matmul.c` and record one multiplication:

```sh
gcc -O2 -g -o matmul matmul.c
taskset -c 0-3 mperf record -s mem -o mem-matmul -- ./matmul 1
```

```
Record profile with Mem scenario
Capture fidelity: mem at 'counter_only' (PEBS: core PMU advertises max_precise=0 — precise sampling needs 2)
Calibrating sustainable host memory bandwidth...
Host memory ceiling: 20.10 GB/s (4 Rayon threads)
Memory method: native timing with DynamoRIO binary accounting
Run 1: collecting native performance data for './matmul 1'
Precise memory sampling unavailable: PEBS: core PMU advertises max_precise=0 — precise sampling needs 2
checksum 11184768.000
Run 2: collecting DynamoRIO instruction and memory accounting for './matmul 1'
DynamoRIO dynamic CFG: 3 natural loops, 0 irreducible cycles; candidates saved to 'mem-matmul/qemu-roofline.loops.json'
Postprocessing...
```

Two things to notice. The recording says up front that this AMD host has no precise memory sampling, so the `mem_samples` tables will be empty. And the program runs twice: once natively for timing, once under DynamoRIO to see every memory reference.

## Read the headline

```sh
mperf query mem-matmul 'SELECT accessed_footprint_bytes, unique_lines, cold_fraction, modeled_dram_read_bytes, bandwidth_source, peak_rss_bytes FROM memory_summary'
```

```
┌──────────────────────────┬──────────────┬───────────────┬─────────────────────────┬──────────────────┬────────────────┐
│ accessed_footprint_bytes ┆ unique_lines ┆ cold_fraction ┆ modeled_dram_read_bytes ┆ bandwidth_source ┆ peak_rss_bytes │
╞══════════════════════════╪══════════════╪═══════════════╪═════════════════════════╪══════════════════╪════════════════╡
│                6,374,148 ┆      100,667 ┆      0.000746 ┆               6,442,688 ┆ process_modeled  ┆     11,972,608 │
└──────────────────────────┴──────────────┴───────────────┴─────────────────────────┴──────────────────┴────────────────┘
```

The program touched 6.4 MB, which is the three 2 MiB matrices. Only 0.07 % of references were cold, so almost all traffic is reuse. Modeled DRAM reads are 6.4 MB too: the modeled last-level cache held everything after the first touch. `bandwidth_source` says `process_modeled`, because this host would not open the memory-controller counters without root.

So the naive multiply is not DRAM-bound. Its problem is between L1 and L3.

## Read the miss-ratio curve

```sh
mperf query mem-matmul 'SELECT cache_bytes, miss_ratio FROM memory_miss_ratio WHERE cache_bytes IN (32768, 524288, 4194304, 16777216)'
```

```
┌─────────────┬────────────┐
│ cache_bytes ┆ miss_ratio │
╞═════════════╪════════════╡
│      32,768 ┆   0.561073 │
│     524,288 ┆   0.125530 │
│   4,194,304 ┆   0.001234 │
│  16,777,216 ┆   0.000746 │
└─────────────┴────────────┘
```

In a 32 KiB L1-sized cache, 56 % of references miss. In a 512 KiB L2-sized cache, 12.5 % miss. At 4 MiB the curve is flat at the cold fraction. The knee is between 512 KiB and 4 MiB: that is how much cache the naive loop order needs to stop missing, and it is more than an L2.

## Read the working set

```sh
mperf query mem-matmul 'SELECT * FROM memory_working_set'
```

```
┌───────────────────┬────────────────┬───────────┬───────────┐
│ window_references ┆   mean_bytes   ┆ p95_bytes ┆ max_bytes │
╞═══════════════════╪════════════════╪═══════════╪═══════════╡
│             1,024 ┆   36777.638285 ┆    36,928 ┆    36,928 │
│             4,096 ┆   61477.009802 ┆    69,760 ┆    73,856 │
│            16,384 ┆  160189.786139 ┆  168,256 ┆   262,272 │
│            65,536 ┆  554966.306796 ┆  566,336 ┆ 1,048,704 │
│         262,144 ┆ 2110904.792233 ┆ 2,109,504 ┆ 2,809,344 │
│         1,048,576 ┆ 2167498.914729 ┆ 2,134,080 ┆ 4,353,728 │
└───────────────────┴────────────────┴───────────┴───────────┘
```

Every 1024 references touch 36 KB of distinct memory, already more than a 32 KiB L1. The inner loop reads one 8-byte element from each of 512 different cache lines of `b`, so a single output element needs 32 KiB of `b` plus the row of `a`. That is the whole story of the slow version in one row.

## Check the stride histogram

```sh
mperf query mem-matmul 'SELECT stride_log2_lines, reference_count FROM memory_strides ORDER BY reference_count DESC LIMIT 3'
```

```
┌───────────────────┬─────────────────┐
│ stride_log2_lines ┆ reference_count │
╞═══════════════════╪═════════════════╡
│               -16 ┆      33,753,770 │
│                16 ┆      33,750,443 │
│               -15 ┆      25,181,974 │
│                15 ┆      25,116,141 │
└───────────────────┴─────────────────┘
```

The stride is the signed distance, in cache lines, between one reference and the next in program order. A value of 16 means 2^16 lines, or 4 MiB, and the sign alternates. That is the distance between the `a` and `b` arrays: the inner loop reads `a[i][k]`, then `b[k][j]`, then `a[i][k+1]`, jumping between the two every reference. A single stream with a small stride would show a peak near 0. Interleaved streams show the distance between the streams instead, which is worth knowing before you read this histogram as an access pattern within one array.

## Compare with the fixed version

Apply the loop interchange from the top-down tutorial, rebuild into a second binary, and record it into a second directory:

```sh
taskset -c 0-3 mperf record -s mem -o mem-ikj -- ./matmul_ikj 1
mperf query mem-ikj 'SELECT window_references, mean_bytes FROM memory_working_set WHERE window_references <= 4096'
```

The 1024-reference window shrinks to a few cache lines, because consecutive references now walk along rows. The miss ratio at 32 KiB drops accordingly. Same footprint, same DRAM traffic, entirely different locality. That is what the working-set table is for: distinguishing a program that needs more memory bandwidth from a program that needs a better access order.

## Where to go next

- With PEBS or SPE hardware, `alloc_site_memory` tells you which allocation the missing loads belong to, and `cacheline_contention` finds false sharing between threads.
- Run the recording as root, or with `perf_event_paranoid` at 0, to replace the modeled DRAM traffic with memory-controller measurements. `bandwidth_source` then reads `hardware_memory_controller`.
- The [Roofline tutorial](roofline.md) puts the achieved bandwidth on the same axes as the machine's limit.
