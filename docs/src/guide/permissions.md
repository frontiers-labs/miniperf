# Set up permissions

miniperf opens hardware counters through the kernel's `perf_event_open` interface. What the kernel allows depends on one sysctl, `kernel.perf_event_paranoid`, and on whether the process is root or has `CAP_PERFMON`.

| `perf_event_paranoid` | What an unprivileged user gets |
|---|---|
| 3 or higher | Nothing. Every `mperf stat` and `mperf record` fails. |
| 2 | User-space counting and sampling of your own processes. No kernel samples, no system-wide events. This is the default on most distributions. |
| 1 | Adds kernel samples. |
| 0 or lower | Adds system-wide events, which miniperf needs for memory-controller bandwidth. |

To record kernel frames and DRAM bandwidth without root, set the level to 0 and persist it:

```sh
sudo sysctl -w kernel.perf_event_paranoid=0
echo 'kernel.perf_event_paranoid = 0' | sudo tee /etc/sysctl.d/99-mperf.conf
```

Two more sysctls affect quality:

- `kernel.kptr_restrict` hides kernel addresses when it is not 0. Kernel frames then stay unsymbolized. Set it to 0 to see kernel function names.
- `kernel.nmi_watchdog` holds one hardware counter while enabled. Disabling it gives sampling groups one more counter, which matters on CPUs with four programmable counters.

## What needs root

The `snapshot` scenario runs a `bpftrace` program to attribute scheduler latency, block I/O, and TCP retransmits to the process tree. Loading BPF programs requires root unless `kernel.unprivileged_bpf_disabled` is 0. Without root, the snapshot still records everything else and marks the BPF collector as `permission_denied`.

```sh
sudo mperf record -s snapshot -o out -- ./workload
```

On SoCs that expose DDR bandwidth through the vendor `/dev/ddr_perf` device, that device is root-only. On macOS, the kperf interface requires root for both `stat` and `record`.

## Sampling buffers on machines with many CPUs

`mperf record` opens one sampling ring per online CPU so the process is captured wherever it runs. Each ring is sized to the per-CPU allowance in `kernel.perf_event_mlock_kb`. The kernel charges all of those rings against one per-user budget, and when that budget is exhausted the last ring fails to map:

```
Error: failed to start source 'pmu_sampling' in pass 'tma'

Caused by:
    0: failed to mmap the sampling buffer for counter 'cycles' (528384 bytes): Operation not permitted (os error 1)
```

The kernel enforces this budget only while `kernel.perf_event_paranoid` is 0 or higher. Setting it to -1 removes the check, and so does running as root, which holds `CAP_IPC_LOCK`:

```sh
sudo sysctl -w kernel.perf_event_paranoid=-1
```

If you would rather keep the sysctl, pin the recording to the CPUs the workload will use. Fewer rings fit the budget, and the pinning also makes Roofline calibration comparable:

```sh
taskset -c 0-7 mperf record -s tma -o out -- ./workload
```

## Debug information

Stacks and source lines need debug information. Build with `-g`. Stripped binaries still resolve function names when their debug files are reachable through `.gnu_debuglink`, `/usr/lib/debug`, or a build-id cache. See [Call stacks and symbols](callstacks.md) for the lookup order and for enabling debuginfod.
