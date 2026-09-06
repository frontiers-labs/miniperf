# miniperf

miniperf is a sampling profiler for native applications on Linux and macOS,
across x86-64, AArch64, and RISC-V. It uses the same kernel interfaces as
Linux perf and adds scenario-driven analysis on top: a USE-method system
snapshot, top-down microarchitecture analysis, whole-process memory analysis
with working sets and miss-ratio curves, and Roofline plots against calibrated
machine ceilings. Every result records how it was measured and what the host
could not provide.

**User manual: <https://frontiers-labs.github.io/miniperf/>**

## Install

Download the archive for your platform from the
[releases page](https://github.com/frontiers-labs/miniperf/releases), unpack
it, and add its `bin` directory to `PATH`. Linux packages bundle QEMU and
DynamoRIO, so every scenario works out of the archive.

To build from source you need Rust 1.85 or newer and a C compiler:

```sh
cargo build --release -p mperf
target/release/mperf doctor
```

See [Install miniperf](https://frontiers-labs.github.io/miniperf/guide/install.html)
for the GUI, the collector library, and the QEMU and DynamoRIO bundles.

## Use

```sh
mperf stat -- ./workload                       # count events, like perf stat
mperf stat --topdown -l 2 -- ./workload        # top-down tree
mperf record -s tma -o out -- ./workload       # hotspots with stall attribution
mperf record -s snapshot -o out --duration 30s -p PID   # USE-method survey of a process tree
mperf record -s mem -o out -- ./workload       # working set, miss-ratio curve, DRAM traffic
mperf record -s roofline -o out -- ./kernel    # loops against calibrated ceilings
mperf show out                                 # terminal viewer
mperf-gui out                                  # desktop viewer
mperf query out 'SELECT func_name, total FROM hotspots ORDER BY total DESC LIMIT 10'
```

The manual has a chapter for each command and scenario, tutorials that walk
through complete investigations, and platform notes for Intel, AMD, Arm, and
the SpacemiT RISC-V boards.

## Architecture

The workspace has two tiers. `libprof` knows how to measure: PMU counting and
sampling, precise memory sampling (PEBS, IBS, SPE), host clocks and thermals,
memory-controller bandwidth, procfs, cgroup, and BPF telemetry, and post-hoc
DWARF unwinding. Everything it exposes compiles on every target, and a host
that cannot provide a source says so at runtime. `mperf` knows when and what to
measure: scenarios, passes, counter selection, storage, postprocessing, and
presentation. It contains no platform `cfg` outside Roofline calibration, and
CI fails if one appears.

Two rules follow. A new data source is one `Source` implementation in
`libprof` writing through `Sink`, plus one registration line in the scenario
that wants it. A new hardware facility is a new `Mechanism` behind an existing
`Feature`, never a new user-facing knob.

See [CONTRIBUTING.md](CONTRIBUTING.md) for the quality gates, the platform
code policy, the truth-fixture policy, and external dependency pins.

## Documentation

The manual lives in `docs/` as an mdBook and deploys to GitHub Pages from the
`Docs` workflow on every push to `master` that touches it. To preview locally:

```sh
mdbook serve docs
```

`docs/check-links.py` fails the build on a broken internal link.

## License

GPL-3.0. See [LICENSE](LICENSE).
