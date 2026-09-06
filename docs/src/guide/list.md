# Discover PMU events with mperf list

`mperf list` prints every counter miniperf can open on this host, one per line, as a name and a description:

```sh
mperf list
```

```
cycles - Number of CPU cycles
instructions - Number of instructions retired
branches - Branch instructions retired
branch_misses - Branch instruction missess
llc_misses - Last level cache misses
llc_references - Last level cache references
cpu_clock - A high-resolution per-CPU timer
page_faults - Number of page faults
cpu_migrations - Number of the times the process has migrated to a new CPU
context_switches - Number of context switches
l2_fill_pending.l2_fill_busy - Cycles with fill pending from L2. Total cycles spent with one or more fill requests in flight from L2.
ls_pref_instr_disp.prefetch_nta - Software Prefetch Instructions (PREFETCHNTA instruction) Dispatched.
...
```

The first ten are the portable events every supported CPU provides. The rest come from the event table for the detected CPU family. On an AMD Zen host that is 183 model-specific events. On a CPU without a curated table, only the portable events appear.

Use any of these names with `mperf stat -e`:

```sh
mperf list | grep -i tlb
mperf stat -e cycles,bp_l1_tlb_fetch_hit -- ./workload
```

On a heterogeneous CPU the list describes the primary cluster. Set `MINIPERF_CPU_FAMILY` to list another cluster's events. See [Linux on Arm](../platforms/arm.md) and [Linux on RISC-V](../platforms/riscv.md) for the values.
