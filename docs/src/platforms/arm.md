# Linux on Arm

miniperf identifies each core from `MIDR_EL1`, the implementer and part number. Cortex-A720 and Cortex-A520 ship with curated event tables of 150 and 141 events and a level-1 top-down scenario. Other cores get the architectural `perf_event` events.

## Heterogeneous systems

On a big.LITTLE system each cluster is a separate PMU with its own `perf_event` type. miniperf handles that throughout:

- `mperf stat` opens every counter on every cluster's PMU, so a task is counted correctly wherever the scheduler puts it. It prints one table per cluster plus a total summed across clusters. Per-cluster values are raw on-cluster counts, never extrapolated.
- `mperf record` samples on every cluster. Besides the merged `flamegraph_cycles.svg`, it writes one per cluster, for example `flamegraph_cycles_cortex_a720.svg`, and the same for instructions.
- Top-down metrics are produced per cluster, named `<metric>.<cluster>`, each restricted to that cluster's CPUs.

The first recognized core decides which event table names events and drives sampling. To target another cluster, set `MINIPERF_CPU_FAMILY` and pin the workload there:

```sh
MINIPERF_CPU_FAMILY=cortex_a520 taskset -c 1-4 mperf stat -- ./workload
```

Accepted values are `cortex_a520` and `cortex_a720`.

## Statistical Profiling Extension

Cores with SPE provide precise memory samples. miniperf opens the `arm_spe_*` PMU on every CPU in its mask, with a sample period of 4096 micro-operations or the hardware minimum. Because SPE samples operations rather than loads, its quality is recorded as estimated. The kernel needs `CONFIG_ARM_SPE_PMU`, and the firmware must expose SPE to the OS. `mperf doctor` reports both.

## Top-down from pmuv3 slots

Cores exposing the pmuv3 `slots`, `stall_slot_frontend`, `stall_slot_backend`, `op_retired`, and `op_spec` events get exact level-1 top-down from hardware. The mispredict term is moved from frontend to bad speculation so the four buckets sum to exactly one. Without those events, the Cortex tables provide level 1 from counter formulas.

## Memory bandwidth

The `arm_cmn` interconnect PMU provides memory-controller bandwidth where present. On SoCs without a perf-exposed memory controller, miniperf probes the vendor `/dev/ddr_perf` device described under [Linux on RISC-V](riscv.md).
