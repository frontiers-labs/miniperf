# Diagnose a bottleneck with top-down

In this tutorial we take a program that is ten times slower than it should be, find the function responsible, learn why the hardware is stalling, fix it, and prove the fix. You need `mperf`, a C compiler, and about fifteen minutes.

## The program

Save this as `matmul.c`. It multiplies two 512 by 512 matrices in the textbook order.

```c
#include <stdio.h>
#include <stdlib.h>

#define N 512

static double a[N][N], b[N][N], c[N][N];

static void fill(void) {
    for (int i = 0; i < N; i++)
        for (int j = 0; j < N; j++) {
            a[i][j] = (double)(i + j) / N;
            b[i][j] = (double)(i - j) / N;
        }
}

static void multiply_naive(void) {
    for (int i = 0; i < N; i++)
        for (int j = 0; j < N; j++) {
            double sum = 0.0;
            for (int k = 0; k < N; k++)
                sum += a[i][k] * b[k][j];
            c[i][j] = sum;
        }
}

static double checksum(void) {
    double s = 0.0;
    for (int i = 0; i < N; i++)
        for (int j = 0; j < N; j++)
            s += c[i][j];
    return s;
}

int main(int argc, char **argv) {
    int reps = argc > 1 ? atoi(argv[1]) : 3;
    fill();
    for (int r = 0; r < reps; r++)
        multiply_naive();
    printf("checksum %.3f\n", checksum());
    return 0;
}
```

Build it with optimization and debug info, and time it:

```sh
gcc -O2 -g -o matmul matmul.c
time ./matmul 2
```

```
checksum 11184768.000

real	0m0.725s
```

Two multiplies of 512-cubed take 0.7 seconds. That is 268 million multiply-adds, so about 0.37 GFLOP/s on a core that can do tens.

## Step 1: count

First, we ask the hardware what it thinks:

```sh
mperf stat -- ./matmul 2
```

```
| cycles                  | 2,945,454,930 |                 |    1.00 | Number of CPU cycles                                 |
| instructions            | 1,078,666,632 | 0.37 inst/cycle |    1.00 | Number of instructions retired                       |
| llc_references          |   289,425,415 |                 |    2.00 | Last level cache references                          |
| llc_misses              |   113,606,753 | 105.32 MPKI     |    2.00 | Last level cache misses                              |
| stalled_cycles_backend  | 2,724,990,753 | 92.52%          |    2.00 | Number of cycles stalled due to backend bottlenecks  |
```

Three numbers are red. The core retires 0.37 instructions per cycle, it misses the last-level cache 105 times per thousand instructions, and the backend is stalled 92 % of the time. That is a memory problem, and a large one: the whole `b` matrix is 2 MiB, and every element of `c` walks a column of it, one cache line per element.

The top-down view says the same thing in its own vocabulary:

```sh
mperf stat --topdown -l 2 -- ./matmul 2
```

```
Top-down analysis (tma)
* be bound                      90.61%  Fraction of cycles backend was out of resources
    memory bound                  94.07%  Backend pressure coincident with outstanding L2 fills
    core bound                    -3.46%  Backend pressure not coincident with outstanding L2 fills
  retiring                       4.02%  Fraction of cycles useful work completed
  bad speculation                0.12%  Fraction of cycles lost to branch misprediction
  fe bound                       0.01%  Fraction of cycles Fetch/Decode not supplied
* dominant path at requested level
```

Backend bound, and within that, memory bound. The small negative `core bound` is a subtraction artifact from saturating stall counters. Read it as zero.

## Step 2: record

Counting told us what. Recording tells us where:

```sh
mperf record -s tma -o tma-naive -- ./matmul 2
```

```
Record profile with TMA scenario
Capture fidelity: tma at 'counter_only' (fixed topdown: core PMU exposes no `slots` + `topdown-*` events (PERF_METRICS is Icelake and newer))
checksum 11184768.000
Postprocessing...
```

If this fails with `failed to mmap the sampling buffer`, prefix the command with `taskset -c 0-7`. [Set up permissions](../guide/permissions.md) explains why.

Now the hotspots:

```sh
mperf query tma-naive 'SELECT func_name, total, cycles, instructions, ipc FROM tma ORDER BY total DESC LIMIT 3'
```

```
┌─────────────────────┬────────┬───────────────┬───────────────┬──────┐
│      func_name      ┆  total ┆     cycles    ┆  instructions ┆  ipc │
╞═════════════════════╪════════╪═══════════════╪═══════════════╪══════╡
│ multiply_naive      ┆ 99.99% ┆ 2,914,499,132 ┆ 1,068,428,237 ┆ 0.37 │
│ fill                ┆  0.01% ┆       268,423 ┆       685,382 ┆ 2.55 │
│ __tunable_get_val   ┆  0.00% ┆        83,513 ┆        88,341 ┆ 1.06 │
└─────────────────────┴────────┴───────────────┴───────────────┴──────┘
```

No surprise in a program this small, but notice `fill` for contrast. It touches the same matrices and runs at 2.55 IPC, because it walks them row by row.

Open the recording to see the loop itself:

```sh
mperf show tma-naive
```

Press `Tab` to reach Hotspots, then `Enter` on `multiply_naive`. The assembly view marks the instructions the samples landed on. Nearly all of them sit on the load of `b[k][j]`. Press `q` to leave.

## Step 3: fix

The inner loop strides through `b` by a whole row per iteration. Swapping the two inner loops makes the innermost loop walk `b` and `c` along a row:

```c
static void multiply_naive(void) {
    for (int i = 0; i < N; i++)
        for (int j = 0; j < N; j++)
            c[i][j] = 0.0;
    for (int i = 0; i < N; i++)
        for (int k = 0; k < N; k++) {
            double aik = a[i][k];
            for (int j = 0; j < N; j++)
                c[i][j] += aik * b[k][j];
        }
}
```

Rebuild and time it:

```sh
gcc -O2 -g -o matmul matmul.c
time ./matmul 2
```

```
checksum 11184768.000

real	0m0.067s
```

Same checksum, ten times faster.

## Step 4: prove it

A faster wall clock is the goal, but the counters tell us whether we fixed the problem we diagnosed or a different one:

```sh
mperf stat -- ./matmul 2
```

```
| cycles                  |   229,954,339 |                 |    1.00 | Number of CPU cycles                                 |
| instructions            |   946,774,369 | 4.12 inst/cycle |    1.00 | Number of instructions retired                       |
| llc_misses              |       769,876 | 0.81 MPKI       |    2.00 | Last level cache misses                              |
| stalled_cycles_backend  |    50,777,514 | 22.08%          |    2.00 | Number of cycles stalled due to backend bottlenecks  |
```

Instructions barely changed. Cycles fell by 13x. LLC misses fell from 105 to 0.8 per thousand instructions. IPC went from 0.37 to 4.12, which on this core is close to the retire width.

```sh
mperf stat --topdown -l 2 -- ./matmul 2
```

```
Top-down analysis (tma)
* retiring                      45.64%  Fraction of cycles useful work completed
  be bound                      22.55%  Fraction of cycles backend was out of resources
    memory bound                  77.95%  Backend pressure coincident with outstanding L2 fills
    core bound                   -55.40%  Backend pressure not coincident with outstanding L2 fills
  fe bound                       3.89%  Fraction of cycles Fetch/Decode not supplied
  bad speculation                3.04%  Fraction of cycles lost to branch misprediction
```

Retiring is now the dominant bucket. The remaining backend share is small enough that its level-2 split is noise; the large negative `core bound` says as much.

## What we learned

- `mperf stat` is enough to classify a problem. Red numbers in the `Info` column and the dominant top-down bucket name the category.
- `mperf record -s tma` and the `tma` table name the function. The assembly view names the instruction.
- The fix should move the counter that diagnosed the problem. Here LLC MPKI dropped by two orders of magnitude, which is how we know the diagnosis was right.

The remaining 22 % backend share is the next target. The [Roofline tutorial](roofline.md) shows how to ask how far a loop is from the machine's limit, and the [memory tutorial](memory.md) shows how to size its working set.
