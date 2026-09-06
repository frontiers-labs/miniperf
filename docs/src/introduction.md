# Introduction

miniperf is a sampling profiler for native Linux and macOS programs. It answers the questions a performance engineer asks in order: where does the time go, why is the hardware stalling there, and how far is this code from what the machine can do.

The command-line tool is `mperf`. It has three jobs:

- **Count.** `mperf stat` counts hardware events for a command, like `perf stat`, and prints a top-down breakdown on request.
- **Record.** `mperf record` samples a program under one of four scenarios and writes a self-contained result directory.
- **Explain.** `mperf show` opens that directory in the terminal, `mperf-gui` opens it in a desktop viewer, and `mperf query` runs SQL against it.

## What makes it different from perf

miniperf uses the same kernel interface as Linux perf. The difference is what it does on top.

Each recording is a scenario with a fixed purpose. `snapshot` surveys a whole process tree with the USE method and ranks what to measure next. `tma` attributes pipeline stalls to functions with the top-down method. `mem` replays every memory reference to compute working sets and miss-ratio curves. `roofline` plots loops against calibrated compute and bandwidth ceilings. You pick the question, and miniperf picks the counters, the passes, and the analysis.

Every number carries its provenance. When a host cannot provide a measurement, the recording says so and says why, instead of writing a zero. The `capture_fidelity` table records which hardware mechanism was used and which better ones were rejected. Bandwidth figures are labeled as measured or modeled, and as process-scoped or system-scoped.

Results are plain Parquet files. `mperf query` opens them with an embedded DuckDB engine, and so does any Parquet reader you already use.

miniperf runs on x86-64, Arm, and RISC-V. It ships curated event tables for Intel Tiger Lake, AMD Zen, Arm Cortex-A720 and A520, SiFive U7, and SpacemiT X60, X100, and A100, and it works around the quirks of those boards.

## How to read this manual

- **Getting started** installs miniperf, fixes permissions, and records a first profile.
- **User guide** describes every command and scenario. Read the chapter for the tool you are about to use.
- **Concepts** explains the methods behind the numbers: how measurement quality is decided, what top-down levels mean, and how a Roofline is built.
- **Platform notes** cover per-CPU behavior you need to know on Intel, AMD, Arm, and RISC-V, and on macOS and Windows.
- **Tutorials** walk through complete investigations on real programs, with the output you should see.
- **Reference** lists every flag, environment variable, output file, and database table.

Commands in this manual assume `mperf` is on your `PATH`. If you built from source, substitute `target/release/mperf`.
