# Check your system with mperf doctor

`mperf doctor` reports what this host can measure and how to fix what it cannot. Run it once after installation and again whenever a recording warns about a missing collector.

```sh
mperf doctor
```

```
mperf doctor - AMD Zen or Zen+ architecture (x86_64), kernel 7.1.9-arch1-2

+----------------------------------+--------------------------------------------------------------------+----------+----------------------------------------------------------------------+
| Feature                          | Status                                                             | Severity | Action                                                               |
+==========================================================================================================================================================================================+
| perf_event_paranoid              | level 2: no kernel samples, no system-wide (uncore) events         | degraded | sudo sysctl -w kernel.perf_event_paranoid=0 (persist by adding ...) |
| hardware counters                | cpu-cycles opens                                                   | ok       | -                                                                    |
| kernel symbols (kptr_restrict)   | kernel addresses are readable                                      | ok       | -                                                                    |
| NMI watchdog                     | enabled - holds one hardware counter, shrinking sampling groups    | degraded | sudo sysctl -w kernel.nmi_watchdog=0 (persist by adding ...)        |
| bpftrace                         | not installed - snapshot loses scheduler, block-IO and TCP metrics | blocker  | install bpftrace (pacman -S bpftrace / apt install bpftrace / ...)   |
| eBPF collection (snapshot)       | not root - the BPF collector will be skipped                       | blocker  | run snapshot under sudo: sudo mperf record -s snapshot -o OUT -- CMD |
| precise sampling (AMD IBS)       | `ibs` CPUID flag absent - possibly disabled in BIOS                | degraded | check BIOS for an IBS / 'Instruction Based Sampling' toggle          |
| branch records (LBR call stacks) | core PMU advertises no branch-record depth in `caps/branches`      | info     | not available on this CPU                                            |
| uncore memory bandwidth          | system-wide events need CAP_PERFMON or perf_event_paranoid <= 0    | degraded | sudo sysctl -w kernel.perf_event_paranoid=0 (persist by adding ...) |
| objdump (disassembly)            | installed                                                          | ok       | -                                                                    |
+----------------------------------+--------------------------------------------------------------------+----------+----------------------------------------------------------------------+

2 blocker(s) found
```

Read the `Severity` column first.

- **blocker** means a scenario will refuse to run or lose a collector it depends on. The command exits with status 1 when any row is a blocker, so you can use it in a setup script.
- **degraded** means recordings work but with less fidelity: no kernel frames, one counter fewer, or an estimated mechanism instead of an exact one.
- **info** describes a hardware capability this CPU does not have. There is nothing to fix.
- **ok** needs no action.

The `Action` column is a command you can paste. Sysctl actions include the line to add under `/etc/sysctl.d` so the fix survives a reboot.

## The mechanism rows

The middle of the table lists the hardware mechanisms miniperf can use on this CPU family, and whether the kernel exposes them:

| Row | Used for |
|---|---|
| `precise sampling (Intel PEBS)`, `(AMD IBS)`, `(Arm SPE)` | Memory access samples with data address, latency, and cache level. Feeds the `mem_samples` tables. |
| `fixed topdown (PERF_METRICS)`, `topdown (Arm pmuv3 slots)` | Exact top-down level-1 metrics from dedicated hardware. |
| `branch records (LBR call stacks)` | Call stacks from the branch record buffer, which avoids copying the user stack on every sample. |
| `uncore memory bandwidth` | Memory-controller read and write bytes, for measured DRAM traffic. |
| `baseline counters` | The fallback for everything: ordinary programmable counters. |

A missing mechanism never blocks. miniperf resolves the best available one at record time and writes the outcome to the `capture_fidelity` table. See [How miniperf measures](../concepts/measurement.md).

## When bpftrace is a blocker

Only the `snapshot` scenario uses BPF. If you do not plan to run snapshots, ignore those two rows. If you do, install `bpftrace`, boot a kernel with `CONFIG_DEBUG_INFO_BTF`, and run the snapshot under `sudo`.
