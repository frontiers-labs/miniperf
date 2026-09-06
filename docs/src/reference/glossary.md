# Glossary

**Arithmetic intensity.** Floating-point operations per byte moved. The horizontal axis of a Roofline plot.

**Backend bound.** Top-down bucket for pipeline slots the backend could not accept, because execution units were busy or a cache miss was outstanding.

**Baseline counters.** Ordinary programmable PMU counters. The mechanism of last resort for every feature, recorded as the `counter_only` rung.

**BPF.** Kernel programs miniperf loads through `bpftrace` in the `snapshot` scenario to attribute scheduler latency, block I/O, and TCP retransmits to a process tree.

**Capture fidelity.** The record of which hardware mechanism a recording used and which better ones were rejected, printed at record time and stored in the `capture_fidelity` table.

**cgroup.** A kernel control group. Launched snapshots run the workload in a private cgroup v2 so CPU and memory accounting for the tree is exact.

**Cold reference.** A memory access to a cache line the program had not touched before.

**DWARF unwinding.** Rebuilding a call stack from a copy of registers and stack memory using the compiler's unwind tables. Works without frame pointers.

**Feature.** A question asked of the hardware: precise memory, top-down, hardware call stacks, DRAM bandwidth. Answered by a mechanism.

**Frontend bound.** Top-down bucket for slots where no decoded instruction was ready.

**IBS.** Instruction Based Sampling, AMD's precise sampling facility. Detected but without a driver in miniperf.

**LBR.** Last Branch Record, Intel's hardware buffer of recent branches. In call-stack mode it yields call stacks without copying the user stack.

**LLC.** Last-level cache.

**Mechanism.** One hardware facility that answers a feature, such as PEBS or SPE. Each has a quality of exact or estimated.

**Miss-ratio curve.** The fraction of references that would miss an LRU cache, as a function of cache size. Computed from the reuse-distance histogram.

**MPKI.** Misses per thousand instructions.

**Multiplexing.** Time-sharing of hardware counters when more events are requested than counters exist. Shown as `Scaling` in `mperf stat` and `confidence` in `pmu_counters`.

**PEBS.** Processor Event-Based Sampling, Intel's precise sampling facility. Provides data addresses and latencies for memory samples.

**PMU.** Performance monitoring unit. The hardware counters in a core or in the uncore.

**PSI.** Pressure stall information, the kernel's measure of time tasks spent waiting for CPU, memory, or I/O.

**Retiring.** Top-down bucket for slots that completed useful work.

**Reuse distance.** The number of distinct cache lines touched between two accesses to the same line.

**Ridge point.** The arithmetic intensity at which the compute and bandwidth roofs meet.

**Rung.** The mechanism a recording used for its scenario's feature, from the ladder of candidates.

**Scenario.** A recording plan with a fixed question: `snapshot`, `tma`, `mem`, or `roofline`.

**Scope.** What a measurement counts: the profiled process tree, or the whole system during the run.

**Shim.** A shared library that forwards a runtime's own tool events, such as OpenMP or CUDA callbacks, into a recording.

**SPE.** Statistical Profiling Extension, Arm's precise sampling facility.

**TMA.** Top-down microarchitecture analysis. Also the name of the scenario that performs it.

**Uncore.** The parts of a CPU package outside the cores: memory controllers, interconnect, last-level cache. Uncore PMUs count system-wide.

**USE method.** For every resource, check utilization, saturation, and errors. The method behind the `snapshot` scenario.

**Working set.** The distinct bytes a program touches in a window of references.
