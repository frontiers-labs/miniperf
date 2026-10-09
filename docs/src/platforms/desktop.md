# macOS and Windows

## macOS

Recording on macOS uses Apple's kperf and KPC interfaces instead of `perf_event`. `mperf stat` and `mperf record` both work on Apple silicon, with these differences from Linux:

- KPC access needs root. Run `sudo mperf ...`. Without it the error reads `macOS KPC/kperf access was denied; try running this command with sudo`.
- There is no `/proc`, so a recording measures the launched process alone, not its children. Process-tree metrics, cgroups, and BPF do not exist.
- KPC has one global configuration, so only one profiler can run at a time.
- The `roofline` and `mem` scenarios need an accounting engine, and neither DynamoRIO nor `qemu-user` runs on macOS. The macOS package ships the collector library but no shims, QEMU, or DynamoRIO. Use a Linux host for those scenarios.

The macOS package ships the viewer as `mperf-gui.app`. Drag it into `/Applications`, or run `bin/mperf-gui <directory>` from the unpacked archive. The bundle exists because a bare binary gets no dock icon or menu bar and cannot be quit normally.

To build the GUI from source you need Xcode and its command-line tools. If Cargo cannot find the SDK, set `SDKROOT="$(xcrun --sdk macosx --show-sdk-path)"`. Metal shaders compile at startup, so the Metal toolchain component is not required.

## Windows

Windows builds include `mperf.exe` and `mperf-gui.exe`. `mperf stat` counts the
available process and ETW hardware events. `mperf record -s snapshot` gathers
process-tree CPU, memory, and I/O measurements alongside disk, network, CPU
frequency, and exposed ACPI thermal-zone metrics. Hardware profile sampling
uses ETW when the host grants access; a lower-fidelity CPU-time sampler remains
available when it does not. `mperf doctor` reports the available profile
sources and tools.

`mem` and `roofline` use the Windows DynamoRIO runner and the miniperf
`dr_roofline.dll` client. Set `MPERF_DYNAMORIO` and `MPERF_DR_CLIENT` to their
paths when building from source. Windows does not use QEMU. Hardware-dependent
top-down analysis requires the corresponding PMU events to be exposed by the
CPU, Windows, and the current security token; `mperf` reports unavailable
events instead of substituting estimates. Thermal-zone readings are supplied
only on machines whose firmware exposes them.

The `tma` scenario and model-specific `stat` events use Windows Performance
Recorder (`wpr.exe`) to capture a coherent counter vector on context switches.
Run them from an elevated terminal with PMU access. TMA recording also captures
timer instruction-pointer samples in the same ETW trace. It matches samples to
measured intervals by process, thread, CPU, and time, then apportions each
interval's counter vector among the sampled functions. The per-function values
in `tma` are statistical estimates; `tma_summary` and `tma_intervals` retain the
measured process totals. `tma_attribution` reports how many intervals and
samples contributed to function estimates. Intervals without a sample remain
in process totals and do not acquire an invented function.
