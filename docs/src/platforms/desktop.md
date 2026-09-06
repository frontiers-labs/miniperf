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

Windows packages contain `mperf-gui.exe` and nothing else. Recording is not supported. Copy a recording directory from a Linux or macOS machine and open it in the viewer.
