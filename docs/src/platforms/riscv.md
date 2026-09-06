# Linux on RISC-V

miniperf reads `marchid`, `mvendorid`, and `mimpid` to identify the core and loads a curated event table where it has one: SiFive U7 series, SpacemiT X60 (K1 and M1), SpacemiT X100 and A100 (K3). Release packages for `linux-riscv64` are built for `rv64gcv_zba_zbb` and contain the CLI, shims, QEMU, and DynamoRIO, but no GUI.

## SpacemiT X60 (K1, M1)

- The cycle and instruction counters do not raise overflow interrupts. Sampling runs on the `u_mode_cycles` event instead. Sampling on machine-mode instructions is unavailable.
- Cache references and misses map to the `l2_access` and `l2_miss` events.
- 164 events. No top-down scenario, so `--topdown` uses the architectural fallback.

## SpacemiT K3 (X100 and A100)

K3 has eight X100 application cores, `cpu0` to `cpu7`, and eight A100 AI cores, `cpu8` to `cpu15`. Both have event tables, and the same raw event code means different things on the two clusters, so the cluster must be identified correctly.

Linux does not schedule onto the A100 cores by itself. A helper such as the `ai` tool from [k3_ai](https://github.com/brucehoult/k3_ai) writes the caller's PID to `/proc/set_ai_thread`. Since `mperf` itself still runs on an X100 core, name the cluster explicitly when measuring on A100:

```sh
MINIPERF_CPU_FAMILY=a100 ai mperf stat --topdown -l 2 -- ./workload
```

Accepted values are `x60`, `x100`, and `a100`, with or without a `spacemit_` prefix.

**Counters.** The PMU exposes 16 counters. Two are dedicated to cycles and instructions, leaving 14 for programmable events, so the whole X100 top-down scenario fits in one group without multiplexing. Unlike X60, X100 does raise overflow interrupts on cycles, so sampling uses `cycles` directly.

**Event tables.** SpacemiT publishes no event table for X100. The names in the shipped table were derived by measuring each raw code against microbenchmarks with known instruction, branch, and cache behavior, and cross-checked against the K3 device tree. The set of valid codes comes from that device tree.

**Swapped stall counters.** The K3 device tree maps `STALLED_CYCLES_FRONTEND` to raw `0x03` and `STALLED_CYCLES_BACKEND` to raw `0x04`. Measurement shows these are reversed on X100: a dependent DRAM pointer chase, which can only stall the backend, puts 99.6 % of its cycles in `0x03`. miniperf uses the corrected assignment. `perf stat -e stalled-cycles-frontend` on this board reports backend stalls. The A100 cluster assigns the codes the other way round, matching the device tree, and miniperf handles each cluster accordingly.

**A100 specifics.** The A100 table is smaller. Most X60 codes read zero, and codes that work often differ from X100. A100 exposes no L2-miss or dTLB event, so its top-down stops at a frontend breakdown, and `be_bound` is reported without a memory and core split. Its `fp_vector_uop` counter is useful on its own: a VLEN-wide vector op counts as several micro-operations, so `vector_uops_per_inst` shows how much of the 1024-bit vector unit a loop uses.

**Top-down.** X100 has a three-level scenario, the deepest miniperf ships, with level 3 attributing memory stalls to L2, DRAM, and dTLB. The stall counters saturate rather than partition, so the level-1 buckets are normalized against the slots not consumed by retiring and bad speculation and always sum to 1. When an execution unit saturates without cache misses, backpressure also stalls the frontend and `fe_bound` reads high. The level-2 breakdown is the reliable signal.

The load-to-use latencies used as top-down constants, in cycles:

| | L1D | L2 | DRAM |
|---|---|---|---|
| X100 | 3 | 25 | 294 |
| A100 | 2 | 35 | 435 |

The X100 DRAM figure is from a hugepage-backed chase. With 4 KiB pages it is 376 cycles, and the 82-cycle difference is the page walk.

**Memory bandwidth.** K3 exposes no uncore PMU through perf and no RAPL counters. DDR bandwidth is available through the vendor `/dev/ddr_perf` character device, which answers an ioctl per AXI port with read and write bytes since the previous call. miniperf probes this device on any host without a perf memory controller and reports `ddr_perf` as the source. Three properties shape its use:

- The device is root-only. Without `sudo`, miniperf reports no memory-controller monitor rather than failing.
- The returned deltas are 32-bit bytes and saturate at 4 GiB, under a second of real traffic. miniperf polls every 50 ms on a background thread and accumulates into 64-bit counters.
- The previous value lives in the driver, one per port for the whole system, so two readers consume each other's deltas. Do not run another DDR bandwidth tool alongside a miniperf collection.

The counters are system-wide. The same `memcpy` benchmark measures 2081 MB read on an idle machine and 5917 MB right after a build, with page-cache writeback still draining. Read the figures as whole-system traffic during the run.

## SiFive U7

35 events from the U74 documentation. No top-down scenario. Sampling uses the architectural events.

## Roofline on RISC-V

RVV accounting in the QEMU plugin uses the executed instruction's runtime `vl`, `vstart`, SEW, and mask state, so changing `vlen` changes lane capacity without changing the accounting rule. Run Roofline recordings on the RISC-V host itself. Automatic mode refuses to present emulator time from an x86 host as RISC-V hardware performance. See [Roofline](../guide/scenario-roofline.md).
