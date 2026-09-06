# Top-down microarchitecture analysis

Top-down analysis explains why a CPU is slow by dividing its pipeline slots into four buckets, then subdividing the dominant bucket. miniperf implements it in `mperf stat --topdown` and in the `tma` recording scenario.

## Level 1

Every cycle, a core has a fixed number of issue slots, four on most cores. Each slot ends up in one of four places:

| Bucket | Meaning | Typical fix |
|---|---|---|
| **Retiring** | The slot completed useful work | Fewer instructions: better algorithm, vectorization |
| **Bad speculation** | The slot was wasted on a mispredicted branch or a machine clear | Predictable branches, branchless code |
| **Frontend bound** | No instruction was ready to issue | Smaller code, fewer indirect jumps, better layout |
| **Backend bound** | The slot was available but the backend could not accept work | Depends on level 2 |

A well-tuned compute kernel retires 50 % or more. Typical application code retires 20 to 30 %.

## Level 2 and below

The backend splits into **memory bound**, where the stall coincides with outstanding cache misses, and **core bound**, where execution units or dependency chains are saturated. Deeper levels attribute memory stalls to L1, L2, L3, DRAM, and TLB, and frontend stalls to fetch latency and fetch bandwidth.

How many levels you get depends on the CPU, because deeper levels need events the vendor may not expose. See the table in [Hotspots and top-down](../guide/scenario-hotspots.md).

## How miniperf computes it

There are three ways, and `mperf record` tells you which one it used.

**Dedicated hardware.** Intel cores from Ice Lake on have a `PERF_METRICS` register that counts the four level-1 buckets directly in slots. Sapphire Rapids adds the level-2 halves. Arm cores with pmuv3 `slots` events do the same at level 1. These are exact, occupy fixed counters, and leave the programmable counters free.

**Curated formulas.** For a CPU with an event table but no dedicated hardware, miniperf ships a scenario file with formulas over programmable events. Tiger Lake, Zen, Cortex-A720, A520, and the SpacemiT cores each have one. The events in a formula are opened together as one coherent group, so they are counted over the same cycles. Metrics are fractions of cycles rather than slots.

**Architectural fallback.** A CPU without a table gets three metrics from the portable events: `retiring = instructions / (4 * cycles)`, `fe_bound = stalled_cycles_frontend / cycles`, and `be_bound = stalled_cycles_backend / cycles`, assuming a retire width of four.

## Reading the numbers

`mperf stat --topdown -l 2` prints a tree. Level-2 values are fractions of their parent:

```
* be bound                      90.61%  Fraction of cycles backend was out of resources
    memory bound                  94.07%  Backend pressure coincident with outstanding L2 fills
    core bound                    -3.46%  Backend pressure not coincident with outstanding L2 fills
```

In a `tma` recording, `tma_summary` gives the same metrics as fractions over the run, and `tma` gives them per function. Look at the per-function numbers for the top three hotspots, not for the tail. A function with 0.1 % of cycles produces noisy fractions.

`tma_intervals` gives every metric per second. A program with phases, say a parse phase that is frontend bound and a compute phase that is memory bound, shows up there and averages out in the summary.

## Caveats of the counter-based path

Stall counters on many cores saturate rather than partition: a cycle stalled for two reasons counts in both. Consequences you will see:

- Level-1 buckets can sum to more than 100 %, and a value computed by subtraction can be slightly negative. Read negative values as zero.
- On AMD Zen, and on SpacemiT X100, an execution-bound loop with no cache misses stalls the frontend through backpressure, so `fe_bound` reads high on what is really a core-bound workload. The level-2 breakdown is the reliable signal there.
- When SMT siblings are busy, shared counters inflate. Profile on a quiet machine.

miniperf normalizes the SpacemiT buckets against the slots not accounted for by retiring and bad speculation, so its level-1 values sum to 1 on those cores. Elsewhere the raw fractions are reported.

For the theory, see Ahmad Yasin, "A Top-Down Method for Performance Analysis and Counters Architecture", ISPASS 2014.
